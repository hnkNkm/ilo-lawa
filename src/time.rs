//! PIT (8253/8254) timer driver and the kernel's monotonic tick counter.
//!
//! Channel 0 is programmed to 100 Hz in square-wave mode, so IRQ0 fires
//! every 10 ms and [`TICKS`] becomes a monotonic uptime counter (issue #7).
//! [`tick`] is called from the timer ISR only; it must stay lock-free,
//! allocation-free, and silent (no console output) — see the ISR rules in
//! `interrupts.rs`.
//!
//! All APIs that sleep ([`sleep_ticks`], [`sleep_ms`]) MUST be called from
//! thread context with interrupts enabled: they park on `enable_and_hlt`
//! and would deadlock (sleep forever, IRQ never serviced on the same CPU)
//! if invoked with interrupts disabled, and calling them from an ISR is
//! forbidden like any other blocking/console work.

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

/// Base input frequency of the PIT in Hz.
const PIT_BASE_FREQUENCY: u64 = 1_193_182;
/// Channel 0 output frequency in Hz (divisor 11_932 => 100.0004 Hz).
const TICKS_PER_SECOND: u64 = 100;
/// Divisor programmed into the PIT for [`TICKS_PER_SECOND`] (rounded).
const PIT_DIVISOR: u16 = (PIT_BASE_FREQUENCY / TICKS_PER_SECOND) as u16;
/// Milliseconds between timer ticks (1000 / 100 Hz).
const MS_PER_TICK: u64 = 1000 / TICKS_PER_SECOND;

/// Monotonic tick counter, incremented by the timer ISR (IRQ0).
///
/// `Relaxed` is enough: this is a standalone counter with no other memory
/// it orders, and the only writer is the single ISR on a single CPU.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Program PIT channel 0 to [`TICKS_PER_SECOND`] (100 Hz).
///
/// Must run after PIC init (IRQ0 handler loaded, IRQ0 unmasked) and before
/// interrupts are enabled, so no tick is serviced through a half-set-up
/// timer.
pub fn init() {
    unsafe {
        let mut command: Port<u8> = Port::new(0x43);
        let mut channel0: Port<u8> = Port::new(0x40);
        // 0x36 = channel 0, read/write LSB then MSB, mode 3 (square wave),
        // binary count. With square-wave mode the output toggles at half
        // the divisor rate, but IRQ0 fires once per full period, so the
        // effective interrupt rate is still TICKS_PER_SECOND.
        command.write(0x36);
        channel0.write(PIT_DIVISOR as u8);
        channel0.write((PIT_DIVISOR >> 8) as u8);
    }
}

/// Increment the tick counter. Timer ISR (IRQ0) only — see module rules.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Timer ticks since kernel start (10 ms resolution).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Whole seconds elapsed since kernel start (10 ms resolution, truncated).
pub fn uptime_seconds() -> u64 {
    ticks() / TICKS_PER_SECOND
}

/// Sleep for at least `n` timer ticks (10 ms each).
///
/// Thread context with interrupts enabled only (see module docs). The
/// wait is "at least": the wake-up happens on the first tick boundary at
/// or after the target, and the idle loop halves latency vs. busy polling.
pub fn sleep_ticks(n: u64) {
    let target = ticks().saturating_add(n);
    while ticks() < target {
        // sti; hlt: the pending timer IRQ wakes us at the next tick and is
        // serviced before the loop re-reads the counter.
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

/// Sleep for at least `ms` milliseconds, rounded up to whole 10 ms ticks.
/// `ms == 0` returns immediately.
///
/// Thread context with interrupts enabled only (see module docs).
pub fn sleep_ms(ms: u64) {
    if ms == 0 {
        return;
    }
    sleep_ticks(ms.div_ceil(MS_PER_TICK));
}
