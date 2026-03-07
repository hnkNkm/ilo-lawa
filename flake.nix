{
  description = "Rust OS development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        
        rustToolchain = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [ "rust-src" "llvm-tools-preview" ];
          targets = [ "x86_64-unknown-uefi" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            rust-analyzer
            cargo-bootimage
            cargo-xbuild
            qemu
            nasm
            binutils
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
            grub2
            xorriso
          ];

          shellHook = ''
            echo "Rust OS development environment loaded"
            echo "Available commands:"
            echo "  cargo build - Build the kernel"
            echo "  cargo run - Build and run in QEMU"
            echo "  make iso - Create bootable ISO image"
          '';
        };
      });
}