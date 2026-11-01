//! CMOS RTC wall-clock driver (issue #20).
//!
//! After ExitBootServices the UEFI Runtime GetTime service is gone
//! (src/main.rs drops the runtime table), so the MC146818-compatible CMOS
//! RTC behind I/O ports 0x70/0x71 is the kernel's only calendar source;
//! monotonic time stays in the PIT tick counter (src/time.rs).
//!
//! [`read`] performs one consistent CMOS read: it waits out the
//! update-in-progress flag (status register A bit 7, set for at least
//! 244 us before each internal update), captures all time registers,
//! re-checks the flag to detect a mid-read update (the sample is then
//! discarded and the read retried), converts the BCD/12-hour encodings
//! selected by status register B, and range-validates every field before
//! returning. [`init`] takes the boot-time snapshot and [`now`] extrapolates
//! it with PIT ticks, so `date` needs no calendar re-read per call — and
//! the same snapshot can later feed FAT32 timestamps (issue #14).
//!
//! Rules: thread context only, plain port I/O, no locks, no heap, no ISRs —
//! no interrupt handler may touch the CMOS, and callers must not hold other
//! locks across [`read`] (the UIP wait busy-polls for up to ~1 ms).

use spin::Once;
use x86_64::instructions::port::Port;

/// CMOS address register. Bit 7 is the NMI mask; the kernel has no NMI
/// handler, so every access writes the index with NMI kept masked.
const CMOS_ADDR: u16 = 0x70;
/// CMOS data register (reads/writes the register last addressed).
const CMOS_DATA: u16 = 0x71;

// RTC register indexes (standard MC146818 layout).
const REG_SECOND: u8 = 0x00;
const REG_MINUTE: u8 = 0x02;
const REG_HOUR: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;
const REG_CENTURY: u8 = 0x32;

/// Status register A bit 7: update-in-progress (time registers in flux).
const STATUS_A_UIP: u8 = 1 << 7;
/// Status register B bit 1: 24-hour mode (clear = 12-hour mode, hour
/// bit 7 = PM).
const STATUS_B_24H: u8 = 1 << 1;
/// Status register B bit 2: binary data mode (clear = BCD).
const STATUS_B_BINARY: u8 = 1 << 2;

/// Busy-poll bound for the UIP wait. The flag never stays set longer than
/// one update period (1 s); each port read is ~1 us, so this covers even
/// pathologically slow hardware without risking a hang.
const UIP_WAIT_LOOPS: u32 = 1_000_000;
/// Full register-set captures before giving up (UIP races and one-shot
/// garbage both get a second chance).
const READ_ATTEMPTS: u32 = 3;

/// A wall-clock date and time exactly as the RTC holds it (no timezone
/// concept — QEMU's default RTC base is UTC, vendor boards hold local
/// time). `year` is the full calendar year (e.g. 2026).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Boot-time wall-clock snapshot: the CMOS value plus the PIT tick at
/// which it was taken. Written once by [`init`] (thread context, before
/// interrupts are enabled), read-only afterwards — no lock needed.
static SNAPSHOT: Once<(DateTime, u64)> = Once::new();

/// Write `index` to the address port (NMI masked) and read the data port.
fn read_reg(index: u8) -> u8 {
    unsafe {
        Port::<u8>::new(CMOS_ADDR).write(index | 0x80);
        Port::<u8>::new(CMOS_DATA).read()
    }
}

/// True while the RTC is mid-internal-update and its time registers are
/// momentarily inconsistent.
fn update_in_progress() -> bool {
    read_reg(REG_STATUS_A) & STATUS_A_UIP != 0
}

/// Busy-wait until the update-in-progress flag clears; `false` when the
/// wait bound is exhausted (RTC wedged — the caller gives up rather than
/// hang).
fn wait_update_done() -> bool {
    for _ in 0..UIP_WAIT_LOOPS {
        if !update_in_progress() {
            return true;
        }
    }
    false
}

/// Convert one register value to binary. BCD values with high or low
/// nibble > 9 are garbage (not clock digits) and rejected, so a bad
/// sample can only fail the read, never produce a wrong digit pair.
fn decode(value: u8, binary_mode: bool) -> Option<u8> {
    if binary_mode {
        return Some(value);
    }
    let hi = value >> 4;
    let lo = value & 0x0F;
    if hi > 9 || lo > 9 {
        return None;
    }
    Some(hi * 10 + lo)
}

/// Read the RTC once and return a validated `DateTime`, or `None` when no
/// consistent sample could be taken (UIP never cleared, update raced every
/// attempt, or out-of-range values).
///
/// Thread context only; must not be called with other locks held (the UIP
/// wait busy-polls) and never from an ISR.
pub fn read() -> Option<DateTime> {
    for _ in 0..READ_ATTEMPTS {
        if !wait_update_done() {
            return None;
        }

        let reg_b = read_reg(REG_STATUS_B);
        let binary_mode = reg_b & STATUS_B_BINARY != 0;
        let hour24 = reg_b & STATUS_B_24H != 0;

        let raw_second = read_reg(REG_SECOND);
        let raw_minute = read_reg(REG_MINUTE);
        let raw_hour = read_reg(REG_HOUR);
        let raw_day = read_reg(REG_DAY);
        let raw_month = read_reg(REG_MONTH);
        let raw_year = read_reg(REG_YEAR);
        let raw_century = read_reg(REG_CENTURY);

        // An update began while the registers were being read — the
        // sample straddles a rollover (e.g. 23:59:59 -> 00:00:00) and is
        // thrown away.
        if update_in_progress() {
            continue;
        }

        // In 12-hour mode bit 7 of the hour register is PM, not a value
        // bit; strip it before decoding in every mode (24-hour values are
        // <= 23, so bit 7 is always clear there anyway).
        let hour_pm = raw_hour & 0x80 != 0;

        // Decode order: second, minute, hour, day, month, year, century.
        let mut v = [0u8; 7];
        let raw = [
            raw_second,
            raw_minute,
            raw_hour & 0x7F,
            raw_day,
            raw_month,
            raw_year,
            raw_century,
        ];
        let mut ok = true;
        for (i, &value) in raw.iter().enumerate() {
            match decode(value, binary_mode) {
                Some(decoded) => v[i] = decoded,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        let [second, minute, hour12, day, month, year, century] = v;

        // 12-hour (1..=12 + PM) -> 24-hour.
        let hour = if hour24 {
            hour12
        } else {
            if !(1..=12).contains(&hour12) {
                continue;
            }
            match (hour12, hour_pm) {
                (12, false) => 0,
                (12, true) => 12,
                (h, true) => h + 12,
                (h, false) => h,
            }
        };

        // Range checks: a passed sample is a plausible wall clock, never a
        // pile of 0xFF from absent hardware.
        if second > 59 || minute > 59 || hour > 23 || !(1..=31).contains(&day) {
            continue;
        }
        if !(1..=12).contains(&month) || year > 99 {
            continue;
        }

        // Century register: used only when it decodes to a plausible
        // century — there is no reliable presence bit, and boards without
        // register 0x32 (or a firmware that never initializes it) return
        // arbitrary values there. QEMU's FADT is rev 1 (no CENTURY field),
        // but its CMOS register 0x32 is maintained and decodes fine.
        let full_year = match decode_checked(century, binary_mode, 19, 21) {
            Some(c) => u16::from(c) * 100 + u16::from(year),
            // No usable century: classic PC pivot on the two-digit year.
            None => {
                if year >= 80 {
                    1900 + u16::from(year)
                } else {
                    2000 + u16::from(year)
                }
            }
        };

        return Some(DateTime {
            year: full_year,
            month,
            day,
            hour,
            minute,
            second,
        });
    }
    None
}

/// [`decode`] plus a range check, for the century plausibility window.
fn decode_checked(value: u8, binary_mode: bool, min: u8, max: u8) -> Option<u8> {
    let decoded = decode(value, binary_mode)?;
    if (min..=max).contains(&decoded) {
        Some(decoded)
    } else {
        None
    }
}

/// Take the boot-time wall-clock snapshot. Returns the snapshot on
/// success; `None` when the RTC is unreadable (the `date` command then
/// reports an error instead of a time).
///
/// Thread context, no locks held. Intended to run once at boot while
/// interrupts are still disabled — plain port I/O, nothing IRQ-related.
pub fn init() -> Option<DateTime> {
    let dt = read()?;
    let base = crate::time::ticks();
    SNAPSHOT.call_once(|| (dt, base));
    Some(dt)
}

/// Current wall-clock time: the boot snapshot extrapolated by PIT ticks.
/// `None` when no snapshot was ever taken (`init` failed or never ran).
pub fn now() -> Option<DateTime> {
    let (snapshot, base) = SNAPSHOT.get()?;
    let elapsed_secs = crate::time::ticks().saturating_sub(*base) / crate::time::TICKS_PER_SECOND;
    Some(advance(*snapshot, elapsed_secs))
}

fn is_leap_year(year: u16) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        // Callers only pass validated months (1..=12).
        _ => 31,
    }
}

/// Add `seconds` to a validated `DateTime`, carrying through the calendar
/// (month lengths, leap years). The month loop runs once per elapsed
/// ~28 days, so even multi-year uptimes stay cheap.
fn advance(dt: DateTime, seconds: u64) -> DateTime {
    let total = u64::from(dt.second) + seconds;
    let second = (total % 60) as u8;

    let total = u64::from(dt.minute) + total / 60;
    let minute = (total % 60) as u8;

    let total = u64::from(dt.hour) + total / 60;
    let hour = (total % 24) as u8;

    let mut year = dt.year;
    let mut month = dt.month;
    let mut day = u32::from(dt.day) + (total / 24) as u32;
    loop {
        let len = u32::from(days_in_month(year, month));
        if day <= len {
            break;
        }
        day -= len;
        month += 1;
        if month > 12 {
            month = 1;
            year += 1;
        }
    }
    // Loop exit guarantees 1 <= day <= days_in_month <= 31: fits u8.
    let day = day as u8;

    DateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    }
}
