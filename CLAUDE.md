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

# Build with the test harness and run it headless in QEMU (exit 33 = pass)
make test

# Clean build artifacts (also removes esp/ and ovmf/)
make clean

# Enter the Nix dev shell directly
nix develop --impure
```

Notes:
- `.cargo/config.toml` sets the default target to `x86_64-unknown-uefi` and enables `build-std` for `core`, `compiler_builtins`, and `alloc` — plain `cargo build` works inside the Nix shell without extra flags.
- `make run` copies the `.efi` to `esp/efi/boot/bootx64.efi` and boots QEMU with OVMF firmware (auto-downloaded to `ovmf/` on first run).
- `make test` is the automated test path: it builds with `--features test-harness` into a separate `target-test/` dir (so `make build`'s cache is untouched), boots headless QEMU with `-device isa-debug-exit,iobase=0xf4,iosize=0x04`, and maps the exit code — 33 (harness wrote 0x10) is pass; anything else (35 = harness reported failures, 124 = timeout, panic/hang) is failure. The full serial log lands in `test-serial.log` (gitignored).
- Without the feature there are no automated tests; verification is done by booting in QEMU and exercising the shell. The console is mirrored to COM1, so the serial output (`-serial stdio` in the Makefile, or `-serial file:...` headless) contains the full boot log and shell session as plain text.
- Requires nightly Rust (provided by the Nix flake) for `#![feature(abi_x86_interrupt)]` and `#![feature(alloc_error_handler)]`.

## Architecture Overview

### Boot Process Flow
1. **UEFI Entry** (`src/main.rs`): Captures framebuffer info from GOP, then calls `ExitBootServices()`. After this point no UEFI services exist.
2. **Kernel Entry** (`src/kernel.rs:kernel_main`): Disables interrupts, then initializes in strict order: terminal → heap allocator → CPU features (FPU/SSE) → GDT → IDT → PIC → memory map + frame allocator → filesystem → PIT timer (100 Hz) → enable interrupts → shell. Filesystem init must stay before interrupt enable so no IRQ can arrive while its locks are held; the PIT is programmed after the PIC (IRQ0 routed) and before interrupts are enabled. Test builds only (feature `test-harness`): after the filesystem and PIT are up, `test_harness::run()` executes the kernel self-tests and exits QEMU via isa-debug-exit before interrupts are ever enabled — the shell and main loop are not reached.
3. **Runtime**: The keyboard ISR only enqueues raw scancodes (`keyboard::add_scancode` → `SCANCODE_QUEUE`). The main loop in `kernel_main` drains the queue in thread context (`try_pop_scancode` with interrupts disabled, `enable_and_hlt` when empty) and dispatches: scancode → `keyboard::KeyEvent` → `shell::handle_key_event` → line editing / command execution.

### Critical Components

**Interrupt Architecture**:
- `src/gdt.rs`: GDT with TSS; IST stack for double faults (`DOUBLE_FAULT_IST_INDEX`)
- `src/interrupts.rs`: IDT with CPU exception handlers (breakpoint, page fault, GPF, invalid opcode, double fault) and hardware IRQs (timer, keyboard). Timer ISR calls `time::tick()` (atomic increment) and sends EOI — nothing else.
- `src/pic.rs`: Legacy 8259 PIC (not APIC); handlers must send EOI

**Timer & Time** (`src/time.rs`):
- PIT 8253/8254 channel 0 programmed to 100 Hz (base 1,193,182 Hz, divisor 11,932, command 0x36 → port 0x43, divisor → port 0x40) by `time::init()`; tick = 10 ms.
- `static TICKS: AtomicU64` (`Ordering::Relaxed`) incremented by the timer ISR via `time::tick()`; readers: `ticks()`, `uptime_seconds()`.
- `sleep_ticks(n)` / `sleep_ms(ms)`: hlt-loop until the target tick — thread context with interrupts enabled ONLY (never from an ISR or with interrupts disabled). 10 ms resolution; `sleep_ms` rounds up to whole ticks, `ms == 0` returns immediately.
- Consumers: shell `uptime` command (days + HH:MM:SS) and `sleep <ms>` command.

**Memory** (`src/allocator.rs`, `src/memory.rs`):
- `src/main.rs` retains the memory map returned by `ExitBootServices()` (it is `'static`) and passes it to `kernel_main`.
- `src/memory.rs` (issue #5 stage 1): copies a compact summary of the firmware map into `MEMORY_SUMMARY` (`spin::Mutex` over a fixed 64-region static array; `phys_start`, `page_count`, memory type) and prints total usable RAM, region count, and free-frame count in the boot log. Regions starting above 4 GiB are ignored (and counted in the log).
- `src/memory.rs` also holds the physical frame allocator: `FRAME_ALLOCATOR: spin::Mutex<BitmapFrameAllocator>` over a static 128 KiB bitmap (1 bit per 4 KiB frame, low 4 GiB = 1,048,576 frames). The static is zero-filled (.bss — no EFI file cost); init marks every bit used, then clears the frames of CONVENTIONAL regions below 4 GiB. `allocate_frame() -> Option<u64>` returns identity-mapped physical addresses (no paging layer yet); `free_frame(addr)` clears the bit (callers must only free frames from `allocate_frame`; double frees are ignored).
- `src/allocator.rs`: heap = `linked_list_allocator::LockedHeap` (frees and coalesces) over a static 1 MiB page-aligned area in .bss, registered as `#[global_allocator]`; `init_heap()` hands it the area exactly once before any allocation. `alloc` types (String, Vec, Box) are available throughout the kernel and deallocation reclaims memory — shell churn no longer leaks.
- Both `MEMORY_SUMMARY` and `FRAME_ALLOCATOR` are thread-context-only spin locks (single-core kernel; never take them from an ISR — if that ever changes, disable interrupts around the lock like `SCANCODE_QUEUE` takers do).
- The kernel owns its paging from `paging::init` (issue #5 stage 2): a static identity map in `.bss` (PML4 + PDPT + 6 PDs, 2 MiB pages, 0..6 GiB) replaces the firmware tables before any other init; CR3 is loaded there and the firmware CR3 stashed (`paging::old_cr3`). The kernel also switches off the firmware stack onto a kernel-owned guarded stack (`src/stacks.rs`) before frame distribution.
- W^X (stage 2b): EFER.NXE + PE section parsing. Every 2 MiB PDE overlapping the loaded image is split into 4 KiB PTs from a 16-entry pool — exec sections RX, writable sections RW|NX, read-only data + PE headers R|NX, in-image gaps RW|NX; all remaining flat leaves (RAM, framebuffer) are NX. A boot-time self-check rejects writable+executable leaves (`[pt] W^X verified` in the boot log). W^X is skipped as a whole if LoadedImage info is missing, PE headers fail validation, or the image needs more than 16 PTs (>32 MiB) — never half-applied.
- Limits: kernel RAM and page tables are all in the low-4-GiB LOADER_CODE region; the frame allocator pool stays <4 GiB (`memory::MAX_SUPPORTED_PHYS`); the map covers 0..6 GiB as headroom for non-relocating loaders. Stack regions have a 2 MiB unmapped guard half each; deliberate faults are exercised by controlled probes (`paging::guard_probe_addr`, `paging::wx_image`), not at runtime.

**Filesystem** (`src/fs/`):
- `mod.rs` defines a `FileSystem` trait and a global `FILESYSTEM: Mutex<Box<dyn FileSystem>>` behind free-function wrappers (`fs::read_file`, `fs::list_directory`, etc.) used by the shell.
- `memory.rs` is the active implementation: a RAM tree seeded with `/bin`, `/home`, `/etc`, and `README.txt`.
- `fat32.rs` is a forward-looking FAT32 driver for issue #14 and is not wired in yet; it carries the repo's only targeted lint allowance (`#![allow(dead_code)]` with an issue reference at the top of the file) until then. Don't blanket-allow dead code elsewhere.

**Shell & I/O**:
- `src/serial.rs`: Polled 16550 COM1 driver. ALL terminal output is mirrored to serial (serial lock taken before, never nested with, the terminal lock), so QEMU's `-serial stdio`/`-serial file:` captures the boot log and entire shell session — the primary debugging channel. Thread context + panic path only; never from ISRs.
- `src/shell.rs`: Command parsing, history (10 entries), built-ins (help, echo, version, uptime, sleep, reboot, ...) and filesystem commands (ls, cd, pwd, cat, mkdir, rm, write). Global singleton behind `spin::Mutex` — command handlers run in the kernel main loop (thread context), fed `keyboard::KeyEvent`s one at a time from the scancode queue.
- Shell line editing (issue #8): `handle_key_event` maintains a byte-index `cursor_position` (commands are ASCII), inserting chars mid-line and redrawing via `terminal::redraw_input`. Left/Right/Home/End move the cursor; Backspace/Delete remove before/at it; chars typed at the end of the line echo directly (serial-visible), everything else only redraws the framebuffer. `Up`/`Down` walk the history (index starts at `history.len()`); the first `Up` from a fresh line saves the buffer into `draft`, which `Down` restores when navigating past the newest entry. Executing a command or Ctrl+C resets index/draft; Ctrl+C prints `^C` + fresh prompt, Ctrl+L clears the screen and redraws any pending input. Rendering state (`input_start_y`, `rendered_cells`) is captured in `print_prompt` so redraws clear exactly what was on screen.
- `src/terminal.rs`: Framebuffer text terminal (8x8 font from `src/font.rs`, direct pixel writes). Scrolling shifts the framebuffer up one text row (10 px) via `KernelGraphics::scroll_up` memmove and clears only the exposed bottom row. `terminal::redraw_input(prompt, buffer, cursor_pos, start_y, previous_cells)` redoes the shell input line in place without ANSI escapes: clears `max(previous_cells, prompt.len()+buffer.len())` cells from `(0, start_y)` via `write_char`, redraws prompt + buffer, and parks the cursor at `prompt.len()+cursor_pos` (row/col via `self.cols`; `start_y` clamped to the last row if output scrolled). Redraws are thread context and not mirrored to serial.
- `src/keyboard.rs`: Scancode set 1 + 0xE0-prefixed extended codes → `KeyEvent` (`Char(c)`, `Enter`, `Backspace`, `Delete`, `Up/Down/Left/Right`, `Home/End`, `CtrlC`, `CtrlL`), tracks Shift + left/right Ctrl. The `extended` flag consumes fake shifts (E0 2A/36/AA/B6 → ignored, no shift-state change); E0 1D/9D track right Ctrl; extended break codes produce no event. Non-extended 0x48/50/4B/4D/47/4F/53 map to the same navigation events (NumLock caveat: with NumLock off real keyboards send digits there; QEMU defaults to NumLock on). Ctrl+<char> yields events only for C and L.

**Test Harness** (`src/test_harness.rs`, cargo feature `test-harness`, issue #13):
- Only compiled with `--features test-harness` (`make test`); normal builds are unchanged. In test builds `kernel_main` calls `test_harness::run()` right after `fs::init()` and BEFORE `interrupts::enable()`, so the harness never touches the shell, the scancode queue, or any IRQ path, and runs with interrupts still disabled.
- Six deterministic tests: heap churn (allocate/verify/drop `Vec`s and `String`s of varied sizes over the linked-list heap), frame-allocator invariants (3 frames: 4 KiB alignment, distinct, < 4 GiB, free count restored), FS CRUD (create/read/overwrite/delete `/harness.txt`, mkdir/rmdir lifecycle incl. non-empty rmdir rejection), path normalization (`cd` with `..`, `//`, trailing `/`, `..` past root; rmdir of the cwd rejected), error formatting (missing paths give non-empty messages naming the path), and keyboard decode (pure `Keyboard` state machine: E0-prefixed arrows/Home/End/Delete/keypad Enter, fake shifts don't stick, Ctrl+C/Ctrl+L, extended release codes ignored). Each test prints `[PASS]`/`[FAIL]` (mirrored to COM1) and returns a `Result` so one failure doesn't halt the rest; no `assert!` panics, no keyboard/QMP dependency.
- Exit path: writes 0x10 (pass) or 0x11 (fail) to isa-debug-exit port 0xF4 → QEMU exit `(value << 1) | 1` = 33 or 35; `make test` treats 33 as success and everything else (including 60 s timeout) as failure.

### Module Dependencies & Conventions
- All interrupt handlers must use `extern "x86-interrupt"` ABI
- Framebuffer info must be captured before `ExitBootServices()`
- All static mutable state is guarded with `spin::Mutex` (+ `lazy_static`). The ONLY lock shared between ISR and thread context is `SCANCODE_QUEUE` in `src/keyboard.rs`; thread-side takers must disable interrupts around it. Never call into `TERMINAL`, `SHELL`, `FILESYSTEM`, or the allocator from an interrupt handler — that reintroduces the single-core deadlock this design removed (issue #2)
- Version comes from `Cargo.toml` alone (`env!("CARGO_PKG_VERSION")`, expanded at compile time — safe before the heap is up): kernel boot banner and the shell `version` command print `ilo-lawa OS v<version>`, the shell banner prints `ilo-lawa Shell v<version>`. Never hardcode version literals in source.
- The release build is warning-free (issue #15). Keep it that way: fix warnings at the root; targeted allowances with an issue reference are the only acceptable form.

## Task Tracking

Work is tracked as GitHub Issues on `hnkNkm/ilo-lawa` (`gh issue list`), labeled P0-critical through P3-low. The issue set came from a full gap analysis (2026-07); check it for known limitations before filing new ones.

## Common Issues and Solutions

- **Crashes after enabling interrupts**: Check GDT segment selectors, ensure all segments are properly initialized
- **Double faults**: Verify IST stack is properly allocated and TSS is loaded
- **Keyboard not working**: Ensure PIC mask enables IRQ1, check EOI is sent properly
- **Allocation failures / panics in shell commands**: the heap is a fixed static 1 MiB `linked_list_allocator` area; if commands OOM, look for leaked allocations first, then grow `HEAP_SIZE` in `src/allocator.rs`
- **Build failures**: Always build inside `nix develop --impure`
