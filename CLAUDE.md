# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

ilo-lawa is a minimal UEFI-based operating system kernel written in Rust that transitions from UEFI boot services to a standalone kernel environment. The kernel implements custom interrupt handling, memory management, and basic I/O.

## Development Commands

### Building
```bash
# Build the kernel (requires Nix environment)
make build

# Alternative: direct cargo build
nix develop --impure -c cargo build --release
```

### Running
```bash
# Run on macOS or systems without KVM
make run-no-kvm

# Run on Linux with KVM support
make run

# Clean build artifacts
make clean
```

### Development Environment
```bash
# Enter Nix development shell (required for all builds)
nix develop --impure
```

## Architecture Overview

### Boot Process Flow
1. **UEFI Entry** (`src/main.rs`): UEFI bootloader entry point that initializes graphics, captures framebuffer info, and calls `ExitBootServices()` to transition to kernel mode
2. **Kernel Entry** (`src/kernel.rs:kernel_main`): Post-UEFI kernel that sets up GDT, IDT, PIC and enables interrupts
3. **Interrupt System**: Custom x86_64 interrupt handling with PIC 8259 support (legacy mode)

### Critical Components

**Interrupt Architecture**:
- `src/gdt.rs`: Global Descriptor Table setup with TSS for interrupt stack switching
- `src/interrupts.rs`: IDT configuration with handlers for timer, keyboard, double fault
- `src/pic.rs`: 8259 PIC initialization and EOI handling

**Key Design Decisions**:
- Uses `x86_64` crate for safe low-level operations
- Implements IST (Interrupt Stack Table) for double fault handling
- Currently uses legacy PIC instead of modern APIC
- No heap allocator - all memory is statically allocated
- Terminal output directly manipulates framebuffer memory

### Module Dependencies
- `kernel` depends on: `gdt`, `interrupts`, `pic`, `terminal`
- `interrupts` depends on: `pic`, `keyboard`
- All interrupt handlers must use `extern "x86-interrupt"` ABI
- Framebuffer info must be captured before `ExitBootServices()`

## Important Constraints

1. **No UEFI Services After kernel_main**: Once `ExitBootServices()` is called, no UEFI protocols or boot services are available
2. **Interrupt Safety**: All static mutable data must be protected with `spin::Mutex` for interrupt safety
3. **No Standard Library**: Project is `#![no_std]` - no heap, threads, or OS abstractions
4. **UEFI Target**: Must compile for `x86_64-unknown-uefi` target
5. **Nightly Rust Required**: Uses unstable features like `#![feature(abi_x86_interrupt)]`

## Common Issues and Solutions

- **Crashes after enabling interrupts**: Check GDT segment selectors, ensure all segments are properly initialized
- **Double faults**: Verify IST stack is properly allocated and TSS is loaded
- **Keyboard not working**: Ensure PIC mask enables IRQ1, check EOI is sent properly
- **Build failures**: Always build inside `nix develop --impure` environment