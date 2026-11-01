//! Kernel-owned identity page tables (issue #5, stage 2a).
//!
//! After ExitBootServices the kernel still runs on the firmware's page
//! tables. [`init`] replaces them with a static table set built in .bss:
//! one PML4, one PDPT and four page directories identity-map the low 4 GiB
//! with 2 MiB pages — every virtual address stays equal to its physical
//! address, so the framebuffer, the retained UEFI memory map, the firmware's
//! leftovers and the kernel image itself all keep working across the CR3
//! switch.
//!
//! Special entries on top of the flat RAM map:
//!   * the low half (2 MiB guard) of each stack region from
//!     `stacks::main_stack_base()` / `stacks::double_fault_stack_base()` is
//!     left non-present, so stack overflow hits a page fault instead of
//!     silently corrupting neighbouring memory;
//!   * the 2 MiB PDEs covering the GOP framebuffer get WRITE_THROUGH |
//!     NO_CACHE (PWT|PCD), so MMIO writes bypass the caches; a defensive
//!     check warns if such a PDE overlaps CONVENTIONAL RAM.
//!
//! Entries are written directly through raw pointers (crate types only for
//! flags/frames) — no `Mapper`/`FrameAllocator`, so this runs before the
//! heap exists and never consumes a frame: the tables are the `.bss`
//! statics themselves.
//!
//! All memory touched here is below 4 GiB, matching
//! `memory::MAX_SUPPORTED_PHYS`. Interrupts are disabled throughout, so the
//! firmware map and this map are never mixed on the CPU.

use core::sync::atomic::{AtomicU64, Ordering};
use uefi::table::boot::{MemoryMap, MemoryType};
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::page_table::PageTableFlags;
use x86_64::structures::paging::{PageTable, PhysFrame};
use x86_64::PhysAddr;

use crate::kernel::FramebufferInfo;

/// One page directory covers 1 GiB with 2 MiB pages. Six cover 0..6 GiB:
/// four for the RAM window (`memory::MAX_SUPPORTED_PHYS`), plus PDPT[5]
/// for 5..6 GiB because the kernel image itself is loaded at the PE
/// preferred base 0x140000000 (5 GiB) — .text/.data/.bss, including these
/// page tables, the stack regions and the heap, live there. All of it hangs
/// under PML4[0]. Stage 2b will narrow the mapping to the exact image
/// bounds via LoadedImage; the flat 1 GiB window is correct for every
/// layout the uefi target produces (default image base).
const PD_COUNT: usize = 6;
const HUGE_PAGE: u64 = 1 << 21;
const PDE_COUNT: usize = 512 * PD_COUNT;

#[repr(C, align(4096))]
struct AlignedPageTable(PageTable);

static mut PML4: AlignedPageTable = AlignedPageTable(PageTable::new());
static mut PDPT: AlignedPageTable = AlignedPageTable(PageTable::new());
static mut PDS: [AlignedPageTable; PD_COUNT] = [
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
];

/// CR3 the firmware left behind, stashed by [`init`] for diagnostics.
static OLD_CR3: AtomicU64 = AtomicU64::new(0);

/// The CR3 value the firmware was using when the kernel took over.
pub fn old_cr3() -> u64 {
    OLD_CR3.load(Ordering::Relaxed)
}

/// Address 8 bytes into the main stack's guard half. A write here must
/// fault once the guard PDE is active — used by controlled guard tests
/// (see the bisect notes in the paging commits).
#[allow(dead_code)]
pub fn guard_probe_addr() -> u64 {
    crate::stacks::main_stack_base() + 8
}

fn base_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE
}

/// Flags for the flat 2 MiB identity PDEs (the PS/HUGE_PAGE bit is what
/// makes the CPU treat the entry as a 2 MiB page instead of a pointer to a
/// 4 KiB page table).
fn leaf_flags() -> PageTableFlags {
    base_flags() | PageTableFlags::HUGE_PAGE
}

/// Raw pointer to the page directory serving 1 GiB window `pdpt_idx`.
fn pd_ptr(pdpt_idx: usize) -> *mut PageTable {
    unsafe { &raw mut PDS[pdpt_idx].0 }
}

/// PDE location and 2 MiB base for a physical address below 4 GiB.
fn pde_for(pa: u64) -> (usize, usize, u64) {
    let pdpt_idx = (pa >> 30) as usize & 0x1FF;
    let pde_idx = (pa >> 21) as usize & 0x1FF;
    let pde_base = pa & !(HUGE_PAGE - 1);
    (pdpt_idx, pde_idx, pde_base)
}

/// Write one 2 MiB PDE for physical address `pa` with `flags`.
unsafe fn set_pde(pa: u64, flags: PageTableFlags) {
    let (pdpt_idx, pde_idx, pde_base) = pde_for(pa);
    // Explicit `&mut`: this nightly rejects *implicit* autoref through a
    // raw pointer (the IndexMut sugar would hide a reference creation).
    unsafe { (&mut *pd_ptr(pdpt_idx))[pde_idx].set_addr(PhysAddr::new(pde_base), flags) }
}

fn table_frame(t: *mut PageTable) -> PhysFrame {
    PhysFrame::containing_address(PhysAddr::new(t as u64))
}

/// Build the static identity map, install guard and framebuffer attributes,
/// load CR3. Runs in thread context with interrupts disabled, before the
/// heap exists.
pub fn init(fb_info: FramebufferInfo, memory_map: &MemoryMap<'static>) {
    // The guard math below relies on both stack windows sitting inside
    // their `.bss` storage.
    crate::stacks::verify_layout();
    crate::serial::print("[pt] building identity map (2 MiB pages, 0..6 GiB)\n");

    unsafe {
        // Flat map: every 2 MiB PDE below 6 GiB present+writable, and the
        // upper levels linked to the directories (all under PML4[0]:
        // PDPT[i] covers the i-th GiB window, i in 0..6).
        (&mut *(&raw mut PML4.0))[0].set_frame(table_frame(&raw mut PDPT.0), base_flags());
        for i in 0..PD_COUNT {
            (&mut *(&raw mut PDPT.0))[i].set_frame(table_frame(pd_ptr(i)), base_flags());
            for j in 0..512 {
                let pa = ((i as u64) << 30) | ((j as u64) << 21);
                set_pde(pa, leaf_flags());
            }
        }
    }

    // Guards: leave the low-half PDE of each stack region non-present.
    // The entries are already zero from `PageTable::new()`, but clear them
    // explicitly so the invariant does not depend on initialisation order.
    for (name, base) in [
        ("main", crate::stacks::main_stack_base()),
        ("double-fault", crate::stacks::double_fault_stack_base()),
    ] {
        let (pdpt_idx, pde_idx, pde_base) = pde_for(base);
        unsafe { (&mut *pd_ptr(pdpt_idx))[pde_idx].set_unused() };
        crate::serial::print_fmt(format_args!(
            "[pt] guard PDE: {} stack {:#x} -> PD{}[{}] not present\n",
            name, pde_base, pdpt_idx, pde_idx
        ));
    }

    // Framebuffer: PWT|PCD on every 2 MiB PDE it touches (MMIO bypasses the
    // caches). Warn if such a PDE overlaps CONVENTIONAL RAM — under QEMU's
    // layout it never does; the warning means the UC flags are eating RAM.
    let fb_base = fb_info.base_address;
    let fb_len = fb_info.stride as u64 * fb_info.height as u64 * 4;
    let fb_start = fb_base & !(HUGE_PAGE - 1);
    let fb_end = (fb_base + fb_len + HUGE_PAGE - 1) & !(HUGE_PAGE - 1);
    let mut uc_count = 0;
    let mut uc_ram_overlap = false;
    let mut pa = fb_start;
    while pa < fb_end {
        unsafe {
            set_pde(pa, leaf_flags() | PageTableFlags::WRITE_THROUGH | PageTableFlags::NO_CACHE)
        };
        uc_count += 1;
        for desc in memory_map.entries() {
            let start = desc.phys_start;
            let end = start + desc.page_count * 4096;
            if desc.ty == MemoryType::CONVENTIONAL && start < pa + HUGE_PAGE && pa < end {
                uc_ram_overlap = true;
                crate::serial::print_fmt(format_args!(
                    "[pt] WARNING: framebuffer UC PDE {:#x} overlaps CONVENTIONAL RAM {:#x}..{:#x}\n",
                    pa, start, end
                ));
            }
        }
        pa += HUGE_PAGE;
    }
    crate::serial::print_fmt(format_args!(
        "[pt] framebuffer {:#x}..{:#x}: {} UC PDE(s), ram-overlap={}\n",
        fb_base,
        fb_base + fb_len,
        uc_count,
        uc_ram_overlap
    ));
    crate::serial::print_fmt(format_args!(
        "[pt] built: {} PDEs, 2 guard PDEs clear, {} framebuffer PDEs UC\n",
        PDE_COUNT, uc_count
    ));

    // Switch CR3. `mov cr3` architecturally invalidates every non-global TLB
    // entry (PGE is off, GLOBAL unused), so no explicit flush is needed.
    let p4 = unsafe { table_frame(&raw mut PML4.0) };
    OLD_CR3.store(Cr3::read().0.start_address().as_u64(), Ordering::Relaxed);
    crate::serial::print_fmt(format_args!(
        "[pt] loading CR3 {:#x} (firmware was {:#x})\n",
        p4.start_address().as_u64(),
        old_cr3()
    ));
    // Compiler-only barrier: Cr3::write's `mov cr3` asm declares no memory
    // clobber, so LLVM may sink the non-volatile page-table stores past it
    // (observed: hardware walked zeroed tables -> #PF on the first fetch).
    // CPU-side ordering needs nothing extra — mov cr3 is serializing.
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    unsafe { Cr3::write(p4, Cr3Flags::empty()) };
    crate::serial::print("[pt] CR3 loaded\n");
}
