.PHONY: build run run-no-kvm test clean setup-ovmf

OVMF_PATH = ovmf

# Separate target dir for the test build so `make build`'s cache is untouched.
TEST_TARGET_DIR = target-test

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