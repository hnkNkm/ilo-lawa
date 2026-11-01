//! ACPI table discovery and power control (issue #12).
//!
//! [`init`] walks the RSDP -> RSDT/XSDT -> FADT chain starting from the
//! physical RSDP address captured from the UEFI configuration table
//! BEFORE ExitBootServices (src/main.rs, `kernel::AcpiInfo`) — after EBS
//! the config table is gone for good. Tables are identity-mapped by
//! `paging::init`, so this module reads them through raw pointers with no
//! heap allocation and no page-table work; it may run before the
//! allocator exists and never allocates.
//!
//! Nothing here can panic: any malformed structure (bad RSDP signature or
//! checksum, table header, length bounds) only disables ACPI power
//! control, and the power commands fall back safely (shutdown halts,
//! reboot uses the 8042 keyboard-controller reset).
//!
//! Known limitation: the S5 sleep type is hardcoded (see [`S5_SLEEP_TYPE`])
//! because the `\_S5` AML package in the DSDT is not parsed. QEMU's
//! generated DSDT uses SLP_TYPa = 5, which is what we verify against;
//! arbitrary vendor firmware may declare a different value.

use core::arch::asm;
use spin::Once;
use x86_64::instructions::port::Port;

/// S5 power-off value written to PM1a_CNT_BLK: SLP_TYPa in bits 10..13
/// plus SLP_EN (bit 13). SLP_TYP values are NOT spec-fixed — each
/// platform defines them in the DSDT `\_S5` package, whose AML this
/// kernel does NOT parse. The value here (0) is read out of QEMU's
/// generated DSDT (dumped via the QEMU monitor: `\_S5 = Package(4)
/// {Zero, Zero, Zero, Zero}`), so power-off is verified on QEMU;
/// arbitrary vendor firmware may need a different SLP_TYPa — a documented
/// limitation, same as before \_S5 parsing exists.
const S5_SLEEP_TYPE: u16 = 0;
const SLP_EN: u16 = 1 << 13;

/// PM1b keeps SLP_EN but uses SLP_TYPb = 0 (firmware-defined; QEMU has
/// no second PM1 block, so this is only defensive).
const PM1B_VALUE: u16 = SLP_EN;

// RSDP layout (ACPI spec): 20 bytes common to every revision; ACPI 2.0+
// extends them to at least 36 bytes with the XSDT pointer and a
// second (extended) checksum over the whole RSDP.
const RSDP_SIGNATURE: [u8; 8] = *b"RSD PTR ";
const RSDP_COMMON_LEN: u64 = 20;
const RSDP_EXT_LEN_MIN: u64 = 36;
// System Description Table header length (RSDT/XSDT/FADT share it).
const SDT_HEADER_LEN: u64 = 36;

const FACP_SIGNATURE: [u8; 4] = *b"FACP";

// FADT field offsets (ACPI spec):
const FADT_PM1A_CNT_BLK: u64 = 0x40; // u32 I/O port
const FADT_PM1B_CNT_BLK: u64 = 0x44; // u32 I/O port
const FADT_RESET_REG: u64 = 0x74; // GAS: space u8 @+0, address u64 @+4
const FADT_RESET_VALUE: u64 = 0x80; // u8
// Shortest FADT that still contains the PM1 control blocks.
const FADT_MIN_LEN_PM1: u64 = 0x48;
// Shortest FADT that still contains the reset register + value.
const FADT_MIN_LEN_RESET: u64 = 0x84;

// Sanity bound for any table length we are asked to checksum: real ACPI
// tables are tiny (FADT <= ~600 bytes), and the cap keeps a corrupt
// length field from walking the checksum off the identity map into an
// unmapped page (which would fault).
const MAX_TABLE_LEN: u64 = 1 << 20;

/// ACPI 1.0 RSDP fields shared by every revision (20 bytes): signature
/// 0..8, checksum 8, OEMID 9..15, revision 15, RSDT address 16..20.
const RSDP_OEM_OFFSET: u64 = 9;
const RSDP_REV_OFFSET: u64 = 15;
const RSDP_RST_OFFSET: u64 = 16;
const RSDP_LEN_OFFSET: u64 = 20;
const RSDP_XSDT_OFFSET: u64 = 24;

/// Control registers parsed out of the FADT. Kept by value in a `Once`:
/// written exactly once during [`init`] (thread context, interrupts
/// disabled), read-only afterwards — no lock needed for the readers.
#[derive(Clone, Copy, Default)]
struct AcpiState {
    /// PM1a_CNT_BLK / PM1b_CNT_BLK: 16-bit I/O ports. 0 = unavailable.
    pm1a_cnt_blk: u16,
    pm1b_cnt_blk: u16,
    /// FADT RESET_REG; reboot uses the 8042 fallback when `None`.
    reset_reg: Option<GenericAddress>,
    /// FADT RESET_VALUE, written to `reset_reg`.
    reset_value: u8,
}

/// Minimal ACPI Generic Address Structure: only the fields needed to
/// perform a write.
#[derive(Clone, Copy, Default)]
struct GenericAddress {
    /// 0 = system memory, 1 = system I/O; anything else is ignored.
    address_space: u8,
    address: u64,
}

static ACPI: Once<AcpiState> = Once::new();

// Unaligned-tolerant raw reads. Every ACPI pointer is a physical address
// and identity-mapped after paging::init; unaligned loads keep this safe
// even where firmware packs fields at odd offsets.
fn read_u8(addr: u64) -> u8 {
    unsafe { core::ptr::read_unaligned(addr as *const u8) }
}

fn read_u32(addr: u64) -> u32 {
    unsafe { core::ptr::read_unaligned(addr as *const u32) }
}

fn read_u64(addr: u64) -> u64 {
    unsafe { core::ptr::read_unaligned(addr as *const u64) }
}

fn read_bytes<const N: usize>(addr: u64) -> [u8; N] {
    unsafe { core::ptr::read_unaligned(addr as *const [u8; N]) }
}

/// Modulo-256 sum of `len` bytes at `addr`; an ACPI table is valid when
/// the sum is 0. Callers bound `len` (see [`MAX_TABLE_LEN`]) so the walk
/// cannot leave the identity map.
fn checksum(addr: u64, len: u64) -> u8 {
    let mut sum: u8 = 0;
    for i in 0..len {
        sum = sum.wrapping_add(read_u8(addr + i));
    }
    sum
}

/// One-time parse of the RSDP/FADT chain. Called from the init sequence
/// (after paging/terminal, before the heap exists); logs the outcome to
/// serial either way so the boot log shows whether ACPI is in control.
pub fn init(rsdp_addr: u64) {
    ACPI.call_once(|| discover(rsdp_addr));
}

/// Walk RSDP -> RSDT/XSDT -> FADT and return the parsed control state.
/// Any malformed step returns the all-disabled default instead of
/// panicking; the power commands then fall back to plain halt/8042.
fn discover(rsdp_addr: u64) -> AcpiState {
    if rsdp_addr == 0 {
        crate::serial::print(
            "[acpi] no RSDP in UEFI config table; ACPI power control disabled\n",
        );
        return AcpiState::default();
    }

    if read_bytes::<8>(rsdp_addr) != RSDP_SIGNATURE {
        crate::serial::print("[acpi] RSDP signature invalid; ACPI disabled\n");
        return AcpiState::default();
    }

    // Legacy checksum over the first 20 bytes exists in every revision.
    if checksum(rsdp_addr, RSDP_COMMON_LEN) != 0 {
        crate::serial::print("[acpi] RSDP checksum invalid; ACPI disabled\n");
        return AcpiState::default();
    }

    let revision = read_u8(rsdp_addr + RSDP_REV_OFFSET);
    let rsdt_addr = u64::from(read_u32(rsdp_addr + RSDP_RST_OFFSET));
    let oem_id = read_bytes::<6>(rsdp_addr + RSDP_OEM_OFFSET);
    let printable = |b: u8| {
        if b.is_ascii_graphic() || b == b' ' {
            b
        } else {
            b'?'
        }
    };
    let oem: [u8; 6] = oem_id.map(printable);

    // ACPI 2.0+ adds the XSDT pointer plus an extended checksum over the
    // whole RSDP. Only read the extended fields when the revision says
    // they exist (an ACPI 1.0 RSDP is just 20 bytes).
    let mut xsdt_addr = 0u64;
    if revision >= 2 {
        let ext_len = u64::from(read_u32(rsdp_addr + RSDP_LEN_OFFSET));
        if !(RSDP_EXT_LEN_MIN..=MAX_TABLE_LEN).contains(&ext_len)
            || checksum(rsdp_addr, ext_len) != 0
        {
            crate::serial::print("[acpi] RSDP extended checksum invalid; ACPI disabled\n");
            return AcpiState::default();
        }
        xsdt_addr = read_u64(rsdp_addr + RSDP_XSDT_OFFSET);
    }

    crate::serial::print_fmt(format_args!(
        "[acpi] RSDP rev {} @ {:#x} OEM '{}'\n",
        revision,
        rsdp_addr,
        core::str::from_utf8(&oem).unwrap_or("??????"),
    ));

    // XSDT (8-byte entries) is preferred on ACPI 2.0+; RSDT (4-byte
    // entries) is the fallback. A root table that fails validation is
    // skipped so the other one still gets a chance.
    if xsdt_addr != 0 {
        if let Some(state) = parse_root(xsdt_addr, 8, b"XSDT") {
            return state;
        }
        crate::serial::print("[acpi] XSDT unusable; falling back to RSDT\n");
    }
    if rsdt_addr != 0 {
        if let Some(state) = parse_root(rsdt_addr, 4, b"RSDT") {
            return state;
        }
        crate::serial::print("[acpi] RSDT unusable\n");
    }
    crate::serial::print("[acpi] no FADT found; ACPI power control disabled\n");
    AcpiState::default()
}

/// Validate an RSDT/XSDT header and scan its entry list for the FADT
/// ("FACP"). `entry_size` is 8 (XSDT) or 4 (RSDT) — ACPI 2.0 widened the
/// pointer entries. `None` = malformed table or no FADT entry.
fn parse_root(root_addr: u64, entry_size: u64, signature: &[u8; 4]) -> Option<AcpiState> {
    if read_bytes::<4>(root_addr) != *signature {
        crate::serial::print_fmt(format_args!(
            "[acpi] {} signature invalid\n",
            core::str::from_utf8(signature).unwrap_or("root"),
        ));
        return None;
    }
    let len = u64::from(read_u32(root_addr + 4));
    if !(SDT_HEADER_LEN..=MAX_TABLE_LEN).contains(&len) || checksum(root_addr, len) != 0 {
        crate::serial::print_fmt(format_args!(
            "[acpi] {} length/checksum invalid\n",
            core::str::from_utf8(signature).unwrap_or("root"),
        ));
        return None;
    }

    let entries = (len - SDT_HEADER_LEN) / entry_size;
    crate::serial::print_fmt(format_args!(
        "[acpi] {} @ {:#x}: {} entries\n",
        core::str::from_utf8(signature).unwrap_or("root"),
        root_addr,
        entries,
    ));
    for i in 0..entries {
        let entry_addr = root_addr + SDT_HEADER_LEN + i * entry_size;
        let table = match entry_size {
            8 => read_u64(entry_addr),
            _ => u64::from(read_u32(entry_addr)),
        };
        if table == 0 || read_bytes::<4>(table) != FACP_SIGNATURE {
            continue;
        }
        return parse_fadt(table);
    }
    None
}

/// Validate the FADT header and extract the PM1 control blocks and (when
/// the table is long enough) the reset register. Malformed FADT returns
/// `None` so the caller falls back safely.
fn parse_fadt(fadt_addr: u64) -> Option<AcpiState> {
    let len = u64::from(read_u32(fadt_addr + 4));
    if !(FADT_MIN_LEN_PM1..=MAX_TABLE_LEN).contains(&len) {
        crate::serial::print_fmt(format_args!(
            "[acpi] FADT @ {:#x}: length {} invalid\n",
            fadt_addr, len,
        ));
        return None;
    }
    if checksum(fadt_addr, len) != 0 {
        crate::serial::print_fmt(format_args!(
            "[acpi] FADT @ {:#x}: checksum invalid\n",
            fadt_addr,
        ));
        return None;
    }

    let mut state = AcpiState::default();
    state.pm1a_cnt_blk = read_u32(fadt_addr + FADT_PM1A_CNT_BLK) as u16;
    state.pm1b_cnt_blk = read_u32(fadt_addr + FADT_PM1B_CNT_BLK) as u16;

    if len >= FADT_MIN_LEN_RESET {
        let space = read_u8(fadt_addr + FADT_RESET_REG);
        let address = read_u64(fadt_addr + FADT_RESET_REG + 4);
        // Only the two writable address spaces are usable with our
        // primitives; PCI config / SMBus GAS entries are ignored.
        if address != 0 && (space == 0 || space == 1) {
            state.reset_reg = Some(GenericAddress { address_space: space, address });
            state.reset_value = read_u8(fadt_addr + FADT_RESET_VALUE);
        } else {
            crate::serial::print_fmt(format_args!(
                "[acpi] reset reg unusable: space {} address {:#x}\n",
                space, address,
            ));
        }
    }

    crate::serial::print_fmt(format_args!(
        "[acpi] FADT @ {:#x} len {}: PM1a_CNT={:#x} PM1b_CNT={:#x}\n",
        fadt_addr, len, state.pm1a_cnt_blk, state.pm1b_cnt_blk,
    ));
    match state.reset_reg {
        Some(reg) if reg.address_space == 1 => crate::serial::print_fmt(format_args!(
            "[acpi] reset reg: I/O {:#x} value {:#x}\n",
            reg.address, state.reset_value,
        )),
        Some(reg) => crate::serial::print_fmt(format_args!(
            "[acpi] reset reg: MMIO {:#x} value {:#x}\n",
            reg.address, state.reset_value,
        )),
        None => crate::serial::print("[acpi] reset reg: none (8042 fallback)\n"),
    }
    Some(state)
}

/// Current ACPI state; the all-disabled default when `init` has not run
/// or found no usable FADT.
fn state() -> AcpiState {
    ACPI.get().copied().unwrap_or_default()
}

/// Power the machine off via ACPI S5, halting forever when no usable FADT
/// was found. Never returns.
pub fn shutdown() -> ! {
    let state = state();
    if state.pm1a_cnt_blk != 0 {
        // S5 via fixed hardware: see S5_SLEEP_TYPE for the \_S5 caveat.
        let value = (S5_SLEEP_TYPE << 10) | SLP_EN;
        unsafe { Port::<u16>::new(state.pm1a_cnt_blk).write(value) };
    }
    if state.pm1b_cnt_blk != 0 {
        // SLP_TYPb = 0 with SLP_EN; unreachable on QEMU (single PM1 block).
        unsafe { Port::<u16>::new(state.pm1b_cnt_blk).write(PM1B_VALUE) };
    }
    halt_loop()
}

/// Reset the machine: FADT RESET_REG first, then the 8042 keyboard
/// controller `0xFE` command, then halt forever. Never returns.
pub fn reboot() -> ! {
    let state = state();
    if let Some(reg) = state.reset_reg {
        match reg.address_space {
            // System I/O: RESET_VALUE is one byte; I/O space is 16-bit.
            1 => unsafe { Port::<u8>::new(reg.address as u16).write(state.reset_value) },
            // System memory: MMIO needs volatile stores.
            0 => unsafe { (reg.address as *mut u8).write_volatile(state.reset_value) },
            _ => {}
        }
        // Give the reset line a moment before trying the fallback.
        spin_wait(4_000_000);
    }

    // 8042 keyboard controller: pulse the CPU reset line (command 0xFE).
    // Wait for the input buffer to drain first (status bit 1 clear), with
    // a bounded timeout so a wedged controller cannot hang us here.
    unsafe {
        let mut status = Port::<u8>::new(0x64);
        for _ in 0..100_000 {
            if status.read() & 0x02 == 0 {
                break;
            }
        }
        Port::<u8>::new(0x64).write(0xFE);
    }
    spin_wait(4_000_000);

    halt_loop()
}

fn halt_loop() -> ! {
    x86_64::instructions::interrupts::disable();
    loop {
        unsafe { asm!("hlt") };
    }
}

/// Busy-wait roughly `n` iterations (reset lines settle in microseconds;
/// this just keeps the fallback from firing in the same instant).
fn spin_wait(n: u64) {
    for i in 0..n {
        core::hint::spin_loop();
        core::hint::black_box(i);
    }
}
