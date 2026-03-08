.PHONY: build run clean setup-ovmf create-disk

OVMF_PATH = ovmf
DISK_IMG = disk.img

setup-ovmf:
	@if [ ! -d "$(OVMF_PATH)" ]; then \
		mkdir -p $(OVMF_PATH); \
		echo "Downloading OVMF firmware..."; \
		curl -L https://retrage.github.io/edk2-nightly/bin/RELEASEX64_OVMF.fd -o $(OVMF_PATH)/OVMF.fd; \
	fi

create-disk:
	@if [ ! -f "$(DISK_IMG)" ]; then \
		echo "Creating 64MB disk image..."; \
		dd if=/dev/zero of=$(DISK_IMG) bs=1M count=64; \
	fi

build:
	nix develop --impure -c cargo build --release

run: build setup-ovmf create-disk
	mkdir -p esp/efi/boot
	cp target/x86_64-unknown-uefi/release/ilo-lawa.efi esp/efi/boot/bootx64.efi
	nix develop --impure -c qemu-system-x86_64 \
		-enable-kvm \
		-bios $(OVMF_PATH)/OVMF.fd \
		-drive format=raw,file=fat:rw:esp \
		-drive file=$(DISK_IMG),if=none,id=disk0,format=raw \
		-device virtio-blk-pci,drive=disk0,disable-legacy=on,disable-modern=off \
		-serial stdio \
		-m 512M

run-no-kvm: build setup-ovmf create-disk
	mkdir -p esp/efi/boot
	cp target/x86_64-unknown-uefi/release/ilo-lawa.efi esp/efi/boot/bootx64.efi
	nix develop --impure -c qemu-system-x86_64 \
		-bios $(OVMF_PATH)/OVMF.fd \
		-drive format=raw,file=fat:rw:esp \
		-drive file=$(DISK_IMG),if=none,id=disk0,format=raw \
		-device virtio-blk-pci,drive=disk0,disable-legacy=on,disable-modern=off \
		-serial stdio \
		-m 512M

clean:
	cargo clean
	rm -rf esp
	rm -rf $(OVMF_PATH)
	rm -f $(DISK_IMG)