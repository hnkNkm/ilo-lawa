# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

ilo-lawa is a minimal UEFI-based operating system kernel written in Rust that transitions from UEFI boot services to a standalone kernel environment. It implements custom interrupt handling, a heap allocator, a framebuffer terminal, an interactive shell, and a RAM-based filesystem.

## Development Commands

```bash
# Build the kernel (all builds must run inside the Nix environment)
make build
# equivalent to: nix develop --impure -c cargo build --release

# Run in QEMU (Linux with KVM)
make run

# Run in QEMU (macOS / no KVM)
make run-no-kvm

# Clean build artifacts (also removes esp/ and ovmf/)
make clean

# Enter the Nix dev shell directly
nix develop --impure
```

Notes:
- `.cargo/config.toml` sets the default target to `x86_64-unknown-uefi` and enables `build-std` for `core`, `compiler_builtins`, and `alloc` — plain `cargo build` works inside the Nix shell without extra flags.
- `make run` copies the `.efi` to `esp/efi/boot/bootx64.efi` and boots QEMU with OVMF firmware (auto-downloaded to `ovmf/` on first run).
- There are no automated tests; verification is done by booting in QEMU and exercising the shell.
- Requires nightly Rust (provided by the Nix flake) for `#![feature(abi_x86_interrupt)]` and `#![feature(alloc_error_handler)]`.

## Architecture Overview

### Boot Process Flow
1. **UEFI Entry** (`src/main.rs`): Captures framebuffer info from GOP, then calls `ExitBootServices()`. After this point no UEFI services exist.
2. **Kernel Entry** (`src/kernel.rs:kernel_main`): Disables interrupts, then initializes in strict order: terminal → heap allocator → CPU features (FPU/SSE) → GDT → IDT → PIC → filesystem → enable interrupts → shell. Filesystem init must stay before interrupt enable so no IRQ can arrive while its locks are held.
3. **Runtime**: The keyboard ISR only enqueues raw scancodes (`keyboard::add_scancode` → `SCANCODE_QUEUE`). The main loop in `kernel_main` drains the queue in thread context (`try_pop_scancode` with interrupts disabled, `enable_and_hlt` when empty) and dispatches: scancode → ASCII → `shell::handle_input` → command execution.

### Critical Components

**Interrupt Architecture**:
- `src/gdt.rs`: GDT with TSS; IST stack for double faults (`DOUBLE_FAULT_IST_INDEX`)
- `src/interrupts.rs`: IDT with CPU exception handlers (breakpoint, page fault, GPF, invalid opcode, double fault) and hardware IRQs (timer, keyboard). Timer increments `pic::TICKS` for uptime.
- `src/pic.rs`: Legacy 8259 PIC (not APIC); handlers must send EOI

**Memory**:
- `src/allocator.rs`: Bump allocator over a 100 KiB static buffer, registered as `#[global_allocator]`. `dealloc` only resets when the allocation count hits zero — long-running allocation churn will exhaust the heap. `alloc` types (String, Vec, Box) are available throughout the kernel.
- No paging/virtual memory management beyond what UEFI leaves behind.

**Filesystem** (`src/fs/`):
- `mod.rs` defines a `FileSystem` trait and a global `FILESYSTEM: Mutex<Box<dyn FileSystem>>` behind free-function wrappers (`fs::read_file`, `fs::list_directory`, etc.) used by the shell.
- `memory.rs` is the active implementation: a RAM tree seeded with `/bin`, `/home`, `/etc`, and `README.txt`.
- `fat32.rs` exists (boot sector / dir entry structs) but is not wired in yet.

**Shell & I/O**:
- `src/shell.rs`: Command parsing, history (10 entries), built-ins (help, echo, version, reboot, ...) and filesystem commands (ls, cd, pwd, cat, mkdir, rm, write). Global singleton behind `spin::Mutex` — command handlers run in the kernel main loop (thread context), fed one character at a time from the scancode queue.
- `src/terminal.rs`: Framebuffer text terminal (8x8 font from `src/font.rs`, direct pixel writes). "Scrolling" currently just clears the screen.
- `src/keyboard.rs`: Scancode set 1 → ASCII, tracks Shift/Ctrl state.

### Module Dependencies & Conventions
- All interrupt handlers must use `extern "x86-interrupt"` ABI
- Framebuffer info must be captured before `ExitBootServices()`
- All static mutable state is guarded with `spin::Mutex` (+ `lazy_static`). The ONLY lock shared between ISR and thread context is `SCANCODE_QUEUE` in `src/keyboard.rs`; thread-side takers must disable interrupts around it. Never call into `TERMINAL`, `SHELL`, `FILESYSTEM`, or the allocator from an interrupt handler — that reintroduces the single-core deadlock this design removed (issue #2)
- Version string ("ilo-lawa OS v0.4.0") is duplicated in `kernel.rs` and `shell.rs`

## Task Tracking

Work is tracked as GitHub Issues on `hnkNkm/ilo-lawa` (`gh issue list`), labeled P0-critical through P3-low. The issue set came from a full gap analysis (2026-07); check it for known limitations before filing new ones.

## Common Issues and Solutions

- **Crashes after enabling interrupts**: Check GDT segment selectors, ensure all segments are properly initialized
- **Double faults**: Verify IST stack is properly allocated and TSS is loaded
- **Keyboard not working**: Ensure PIC mask enables IRQ1, check EOI is sent properly
- **Allocation failures / panics in shell commands**: The 100 KiB bump heap doesn't truly free; reduce allocations or grow `HEAP_SIZE` in `src/allocator.rs`
- **Build failures**: Always build inside `nix develop --impure`
