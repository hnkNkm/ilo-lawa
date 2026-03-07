.PHONY: build run clean setup-ovmf

OVMF_PATH = ovmf

setup-ovmf:
	@if [ ! -d "$(OVMF_PATH)" ]; then \
		mkdir -p $(OVMF_PATH); \
		echo "Downloading OVMF firmware..."; \
		curl -L https://retrage.github.io/edk2-nightly/bin/RELEASEX64_OVMF.fd -o $(OVMF_PATH)/OVMF.fd; \
	fi

build:
	nix develop --impure -c cargo build --release

run: build setup-ovmf
	mkdir -p esp/efi/boot
	cp target/x86_64-unknown-uefi/release/ilo-lawa.efi esp/efi/boot/bootx64.efi
	nix develop --impure -c qemu-system-x86_64 \
		-enable-kvm \
		-bios $(OVMF_PATH)/OVMF.fd \
		-drive format=raw,file=fat:rw:esp \
		-serial stdio \
		-m 512M

run-no-kvm: build setup-ovmf
	mkdir -p esp/efi/boot
	cp target/x86_64-unknown-uefi/release/ilo-lawa.efi esp/efi/boot/bootx64.efi
	nix develop --impure -c qemu-system-x86_64 \
		-bios $(OVMF_PATH)/OVMF.fd \
		-drive format=raw,file=fat:rw:esp \
		-serial stdio \
		-m 512M

clean:
	cargo clean
	rm -rf esp
	rm -rf $(OVMF_PATH)