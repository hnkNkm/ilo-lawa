use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;
use x86_64::instructions::port::Port;

use crate::keyboard::KeyEvent;

const COM1_BASE: u16 = 0x3F8;

// TX side of COM1. This lock is for thread context and the panic path
// ONLY — ISRs must never take it (see terminal.rs / bug #2: an IRQ
// arriving while the main loop holds it would deadlock). The RX side
// never touches it: the IRQ4 handler below reads its own ports.
static SERIAL1: Mutex<SerialPort> = Mutex::new(SerialPort::new(COM1_BASE));

// RX-side reads happen on locally constructed ports in the IRQ4 handler
// (same pattern as the keyboard ISR): the TX lock must never be contended.

const RX_QUEUE_SIZE: usize = 256;

// Received bytes are pushed here by the IRQ4 handler and drained by the
// main loop. This is the serial-side lock shared with interrupt context;
// the main loop must take it with interrupts disabled (same rule as
// keyboard::SCANCODE_QUEUE). SERIAL1 stays thread-context-only.
struct SerialRxQueue {
    buffer: [u8; RX_QUEUE_SIZE],
    read_pos: usize,
    write_pos: usize,
}

impl SerialRxQueue {
    const fn new() -> Self {
        SerialRxQueue {
            buffer: [0; RX_QUEUE_SIZE],
            read_pos: 0,
            write_pos: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        let next = (self.write_pos + 1) % RX_QUEUE_SIZE;
        if next != self.read_pos {
            self.buffer[self.write_pos] = byte;
            self.write_pos = next;
        }
        // Queue full: drop the byte rather than block in the ISR
    }

    fn pop(&mut self) -> Option<u8> {
        if self.read_pos == self.write_pos {
            None
        } else {
            let byte = self.buffer[self.read_pos];
            self.read_pos = (self.read_pos + 1) % RX_QUEUE_SIZE;
            Some(byte)
        }
    }
}

static RX_QUEUE: Mutex<SerialRxQueue> = Mutex::new(SerialRxQueue::new());

/// Set when the previous byte consumed from COM1 was CR, so a following
/// LF in a CRLF pair does not produce a second Enter. Thread-context only.
static LAST_BYTE_WAS_CR: AtomicBool = AtomicBool::new(false);

pub struct SerialPort {
    data: Port<u8>,
    interrupt_enable: Port<u8>,
    fifo_control: Port<u8>,
    line_control: Port<u8>,
    modem_control: Port<u8>,
    line_status: Port<u8>,
}

impl SerialPort {
    const fn new(base: u16) -> Self {
        SerialPort {
            data: Port::new(base),
            interrupt_enable: Port::new(base + 1),
            fifo_control: Port::new(base + 2),
            line_control: Port::new(base + 3),
            modem_control: Port::new(base + 4),
            line_status: Port::new(base + 5),
        }
    }

    fn init(&mut self) {
        unsafe {
            self.interrupt_enable.write(0x00); // no UART interrupts while programming
            self.line_control.write(0x80); // DLAB set: next writes hit divisor latch
            self.data.write(0x03); // divisor low (115200 / 3 = 38400 baud)
            self.interrupt_enable.write(0x00); // divisor high
            self.line_control.write(0x03); // 8N1, DLAB clear
            self.fifo_control.write(0xC7); // enable + clear FIFOs, 14-byte trigger
            self.modem_control.write(0x0B); // DTR | RTS | OUT2 (OUT2 gates IRQs to the PIC)
            self.interrupt_enable.write(0x01); // received data available -> IRQ4
        }
    }

    fn send(&mut self, byte: u8) {
        if byte == b'\n' {
            self.send_raw(b'\r');
        }
        self.send_raw(byte);
    }

    fn send_raw(&mut self, byte: u8) {
        unsafe {
            // LSR bit 5: transmitter holding register empty
            while self.line_status.read() & 0x20 == 0 {
                core::hint::spin_loop();
            }
            self.data.write(byte);
        }
    }
}

impl core::fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            self.send(byte);
        }
        Ok(())
    }
}

pub fn init() {
    SERIAL1.lock().init();
}

pub fn print(s: &str) {
    use core::fmt::Write;
    let _ = SERIAL1.lock().write_str(s);
}

pub fn print_char(c: char) {
    let mut buf = [0u8; 4];
    print(c.encode_utf8(&mut buf));
}

pub fn print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let _ = SERIAL1.lock().write_fmt(args);
}

/// Best-effort print for exception context: the faulting code may already
/// hold the lock, so skip output instead of spinning forever.
pub fn try_print(s: &str) {
    use core::fmt::Write;
    if let Some(mut port) = SERIAL1.try_lock() {
        let _ = port.write_str(s);
    }
}

/// Recover the serial lock on the panic path: the interrupted context may
/// still hold it and will never resume. Only safe to call with interrupts
/// disabled and when the lock holder cannot run again.
pub unsafe fn force_unlock() {
    SERIAL1.force_unlock();
}

/// Called from the IRQ4 handler: drain every byte the UART has received
/// into the RX queue. Reads the UART only — no TX lock (printing code may
/// hold it), no allocation, no console output (ISR lock rules). An empty
/// FIFO (spurious/no-data IRQ) simply drains nothing.
pub fn service_rx_interrupt() {
    let mut line_status: Port<u8> = Port::new(COM1_BASE + 5);
    let mut data: Port<u8> = Port::new(COM1_BASE);
    loop {
        let lsr: u8 = unsafe { line_status.read() };
        if lsr & 0x01 == 0 {
            return; // data ready (LSR bit 0) clear: FIFO empty
        }
        let byte: u8 = unsafe { data.read() };
        if lsr & 0x1E != 0 {
            continue; // overrun/parity/framing/break: drop the bad byte
        }
        RX_QUEUE.lock().push(byte);
    }
}

/// Called from the main loop. The caller must have interrupts disabled,
/// because the IRQ4 handler takes the same queue lock.
pub fn try_pop_input() -> Option<u8> {
    RX_QUEUE.lock().pop()
}

/// Decode one byte received from COM1 and feed the shell. Runs in the
/// main loop (thread context), never inside an ISR.
pub fn handle_input(byte: u8) {
    let after_cr = LAST_BYTE_WAS_CR.swap(false, Ordering::Relaxed);

    let event = match byte {
        // CR always means Enter; LF means Enter only when it is not the
        // tail of a CRLF pair (the CR already produced one).
        b'\r' => Some(KeyEvent::Enter),
        b'\n' if !after_cr => Some(KeyEvent::Enter),
        0x08 | 0x7F => Some(KeyEvent::Backspace),
        0x03 => Some(KeyEvent::CtrlC),
        0x0C => Some(KeyEvent::CtrlL),
        0x20..=0x7E => Some(KeyEvent::Char(byte as char)),
        _ => None, // other control bytes are ignored
    };

    LAST_BYTE_WAS_CR.store(byte == b'\r', Ordering::Relaxed);

    if let Some(event) = event {
        crate::shell::handle_key_event(event);
    }
}
