//! Kernel-owned identity page tables (issue #5, stage 2a).
//!
//! After ExitBootServices the kernel still runs on the firmware's page
//! tables. [`init`] replaces them with a static table set built in .bss:
//! one PML4, one PDPT and six page directories identity-map 0..6 GiB with
//! 2 MiB pages — every virtual address stays equal to its physical
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
//! Stage 2b (W^X) then splits every 2 MiB PDE overlapping the loaded image
//! into 4 KiB page tables and applies the PE section traits: exec sections
//! RX, writable sections RW|NX, read-only data and the PE headers R|NX,
//! gap/padding pages RW|NX; all remaining flat leaves (RAM, framebuffer)
//! become NX. EFER.NXE is enabled before any NX bit is set. A boot-time
//! self-check walks all leaves and rejects any writable+executable entry.
//! W^X is skipped as a whole (stage-2a map kept, no partial NX) when the
//! LoadedImage info is missing, the PE headers fail validation, or the
//! image allocation needs more page tables than the 16-entry pool holds.
//!
//! Entries are written directly through raw pointers (crate types only for
//! flags/frames) — no `Mapper`/`FrameAllocator`, so this runs before the
//! heap exists and never consumes a frame: the tables are the `.bss`
//! statics themselves.
//!
//! All memory the kernel actually touches sits below 4 GiB (OVMF relocates
//! the image into low RAM — boot logs show CR3 ≈ 0x1d4ba000, not the PE
//! preferred base); the frame allocator pool is bounded by
//! `memory::MAX_SUPPORTED_PHYS`. Interrupts are disabled throughout, so the
//! firmware map and this map are never mixed on the CPU.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use uefi::table::boot::{MemoryMap, MemoryType};
use x86_64::registers::control::{Cr3, Cr3Flags, Efer, EferFlags};
use x86_64::structures::paging::page_table::PageTableFlags;
use x86_64::structures::paging::{PageTable, PhysFrame};
use x86_64::PhysAddr;

use crate::kernel::{FramebufferInfo, ImageInfo};

/// One page directory covers 1 GiB with 2 MiB pages; six cover 0..6 GiB,
/// all under PML4[0]. Four would bound the RAM/frame-allocator window
/// (`memory::MAX_SUPPORTED_PHYS` = 4 GiB); the extra two are defensive
/// headroom for firmware that loads the kernel image at (or near) the PE
/// preferred base 0x140000000 = 5 GiB instead of relocating it low like
/// OVMF does. Stage 2b keeps this window and only splits the image-
/// overlapping PDEs into 4 KiB page tables (see `apply_wx`).
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

/// Spare 4 KiB page tables for the stage-2b W^X split: every 2 MiB PDE
/// overlapping the loaded image gets its own PT so code can be RX and data
/// NX at 4 KiB granularity. 16 PTs cover a 32 MiB image allocation — the
/// current image is ~13.4 MiB (0x1d465000..0x1e1be000 under QEMU/OVMF) and
/// needs 5 PTs. If a future image outgrows the pool, W^X is skipped entirely
/// (stage-2a map stays intact) rather than half-applied.
const PT_POOL_SIZE: usize = 16;
static mut PT_POOL: [AlignedPageTable; PT_POOL_SIZE] = [
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
    AlignedPageTable(PageTable::new()),
];
static PT_USED: AtomicUsize = AtomicUsize::new(0);

/// Image bounds W^X was applied to (0/0 = unavailable/skipped). Published
/// by [`init`] for diagnostics and controlled negative probes.
static WX_IMAGE_BASE: AtomicU64 = AtomicU64::new(0);
static WX_IMAGE_SIZE: AtomicU64 = AtomicU64::new(0);

/// (base, size) of the image allocation W^X was applied to (0/0 if W^X was
/// skipped). Controlled negative probes (write to a text page, execute a
/// data page) derive their addresses from this.
#[allow(dead_code)]
pub fn wx_image() -> (u64, u64) {
    (
        WX_IMAGE_BASE.load(Ordering::Relaxed),
        WX_IMAGE_SIZE.load(Ordering::Relaxed),
    )
}

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

/// PDE location and 2 MiB base for a physical address inside the mapped
/// 0..6 GiB window (`pde_for` results index `PDS` and must stay below
/// `PD_COUNT`).
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

// ---------------------------------------------------------------------------
// Stage 2b: NX/W^X with PE section parsing
// ---------------------------------------------------------------------------

/// Section table capacity. A Rust uefi image has ~5 sections; bail out of
/// W^X (never truncate the mapping) if a firmware hands us more.
const MAX_SECTIONS: usize = 16;
const PE_SECTION_HEADER_SIZE: u64 = 40;
const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;

/// One PE section's loaded range (virtual = physical, identity map) and the
/// 4 KiB PTE flags for its pages.
#[derive(Clone, Copy)]
struct WxSection {
    start: u64,
    end: u64,
    flags: PageTableFlags,
}

/// Everything needed to apply W^X, all validated up front: no state is
/// touched unless `parse_pe` returned a complete plan.
struct WxPlan {
    image_base: u64,
    image_end: u64,
    /// End of the header region (image start .. first section): R|NX.
    headers_end: u64,
    sections: [Option<WxSection>; MAX_SECTIONS],
}

fn read_u16(pa: u64) -> Option<u16> {
    // All reads are from our own image (present RAM); alignment is native.
    Some(unsafe { core::ptr::read_unaligned(pa as *const u16) })
}

fn read_u32(pa: u64) -> Option<u32> {
    Some(unsafe { core::ptr::read_unaligned(pa as *const u32) })
}

/// Parse the PE headers at `base` (bounded by `size`) into a W^X plan.
/// Returns None on any structural surprise — caller then skips W^X with a
/// warning instead of mapping half-known memory.
fn parse_pe(base: u64, size: u64) -> Option<WxPlan> {
    // Allocation bounds: 4 KiB-aligned base, non-empty, within the split
    // budget (each 2 MiB image PDE needs one pool PT).
    if base == 0 || base % 4096 != 0 || size == 0 || size > (PT_POOL_SIZE as u64) * HUGE_PAGE {
        return None;
    }
    let image_end = base.checked_add(size)?;
    let rd16 = |off: u64| -> Option<u16> {
        if off.checked_add(2)? <= size {
            read_u16(base + off)
        } else {
            None
        }
    };
    let rd32 = |off: u64| -> Option<u32> {
        if off.checked_add(4)? <= size {
            read_u32(base + off)
        } else {
            None
        }
    };

    if rd16(0)? != 0x5A4D {
        return None; // "MZ"
    }
    let pe_off = rd32(0x3C)? as u64;
    let coff = pe_off.checked_add(4)?;
    if coff + 20 > size || rd32(pe_off)? != 0x0000_4550 {
        return None; // "PE\0\0"
    }
    let nsec = rd16(coff + 2)? as usize; // NumberOfSections (u16 @ PE+6)
    let opt_size = rd16(coff + 16)? as u64; // SizeOfOptionalHeader (u16 @ PE+20)
    let sec_table = coff + 20 + opt_size;
    if nsec == 0 || nsec > MAX_SECTIONS || sec_table + (nsec as u64) * PE_SECTION_HEADER_SIZE > size
    {
        return None;
    }

    let mut sections: [Option<WxSection>; MAX_SECTIONS] = [None; MAX_SECTIONS];
    let mut count = 0;
    let mut headers_end = u64::MAX;
    for i in 0..nsec {
        let h = sec_table + (i as u64) * PE_SECTION_HEADER_SIZE;
        let vsize = rd32(h + 8)? as u64;
        let va = rd32(h + 12)? as u64;
        let chars = rd32(h + 36)?;
        if va % 4096 != 0 {
            return None;
        }
        if vsize == 0 {
            continue; // empty section: nothing to protect
        }
        let start = base.checked_add(va)?;
        let end = start.checked_add(vsize)?;
        if end > image_end {
            return None; // section claims memory outside the allocation
        }
        headers_end = headers_end.min(start);
        // Traits -> PTE flags. EXEC-only => RX (PRESENT, no writable, no NX);
        // WRITE => RW|NX; anything else => R|NX. A write+exec section (never
        // produced by this target) is treated as writable data, not
        // writable+executable: fail towards W^X, log loudly.
        let flags = if chars & IMAGE_SCN_MEM_EXECUTE != 0 && chars & IMAGE_SCN_MEM_WRITE == 0 {
            PageTableFlags::PRESENT
        } else if chars & IMAGE_SCN_MEM_WRITE != 0 {
            if chars & IMAGE_SCN_MEM_EXECUTE != 0 {
                crate::serial::print(
                    "[pt] WARNING: PE section is write+execute; mapping writable+NX\n",
                );
            }
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE
        } else {
            PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE
        };
        sections[count] = Some(WxSection { start, end, flags });
        count += 1;
    }
    if count == 0 {
        return None;
    }
    Some(WxPlan {
        image_base: base,
        image_end,
        headers_end,
        sections,
    })
}

/// 4 KiB PTE flags for one page of the image window.
fn page_flags(plan: &WxPlan, page: u64) -> PageTableFlags {
    if page < plan.image_base || page + 4096 > plan.image_end {
        // Outside the image allocation (padding of the last split PDE):
        // ordinary writable RAM, NX like the rest of the flat map.
        return PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    }
    if page < plan.headers_end {
        // PE headers: read-only, NX.
        return PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE;
    }
    for s in plan.sections.iter().flatten() {
        // Page-granular mapping: a page overlapping the section uses the
        // section's traits (sections are 4 KiB-aligned in practice, so this
        // only ever bites the last partial page, if at all).
        if page < s.end && page + 4096 > s.start {
            return s.flags;
        }
    }
    // Inside the allocation but outside any section (alignment gaps).
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE
}

/// Take one spare PT out of the pool.
fn alloc_pt() -> *mut PageTable {
    let i = PT_USED.fetch_add(1, Ordering::Relaxed);
    if i >= PT_POOL_SIZE {
        // Callers pre-count the requirement, so this is unreachable.
        panic!("PT pool exhausted despite pre-flight count");
    }
    unsafe { &raw mut PT_POOL[i].0 }
}

/// Apply W^X for `image` on top of the flat stage-2a map. Returns true when
/// W^X was fully applied; skips entirely — map left untouched, no NX
/// anywhere, caller must not publish the image bounds — when the image info
/// is missing, malformed, or outside the mapped window. Runs before the CR3
/// switch, with interrupts disabled.
fn apply_wx(image: ImageInfo) -> bool {
    if image.base == 0 || image.size == 0 {
        crate::serial::print(
            "[pt] W^X skipped: image info unavailable (LoadedImage open failed)\n",
        );
        return false;
    }
    let Some(plan) = parse_pe(image.base, image.size) else {
        crate::serial::print_fmt(format_args!(
            "[pt] W^X skipped: PE headers at {:#x} (size {:#x}) failed validation\n",
            image.base, image.size
        ));
        return false;
    };
    // The whole allocation must sit inside the mapped 0..6 GiB window:
    // pde_for() indexes PDS, and an allocation straddling the window edge
    // (e.g. a non-relocating loader near 5 GiB + 32 MiB) would run past it.
    let map_limit = (PD_COUNT as u64) << 30;
    if plan.image_base >= map_limit || plan.image_end > map_limit {
        crate::serial::print_fmt(format_args!(
            "[pt] W^X skipped: image {:#x}..{:#x} outside the mapped window (limit {:#x})\n",
            plan.image_base, plan.image_end, map_limit
        ));
        return false;
    }

    // Pre-flight: every 2 MiB PDE overlapping the image needs one PT,
    // except guard PDEs (left non-present, never split — the stack regions'
    // .bss guard halves must not be re-mapped).
    let guards = [
        crate::stacks::main_stack_base(),
        crate::stacks::double_fault_stack_base(),
    ];
    let first = pde_for(plan.image_base);
    let last = pde_for(plan.image_end - 1);
    let mut required = 0usize;
    let mut pdpt = first.0;
    loop {
        let lo = if pdpt == first.0 { first.1 } else { 0 };
        let hi = if pdpt == last.0 { last.1 } else { 511 };
        for pde_idx in lo..=hi {
            let pde_base = ((pdpt as u64) << 30) | ((pde_idx as u64) << 21);
            if !guards.contains(&pde_base) {
                required += 1;
            }
        }
        if pdpt == last.0 {
            break;
        }
        pdpt += 1;
    }
    if required > PT_POOL_SIZE {
        crate::serial::print_fmt(format_args!(
            "[pt] W^X skipped: image needs {} page tables, {} available\n",
            required, PT_POOL_SIZE
        ));
        return false;
    }

    crate::serial::print_fmt(format_args!(
        "[pt] W^X: image {:#x}..{:#x}, {} section(s), {} PT(s) required of {}\n",
        plan.image_base,
        plan.image_end,
        plan.sections.iter().flatten().count(),
        required,
        PT_POOL_SIZE
    ));

    // 1. NXE first — the NX bit is a reserved-bit fault while NXE=0, and no
    //    NX bit is set anywhere yet.
    unsafe { Efer::write(Efer::read() | EferFlags::NO_EXECUTE_ENABLE) };

    // 2. Split the image PDEs into 4 KiB PTs and fill per-section PTEs.
    //    Guard PDEs are not split: they stay non-present.
    let mut splits = 0usize;
    let mut pdpt = first.0;
    loop {
        let lo = if pdpt == first.0 { first.1 } else { 0 };
        let hi = if pdpt == last.0 { last.1 } else { 511 };
        for pde_idx in lo..=hi {
            let pde_base = ((pdpt as u64) << 30) | ((pde_idx as u64) << 21);
            if guards.contains(&pde_base) {
                continue;
            }
            let pt = alloc_pt();
            unsafe {
                (&mut *pd_ptr(pdpt))[pde_idx].set_frame(table_frame(pt), base_flags());
                for pte_idx in 0..512usize {
                    let page = pde_base + (pte_idx as u64) * 4096;
                    (&mut *pt)[pte_idx].set_addr(PhysAddr::new(page), page_flags(&plan, page));
                }
            }
            splits += 1;
        }
        if pdpt == last.0 {
            break;
        }
        pdpt += 1;
    }

    // 3. NX on every remaining flat 2 MiB leaf (RAM, framebuffer — PWT|PCD
    //    preserved). Split PDEs are left alone; their PTEs already carry
    //    the right NX bits.
    for i in 0..PDE_COUNT {
        let pa = (i as u64) * HUGE_PAGE;
        let (pdpt_idx, pde_idx, pde_base) = pde_for(pa);
        unsafe {
            let e = &(&mut *pd_ptr(pdpt_idx))[pde_idx];
            let f = e.flags();
            if f.contains(PageTableFlags::PRESENT) && f.contains(PageTableFlags::HUGE_PAGE) {
                set_pde(pde_base, f | PageTableFlags::NO_EXECUTE);
            }
        }
    }

    // 4. The asm behind tlb::flush_all declares no memory clobber either —
    //    keep the PTE stores on this side of it.
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    x86_64::instructions::tlb::flush_all();

    crate::serial::print_fmt(format_args!(
        "[pt] W^X: {} PDE(s) split, EFER.NXE on, flat leaves NX\n",
        splits
    ));
    wx_self_check();
    true
}

/// Per-leaf-class counts for the W^X self-check.
#[derive(Default)]
struct LeafCounts {
    code: u64,
    data: u64,
    ro: u64,
    bad: u64,
}

impl LeafCounts {
    fn count(&mut self, flags: PageTableFlags, pa: u64) {
        let writable = flags.contains(PageTableFlags::WRITABLE);
        let executable = !flags.contains(PageTableFlags::NO_EXECUTE);
        if writable && executable {
            self.bad += 1;
            crate::serial::print_fmt(format_args!(
                "[pt] W^X VIOLATION: writable+executable leaf at {:#x}\n",
                pa
            ));
        } else if executable {
            self.code += 1;
        } else if writable {
            self.data += 1;
        } else {
            self.ro += 1;
        }
    }
}

/// Walk every present leaf (huge PDEs and all PTEs of split PDEs): no leaf
/// may be writable AND executable.
fn wx_self_check() {
    let mut c = LeafCounts::default();
    for i in 0..PDE_COUNT {
        let pa = (i as u64) * HUGE_PAGE;
        let (pdpt_idx, pde_idx, _) = pde_for(pa);
        let e = unsafe { &(&mut *pd_ptr(pdpt_idx))[pde_idx] };
        let f = e.flags();
        if !f.contains(PageTableFlags::PRESENT) {
            continue;
        }
        if f.contains(PageTableFlags::HUGE_PAGE) {
            c.count(f, pa);
        } else {
            let pt = e.addr().as_u64();
            for j in 0..512usize {
                let entry_pa = pt + (j as u64) * 8;
                let entry = unsafe { core::ptr::read_volatile(entry_pa as *const u64) };
                let pf = PageTableFlags::from_bits_truncate(entry);
                if pf.contains(PageTableFlags::PRESENT) {
                    c.count(pf, entry_pa);
                }
            }
        }
    }
    if c.bad == 0 {
        crate::serial::print_fmt(format_args!(
            "[pt] W^X verified: {} exec, {} writable-NX, {} read-only-NX leaves\n",
            c.code, c.data, c.ro
        ));
    } else {
        crate::serial::print_fmt(format_args!(
            "[pt] W^X verification FAILED: {} violation(s)\n",
            c.bad
        ));
    }
}

/// Build the static identity map, install guard and framebuffer attributes,
/// apply W^X when possible, and load CR3. Runs in thread context with
/// interrupts disabled, before the heap exists.
pub fn init(fb_info: FramebufferInfo, memory_map: &MemoryMap<'static>, image: ImageInfo) {
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
    // Defensive bounds: an unusual GOP bar or size must never push pde_for
    // past the mapped window (it would index PDS out of range) — log and
    // limit/skip the UC marking instead.
    let map_limit = (PD_COUNT as u64) << 30;
    let fb_base = fb_info.base_address;
    let fb_len = fb_info.stride as u64 * fb_info.height as u64 * 4;
    let fb_start = fb_base & !(HUGE_PAGE - 1);
    let mut uc_count = 0;
    let mut uc_ram_overlap = false;
    let mut pa = fb_start;
    let fb_end = match fb_base.checked_add(fb_len) {
        // Fits comfortably inside the mapped window: mark as planned.
        Some(raw_end) if raw_end <= map_limit => {
            (raw_end + HUGE_PAGE - 1) & !(HUGE_PAGE - 1)
        }
        // Overlaps the window edge: mark only what is mapped.
        Some(raw_end) => {
            crate::serial::print_fmt(format_args!(
                "[pt] WARNING: framebuffer {:#x}..{:#x} extends past the mapped {:#x}; marking only the mapped part UC\n",
                fb_base, raw_end, map_limit
            ));
            map_limit
        }
        // Base+length overflows u64: mark nothing.
        None => {
            crate::serial::print_fmt(format_args!(
                "[pt] WARNING: framebuffer base {:#x} + len {:#x} overflows; skipping UC marking\n",
                fb_base, fb_len
            ));
            fb_start
        }
    };
    while pa < fb_end && pa < map_limit {
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
        "[pt] framebuffer base={:#x} len={:#x}: {} UC PDE(s), ram-overlap={}\n",
        fb_base,
        fb_len,
        uc_count,
        uc_ram_overlap
    ));
    crate::serial::print_fmt(format_args!(
        "[pt] built: {} PDEs, 2 guard PDEs clear, {} framebuffer PDEs UC\n",
        PDE_COUNT, uc_count
    ));

    // Stage 2b: EFER.NXE + W^X. Splits the image PDEs into 4 KiB PTs
    // (code RX, headers/readonly NX, data NX), marks every remaining flat
    // leaf NX, and verifies no leaf is writable+executable. Leaves the
    // stage-2a map untouched when the image info is missing or invalid;
    // the probe-visible bounds stay (0,0) whenever W^X did not apply.
    if apply_wx(image) {
        WX_IMAGE_BASE.store(image.base, Ordering::Relaxed);
        WX_IMAGE_SIZE.store(image.size, Ordering::Relaxed);
    }

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
