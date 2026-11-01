//! Ring-3 execution demo (issue #19 stage 1).
//!
//! A tiny hand-assembled user program lives in [`USER_BLOB`]. At boot
//! [`init`] copies it into a frame allocated from the frame allocator,
//! zeroes a stack frame, and asks [`crate::paging::map_user_pages`] to map
//! the pair at `USER_CODE_BASE` (8 GiB, RX|USER) and `USER_STACK_BASE`
//! (RW|NX|USER) — real frames, not the identity map, so the user virtual
//! address space is disjoint from kernel data.
//!
//! The `usermode` shell command enters ring 3 with `iretq`
//! ([`enter_user`]): it snapshots the kernel context (RIP/RCS/RFLAGS/RSP/
//! RSS) into [`RESUME_CTX`], pushes a ring-3 iretq frame, and lands in the
//! blob. The blob prints a string with `write(1, buf, len)` and leaves
//! with `exit(code)`, both via `int 0x80`:
//!
//! * `int 0x80` is an IDT gate with DPL 3 (src/interrupts.rs); its
//!   handler is the global-asm entry below. The CPU has already switched
//!   to the ring-3 transition stack (`TSS.privilege_stack_table[0]`,
//!   src/stacks.rs) and pushed `SS RSP RFLAGS CS RIP`. The asm saves the
//!   full user GPR frame, marshals the syscall arguments onto the
//!   Microsoft x64 ABI (`extern "C"` on the UEFI target: args in
//!   `RCX RDX R8 R9`, result in `RAX`), calls [`syscall_dispatch`], and
//!   `iretq`s the user GPRs + result back to ring 3.
//! * `write` returns the byte count to the user. The buffer is validated
//!   page by page with [`crate::paging::user_translate`] (present + USER
//!   at every level) and copied through the frame's physical address, so
//!   a kernel pointer is rejected (`-EFAULT`) and never dereferenced.
//! * `exit` stores the code in [`EXIT_CODE`] and jumps to
//!   `ilo_usermode_resume`, which points RSP at the saved kernel iretq
//!   frame and `iretq`s straight back into the shell's call — kernel
//!   RSP/RFLAGS/callee-saved state restored, prompt intact.
//!
//! Stage-1 limits (documented, not accidental): one synchronous user task
//! running in the kernel address space — no per-process address spaces, no
//! scheduling; a fault in user code (bad pointer, privilege instruction,
//! write to the RX page) hits the normal CPU exception handlers and
//! panics the kernel with the faulting frame.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Syscall numbers of the `int 0x80` gate (stage 1: write + exit only).
pub const SYS_WRITE: u64 = 1;
pub const SYS_EXIT: u64 = 2;

/// Negative errno-style return values handed back to user code in RAX.
const ERR_EBADF: u64 = (-9i64) as u64;
const ERR_EFAULT: u64 = (-14i64) as u64;
const ERR_EINVAL: u64 = (-22i64) as u64;
const ERR_ENOSYS: u64 = (-38i64) as u64;

/// Upper bound on one `write` length, so a bogus `len` cannot turn the
/// page-table walk into an unbounded loop.
const MAX_WRITE_LEN: u64 = 64 * 1024;

/// Exit code of the last user program (written by `exit`, read back by
/// [`enter_user`]). Single-threaded around the ring-3 excursion.
static EXIT_CODE: AtomicU64 = AtomicU64::new(0);

/// Physical address of the user code frame (identity-mapped, so the kernel
/// can (re)fill it directly).
static CODE_FRAME_PA: AtomicU64 = AtomicU64::new(0);

/// Entry RIP handed to `enter_user`: the blob sits at the start of the
/// user code page.
const USER_ENTRY_RIP: u64 = crate::paging::USER_CODE_BASE;
/// Initial user RSP: top of the user stack page (first push lands below it).
const USER_ENTRY_RSP: u64 = crate::paging::USER_STACK_BASE + 4096;

/// The user program, hand-assembled x86-64 (`ndisasm`/`as`-verified):
///
/// ```text
/// 0:  bf 01 00 00 00          mov edi, 1                 ; fd = 1
/// 5:  48 8d 35 18 00 00 00    lea rsi, [rip+0x18]        ; -> msg (36)
/// 12: ba 0e 00 00 00          mov edx, 14                ; len
/// 17: b8 01 00 00 00          mov eax, 1                 ; SYS_WRITE
/// 22: cd 80                   int 0x80
/// 24: bf 2a 00 00 00          mov edi, 42                ; exit code
/// 29: b8 02 00 00 00          mov eax, 2                 ; SYS_EXIT
/// 34: cd 80                   int 0x80
/// 36: "ring3 says hi\n"       ; 14 bytes
/// ```
static USER_BLOB: [u8; 50] = [
    0xbf, 0x01, 0x00, 0x00, 0x00, // mov edi, 1
    0x48, 0x8d, 0x35, 0x18, 0x00, 0x00, 0x00, // lea rsi, [rip+0x18]
    0xba, 0x0e, 0x00, 0x00, 0x00, // mov edx, 14
    0xb8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
    0xcd, 0x80, // int 0x80
    0xbf, 0x2a, 0x00, 0x00, 0x00, // mov edi, 42
    0xb8, 0x02, 0x00, 0x00, 0x00, // mov eax, 2
    0xcd, 0x80, // int 0x80
    b'r', b'i', b'n', b'g', b'3', b' ', b's', b'a', b'y', b's', b' ', b'h', b'i', b'\n',
];

/// Boot-time setup: allocate two frames above the low-memory floor, fill
/// them through the identity map, and link the user window into the page
/// tables. Runs with interrupts disabled, before the shell exists — later
/// `usermode` commands only enter and leave ring 3, so repeated runs are
/// safe. Returns false (and logs) when no usable frames are available;
/// the shell command then refuses to run instead of faulting.
pub fn init() -> bool {
    // The UEFI map may offer the historic low pages as free CONVENTIONAL
    // RAM (a fragmented map leaves small holes below 1 MiB); user memory
    // simply goes above them.
    const FRAME_FLOOR: u64 = 0x10_0000;
    let Some(code_pa) = crate::memory::reserve_frames_above(FRAME_FLOOR, 2) else {
        crate::serial::print("[user] WARNING: no free frames for the user region; ring 3 disabled\n");
        return false;
    };
    let stack_pa = code_pa + crate::memory::FRAME_SIZE;
    unsafe {
        core::ptr::copy_nonoverlapping(USER_BLOB.as_ptr(), code_pa as *mut u8, USER_BLOB.len());
        core::ptr::write_bytes(stack_pa as *mut u8, 0, crate::memory::FRAME_SIZE as usize);
    }
    CODE_FRAME_PA.store(code_pa, Ordering::Relaxed);
    match crate::paging::map_user_pages(code_pa, stack_pa) {
        Ok(()) => true,
        Err(e) => {
            crate::serial::print_fmt(format_args!("[user] WARNING: user mapping failed: {}\n", e));
            false
        }
    }
}

/// The `usermode` shell command: enter ring 3 synchronously, print what
/// the user program writes, resume the kernel when it exits.
pub fn run_demo() {
    if !crate::paging::user_region_mapped() {
        crate::terminal::print("usermode: user region not mapped (boot init failed)\n");
        return;
    }
    crate::terminal::print_fmt(format_args!(
        "usermode: entering ring 3 at {:#x} (int 0x80: write=1, exit=2)...\n",
        USER_ENTRY_RIP
    ));
    let code = unsafe { enter_user(USER_ENTRY_RIP, USER_ENTRY_RSP) };
    crate::terminal::print_fmt(format_args!(
        "usermode: user program exited with code {}; kernel context restored\n",
        code
    ));
}

/// Snapshot the kernel context, iretq into ring 3, and — once the user
/// program raises `exit` — resume here with the exit code in hand.
///
/// Interrupts stay enabled while user code runs (the saved kernel RFLAGS
/// carries IF=1), so timer/keyboard IRQs really do arrive from CPL 3 and
/// are serviced on the ring-3 transition stack. The `cli` below only
/// covers the handful of instructions that build the iretq frame.
unsafe fn enter_user(user_rip: u64, user_rsp: u64) -> u64 {
    let ctx = &raw mut RESUME_CTX;
    unsafe {
        (*ctx).user_cs = crate::gdt::user_code_selector().0;
        (*ctx).user_ss = crate::gdt::user_data_selector().0;
    }
    EXIT_CODE.store(0, Ordering::SeqCst);
    // Does not "return" through its own call: the asm iretqs to user mode;
    // the exit syscall's iretq lands on the label right after the frame
    // push, restoring this exact stack position, and `ret` comes back here.
    unsafe { ilo_usermode_enter(user_rip, user_rsp, ctx) };
    EXIT_CODE.load(Ordering::SeqCst)
}

/// Syscall dispatcher, called from the asm entry with the Microsoft x64
/// ABI (`extern "C"` on the UEFI target): `RCX` = syscall number, then
/// the raw user `RDI`, `RSI`, `RDX`. Returns the user-visible result in
/// RAX. Runs on the ring-3 transition stack with interrupts disabled
/// (interrupt gate), in the interrupted program's thread context, so
/// terminal/serial printing is allowed.
#[no_mangle]
extern "C" fn syscall_dispatch(number: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    match number {
        SYS_WRITE => sys_write(a1, a2, a3),
        SYS_EXIT => {
            EXIT_CODE.store(a1, Ordering::SeqCst);
            crate::terminal::print_fmt(format_args!(
                "[usermode] syscall: exit({}); resuming kernel context\n",
                a1
            ));
            // Does not return: iretq back into the kernel main loop.
            unsafe { ilo_usermode_resume(&raw const RESUME_CTX) }
        }
        _ => ERR_ENOSYS,
    }
}

/// write(fd, buf, len): validate the whole user buffer range (present +
/// USER at every paging level, via `paging::user_translate`), copy it
/// through the frame's physical address (kernel addresses never pass the
/// walk, so user code cannot make the kernel print kernel memory), and
/// print it on the mirrored console. Returns `len`, or a negative errno.
fn sys_write(fd: u64, buf: u64, len: u64) -> u64 {
    if fd != 1 {
        return ERR_EBADF;
    }
    if len == 0 {
        return 0;
    }
    if len > MAX_WRITE_LEN {
        return ERR_EINVAL;
    }
    let mut done = 0u64;
    let mut chunk = [0u8; 128];
    while done < len {
        let Some(va) = buf.checked_add(done) else {
            return ERR_EFAULT;
        };
        // Page-by-page validation + physical alias for the copy.
        let Some((page_pa, _flags)) = crate::paging::user_translate(va) else {
            return ERR_EFAULT;
        };
        let in_page = va & 0xFFF;
        let n = (len - done).min(4096 - in_page).min(chunk.len() as u64) as usize;
        unsafe {
            core::ptr::copy_nonoverlapping(
                (page_pa + in_page) as *const u8,
                chunk.as_mut_ptr(),
                n,
            );
        }
        match core::str::from_utf8(&chunk[..n]) {
            Ok(s) => crate::terminal::print(s),
            Err(_) => crate::terminal::print("<non-utf8>"),
        }
        done += n as u64;
    }
    done
}

// ---------------------------------------------------------------------------
// Global-asm entry points (Intel syntax).
//
// The UEFI target compiles Rust functions with the Microsoft x64 ABI, so
// the asm side obeys win64 rules: arguments in RCX/RDX/R8/R9, 32 bytes of
// shadow space below the call, RSP 16-byte aligned before `call`,
// RBX/RBP/RSI/RDI/R12-R15 callee-saved. The int-0x80 entry never assumes
// SysV registers anywhere.
// ---------------------------------------------------------------------------

global_asm!(
    // The uefi target's assembler defaults to Intel syntax (a `.intel_syntax`
    // directive here only triggers a bad_asm_style warning).
    ".text",

    // -- int 0x80 gate (DPL 3, src/interrupts.rs) -----------------------
    // Entered from ring 3 on the ring-3 transition stack; the CPU has
    // already pushed SS, RSP, RFLAGS, CS, RIP. Interrupts are off (gate).
    ".globl ilo_int80_entry",
    "ilo_int80_entry:",
    // Save the user GPR frame (user RSP lives in the CPU-pushed frame).
    "push rax",
    "push rcx",
    "push rdx",
    "push rbx",
    "push rbp",
    "push rsi",
    "push rdi",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    // Marshal onto win64: (number, a1, a2, a3) = (rax, rdi, rsi, rdx).
    // Reads before writes: rcx<-rax, r8<-rsi, r9<-rdx, then rdx<-rdi.
    "mov rcx, rax",
    "mov r8, rsi",
    "mov r9, rdx",
    "mov rdx, rdi",
    "sub rsp, 32",
    "call syscall_dispatch",
    "add rsp, 32",
    // RAX = syscall result for the user. Restore the user GPRs (RAX is
    // deliberately not restored: it carries the return value).
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rdi",
    "pop rsi",
    "pop rbp",
    "pop rbx",
    "pop rdx",
    "pop rcx",
    "add rsp, 8",
    "iretq", // back to ring 3: pops RIP/CS/RFLAGS/RSP/SS

    // -- enter ring 3 (called from Rust, win64) -------------------------
    // (user_rip: rcx, user_rsp: rdx, ctx: r8). Preserves the kernel's
    // Microsoft x64 nonvolatile state across the excursion — user code
    // (and anything it faults on) may trash all of it: RBX/RBP/RDI/RSI/
    // R12-R15 are pushed, and FXSAVE spills x87 + MXCSR + all XMM (CR4.
    // OSFXSR is on since cpu::init; 64-bit-mode FXSAVE covers XMM0-15).
    // The iretq frame into user mode is built only after the saves, so
    // ctx.kernel_rsp points at the spill area and the exit syscall's
    // iretq resumes exactly at `.ilo_kernel_resume` with the full
    // restore still pending. Must not be interrupted between cli and
    // iretq.
    ".globl ilo_usermode_enter",
    "ilo_usermode_enter:",
    "push rbx",
    "push rbp",
    "push rdi",
    "push rsi",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "sub rsp, 8", // re-align: entry RSP = 8 (mod 16), +64 +8 keeps the
    "sub rsp, 512", // ...FXSAVE area 16-byte aligned
    "fxsave [rsp]",
    // ResumeCtx: 0 user_cs, 2 user_ss, 8 kernel_rip, 16 kernel_cs,
    // 24 kernel_rflags, 32 kernel_rsp, 40 kernel_ss.
    "lea rax, [rip + .ilo_kernel_resume]",
    "mov [r8 + 8], rax",
    "xor eax, eax",
    "mov ax, cs",
    "mov [r8 + 16], rax",
    "pushfq",
    "pop rax",
    "mov [r8 + 24], rax",
    "mov [r8 + 32], rsp",
    "xor eax, eax",
    "mov ax, ss",
    "mov [r8 + 40], rax",
    "cli",
    "movzx eax, word ptr [r8 + 2]",
    "push rax", // user SS
    "push rdx", // user RSP
    "mov rax, 0x202",
    "push rax", // user RFLAGS: IF=1, reserved bit 1
    "movzx eax, word ptr [r8]",
    "push rax", // user CS (RPL 3)
    "push rcx", // user RIP
    "iretq",
    // Landed here only from `exit`'s iretq with the kernel context
    // restored (RSP = the value snapshotted above, at the FXSAVE area).
    // Restore the kernel's FP/SIMD control+register state and the
    // nonvolatile GPRs, then return to the Rust caller.
    ".ilo_kernel_resume:",
    "fxrstor [rsp]",
    "add rsp, 520",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rsi",
    "pop rdi",
    "pop rbp",
    "pop rbx",
    "ret",

    // -- resume kernel after exit(2) (called from the dispatcher) -------
    // (ctx: rcx) -> !. Point RSP at the saved kernel iretq frame; iretq
    // restores RIP/CS/RFLAGS/RSP/SS in one step.
    ".globl ilo_usermode_resume",
    "ilo_usermode_resume:",
    "lea rsp, [rcx + 8]",
    "iretq",
);


/// Saved kernel context for the `exit` return path, laid out so that
/// `lea rsp, [ctx+8]; iretq` pops it in hardware order. `user_cs`/
/// `user_ss` are the ring-3 selectors for the iretq frame *into* user
/// mode; `kernel_*` are written by the asm in `ilo_usermode_enter` and
/// consumed by `ilo_usermode_resume`. Only touched on the synchronous
/// kernel thread while executing `usermode`.
#[repr(C)]
struct ResumeCtx {
    user_cs: u16,
    user_ss: u16,
    _pad: u32,
    kernel_rip: u64,
    kernel_cs: u64,
    kernel_rflags: u64,
    kernel_rsp: u64,
    kernel_ss: u64,
}

static mut RESUME_CTX: ResumeCtx = ResumeCtx {
    user_cs: 0,
    user_ss: 0,
    _pad: 0,
    kernel_rip: 0,
    kernel_cs: 0,
    kernel_rflags: 0,
    kernel_rsp: 0,
    kernel_ss: 0,
};

/// Address of the asm `int 0x80` handler, for the IDT gate.
pub fn int80_entry_addr() -> u64 {
    ilo_int80_entry as *const () as u64
}

extern "C" {
    fn ilo_usermode_enter(user_rip: u64, user_rsp: u64, ctx: *mut ResumeCtx);
    fn ilo_usermode_resume(ctx: *const ResumeCtx) -> !;
}

extern "C" {
    fn ilo_int80_entry();
}
