use spin::Mutex;
use x86_64::instructions::port::Port;

const COM1_BASE: u16 = 0x3F8;

// Polled 16550 UART on COM1. This lock is for thread context and the
// panic path ONLY — ISRs must never take it (see terminal.rs / bug #2:
// an IRQ arriving while the main loop holds it would deadlock).
static SERIAL1: Mutex<SerialPort> = Mutex::new(SerialPort::new(COM1_BASE));

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
            self.interrupt_enable.write(0x00); // no UART interrupts, we poll
            self.line_control.write(0x80); // DLAB set: next writes hit divisor latch
            self.data.write(0x03); // divisor low (115200 / 3 = 38400 baud)
            self.interrupt_enable.write(0x00); // divisor high
            self.line_control.write(0x03); // 8N1, DLAB clear
            self.fifo_control.write(0xC7); // enable + clear FIFOs, 14-byte trigger
            self.modem_control.write(0x0B); // DTR | RTS | OUT2
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
