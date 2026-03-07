# ilo-lawa

A minimal UEFI-based operating system written in Rust.

## Features

- UEFI boot support
- Written in Rust for memory safety
- Nix flakes for reproducible development environment
- QEMU support for testing

## Prerequisites

- Nix with flakes enabled
- x86_64 architecture support

## Building

```bash
make build
```

## Running

Run with QEMU (macOS/systems without KVM):
```bash
make run-no-kvm
```

Run with QEMU (Linux with KVM):
```bash
make run
```

## Project Structure

- `src/main.rs` - Main entry point for the UEFI application
- `Cargo.toml` - Rust project configuration
- `flake.nix` - Nix flake for development environment
- `Makefile` - Build and run commands
- `.cargo/config.toml` - Cargo configuration for UEFI target

## Development

Enter the Nix development shell:
```bash
nix develop --impure
```

Clean build artifacts:
```bash
make clean
```

## License

MIT