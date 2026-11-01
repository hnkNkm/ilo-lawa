.PHONY: build run run-no-kvm test clean setup-ovmf disk

OVMF_PATH = ovmf

# Separate target dir for the test build so `make build`'s cache is untouched.
TEST_TARGET_DIR = target-test

# Raw data disk for the virtio-blk driver (issue #14). Created once and
# kept persistent across runs; `make disk` (re)installs the test file.
DISK_IMG = disk.img
DISK_SIZE_MIB = 64

setup-ovmf:
	@if [ ! -d "$(OVMF_PATH)" ]; then \
		mkdir -p $(OVMF_PATH); \
		echo "Downloading OVMF firmware..."; \
		curl -L https://retrage.github.io/edk2-nightly/bin/RELEASEX64_OVMF.fd -o $(OVMF_PATH)/OVMF.fd; \
	fi

# 64 MiB FAT32 image (superfloppy, no partition table) with a test file.
# Needs dosfstools/mtools (in the Nix dev shell). The image is created
# only if absent; the test file is copied on every invocation (mcopy -o
# overwrites) so content updates propagate. sha256sum prints the image
# digest so the booted sector content can be cross-checked.
disk:
	@if [ ! -f "$(DISK_IMG)" ]; then \
		echo "Creating $(DISK_SIZE_MIB) MiB raw disk image ($(DISK_IMG))..."; \
		dd if=/dev/zero of=$(DISK_IMG) bs=1M count=$(DISK_SIZE_MIB) status=none; \
		nix develop --impure -c mkfs.vfat -F 32 -n ILOLAWA $(DISK_IMG); \
	fi
	nix develop --impure -c sh -c 'printf "Hello from ilo-lawa virtio-blk! (issue #14)\n" > .disk-hello.txt && mcopy -o -i $(DISK_IMG) .disk-hello.txt ::HELLO.TXT && mcopy -o -i $(DISK_IMG) .disk-hello.txt ::long_file_name_test.txt && rm .disk-hello.txt && mdir -i $(DISK_IMG) ::'
	sha256sum $(DISK_IMG)

build:
	nix develop --impure -c cargo build --release

run: build setup-ovmf disk
	mkdir -p esp/efi/boot
	cp target/x86_64-unknown-uefi/release/ilo-lawa.efi esp/efi/boot/bootx64.efi
	nix develop --impure -c qemu-system-x86_64 \
		-enable-kvm \
		-bios $(OVMF_PATH)/OVMF.fd \
		-drive format=raw,file=fat:rw:esp \
		-drive format=raw,file=$(CURDIR)/$(DISK_IMG),if=none,id=disk0 \
		-device virtio-blk-pci,drive=disk0 \
		-serial stdio \
		-m 512M

run-no-kvm: build setup-ovmf disk
	mkdir -p esp/efi/boot
	cp target/x86_64-unknown-uefi/release/ilo-lawa.efi esp/efi/boot/bootx64.efi
	nix develop --impure -c qemu-system-x86_64 \
		-bios $(OVMF_PATH)/OVMF.fd \
		-drive format=raw,file=fat:rw:esp \
		-drive format=raw,file=$(CURDIR)/$(DISK_IMG),if=none,id=disk0 \
		-device virtio-blk-pci,drive=disk0 \
		-serial stdio \
		-m 512M

# Build the kernel with the test harness feature, boot it headless in QEMU
# with an isa-debug-exit device, and map the exit code: the harness writes
# 0x10 to port 0xF4 on success -> QEMU exits (0x10<<1)|1 = 33 (pass);
# 0x11 -> 35, and timeout/panic/hang give any other code (all failures).
# The full serial log lands in test-serial.log (gitignored).
test: setup-ovmf
	nix develop --impure -c env CARGO_TARGET_DIR=$(TEST_TARGET_DIR) cargo build --release --features test-harness
	mkdir -p esp/efi/boot
	cp $(TEST_TARGET_DIR)/x86_64-unknown-uefi/release/ilo-lawa.efi esp/efi/boot/bootx64.efi
	@echo "Running kernel test harness (headless QEMU, 60s timeout)..."
	@timeout 60 nix develop --impure -c qemu-system-x86_64 \
		-bios $(OVMF_PATH)/OVMF.fd \
		-drive format=raw,file=fat:rw:esp \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04 \
		-display none \
		-serial file:test-serial.log \
		-m 512M \
		-no-reboot; \
	status=$$?; \
	echo; \
	echo "--- serial log (test-serial.log) ---"; \
	cat test-serial.log; \
	echo "--- end serial log ---"; \
	if [ $$status -eq 33 ]; then \
		echo "make test: PASS (QEMU exit 33, ALL TESTS PASSED)"; \
	else \
		echo "make test: FAIL (QEMU exit $$status, expected 33)"; \
		exit 1; \
	fi

clean:
	cargo clean
	rm -rf esp
	rm -rf $(OVMF_PATH)
	rm -rf $(TEST_TARGET_DIR)
	rm -f test-serial.log
	rm -f $(DISK_IMG)