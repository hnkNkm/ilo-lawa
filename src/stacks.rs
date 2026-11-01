//! Kernel-owned stack regions (issue #5, stage 2).
//!
//! After ExitBootServices the kernel still runs on the firmware's stack —
//! memory the firmware may treat as reclaimable and that the kernel does not
//! own. [`switch_to_kernel_stack`] moves execution onto a static,
//! kernel-owned stack before any frame distribution happens, so the kernel
//! never keeps state on memory it does not control.
//!
//! Each region is a 4 MiB window aligned to 2 MiB, carved out of an
//! over-sized static, which puts both halves on exactly one 2 MiB
//! page-directory entry each once the kernel owns the page tables:
//!
//! ```text
//! base                base+2MiB           base+4MiB
//! +-------------------+-------------------+
//! | guard (2 MiB)     | usable stack      |
//! | never touched;    | grows down from   |
//! | its PDE becomes   | top = base+4MiB   |
//! | non-present in    |                   |
//! | the paging stage  |                   |
//! +-------------------+-------------------+
//! ```
//!
//! The guard half makes stack overflow hit an unmapped page (and the panic
//! handler) instead of silently corrupting neighbouring statics. Because the
//! aligned window is computed to sit entirely inside the region's own static
//! storage (checked by [`verify_layout`]), the linker can never place a
//! foreign object inside a future guard PDE.
//!
//! Why runtime alignment? The obvious `#[repr(C, align(2*1024*1024))]`
//! static crashes this toolchain: LLVM 22's WinCOFF object writer faults
//! (SIGILL in `WinCOFFWriter::defineSection`) when asked to emit a section
//! with 2 MiB alignment, so the storage array uses the plain 4 KiB alignment
//! the rest of the kernel's big statics already use, and the 2 MiB region
//! window is derived with two ALU ops. All statics live in .bss: no EFI image
//! file cost, and the guard half is never written, so it costs no RAM.

use core::arch::asm;

/// Size of the aligned region window: 2 MiB guard + 2 MiB usable.
const REGION_WINDOW: u64 = 4 * 1024 * 1024;
/// Alignment of the region window; both halves land on one 2 MiB PDE each.
const REGION_ALIGN: u64 = 2 * 1024 * 1024;
/// Page size the storage arrays are aligned to.
const STORAGE_ALIGN: u64 = 4096;
/// Storage bytes needed so a 2 MiB-aligned 4 MiB window always fits,
/// whatever 4 KiB-aligned address the linker picks for the array.
const REGION_STORAGE: usize = (REGION_WINDOW + REGION_ALIGN - STORAGE_ALIGN) as usize;

#[repr(C, align(4096))]
struct StackRegion {
    _bytes: [u8; REGION_STORAGE],
}

/// Kernel main stack. `kernel_main` switches onto it before any other
/// subsystem initialises.
static mut MAIN_STACK: StackRegion = StackRegion {
    _bytes: [0; REGION_STORAGE],
};

/// Double-fault IST stack. Its region is reserved here so the layout is
/// fixed once; gdt.rs starts using it in the paging commit (stage 2a).
static mut DOUBLE_FAULT_STACK: StackRegion = StackRegion {
    _bytes: [0; REGION_STORAGE],
};

/// 64 KiB kernel stack for every CPU transition from ring 3 to ring 0
/// (issue #19 stage 1). The `int 0x80` syscall gate lands here, and so do
/// timer/keyboard and all other IRQs that fire while user code runs: for
/// any interrupt or exception taken at CPL 3 the CPU loads this stack top
/// from `TSS.privilege_stack_table[0]` (src/gdt.rs) and pushes the
/// interrupt frame on it. Ring-0 execution never switches to it — at
/// CPL 0 the CPU keeps the interrupted stack. The stack is 16-byte
/// aligned as the x86-64 interrupt-frame contract expects, has no guard
/// half (unlike the regions above: it is only ever touched by short
/// handlers, never grown by nested kernel code) and is re-entered from
/// the top on every transition, so per-interrupt scratch is transient.
const SYSCALL_STACK_BYTES: usize = 64 * 1024;

#[repr(C, align(4096))]
struct SyscallStack {
    _bytes: [u8; SYSCALL_STACK_BYTES],
}

static mut SYSCALL_STACK: SyscallStack = SyscallStack {
    _bytes: [0; SYSCALL_STACK_BYTES],
};

/// One-time check that both region windows sit inside their storage arrays.
/// Cheap (a few comparisons, no allocation); called from
/// [`switch_to_kernel_stack`] for the main stack, and available for the
/// paging commit to re-check before wiring the IST stack.
pub fn verify_layout() {
    for name_array in [
        ("main", &raw const MAIN_STACK),
        ("double-fault", &raw const DOUBLE_FAULT_STACK),
    ] {
        let (base, top) = region_window(name_array.1);
        assert!(
            base >= name_array.1 as u64 && top <= storage_end(name_array.1),
            "{} stack region window [{:#x},{:#x}) escapes its storage",
            name_array.0,
            base,
            top
        );
    }
}

fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

/// Aligned window `[base, top)` inside one region's storage.
fn region_window(array: *const StackRegion) -> (u64, u64) {
    let base = align_up(array as u64, REGION_ALIGN);
    (base, base + REGION_WINDOW)
}

fn storage_end(array: *const StackRegion) -> u64 {
    array as u64 + REGION_STORAGE as u64
}

/// Base (lowest address) of the main stack region; the guard half starts
/// here.
pub fn main_stack_base() -> u64 {
    region_window(&raw const MAIN_STACK).0
}

/// Initial RSP for the kernel main stack (top of the region). 16-byte
/// aligned: the base is 2 MiB-aligned and the window is 4 MiB.
pub fn main_stack_top() -> u64 {
    main_stack_base() + REGION_WINDOW
}

/// Base (lowest address) of the double-fault IST stack region.
#[allow(dead_code)]
pub fn double_fault_stack_base() -> u64 {
    region_window(&raw const DOUBLE_FAULT_STACK).0
}

/// Initial RSP for the double-fault IST stack (top of the region).
#[allow(dead_code)]
pub fn double_fault_stack_top() -> u64 {
    region_window(&raw const DOUBLE_FAULT_STACK).1
}

/// Top of the ring-3 transition stack (`TSS.privilege_stack_table[0]`).
/// Page-aligned storage plus a 64 KiB size keeps this 16-byte aligned.
pub fn syscall_stack_top() -> u64 {
    &raw const SYSCALL_STACK as u64 + SYSCALL_STACK_BYTES as u64
}

/// Current RSP. Diagnostics only (serial boot-log checkpoints).
pub fn current_stack_pointer() -> u64 {
    let rsp: u64;
    unsafe { asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack)) };
    rsp
}

/// Move RSP to the top of the kernel main stack and continue in
/// `kernel_main_owned`. The firmware stack is abandoned; the `call` pushes a
/// fresh return address onto the new stack, so the callee is a normal Rust
/// function and the alignment contract (RSP 16-byte aligned at the `call`)
/// holds by construction: the top is 2 MiB-aligned + 4 MiB.
///
/// Callers must have parked everything the continuation needs (see
/// `kernel::BOOT_ARGS`) and must never return here (`-> !`).
pub unsafe fn switch_to_kernel_stack() -> ! {
    verify_layout();
    let top = main_stack_top();
    asm!(
        "mov rsp, {top}",
        "xor ebp, ebp",
        "call {continue_main}",
        top = in(reg) top,
        continue_main = sym crate::kernel::kernel_main_owned,
        options(noreturn)
    )
}
