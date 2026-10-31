use spin::Mutex;
use lazy_static::lazy_static;

const QUEUE_SIZE: usize = 256;

// Scancodes are pushed here by the keyboard ISR and drained by the main
// loop. This is the ONLY lock shared between interrupt and thread context;
// the main loop must hold it with interrupts disabled.
struct ScancodeQueue {
    buffer: [u8; QUEUE_SIZE],
    read_pos: usize,
    write_pos: usize,
}

impl ScancodeQueue {
    const fn new() -> Self {
        ScancodeQueue {
            buffer: [0; QUEUE_SIZE],
            read_pos: 0,
            write_pos: 0,
        }
    }

    fn push(&mut self, scancode: u8) {
        let next = (self.write_pos + 1) % QUEUE_SIZE;
        if next != self.read_pos {
            self.buffer[self.write_pos] = scancode;
            self.write_pos = next;
        }
        // Queue full: drop the scancode rather than block in the ISR
    }

    fn pop(&mut self) -> Option<u8> {
        if self.read_pos == self.write_pos {
            None
        } else {
            let scancode = self.buffer[self.read_pos];
            self.read_pos = (self.read_pos + 1) % QUEUE_SIZE;
            Some(scancode)
        }
    }
}

static SCANCODE_QUEUE: Mutex<ScancodeQueue> = Mutex::new(ScancodeQueue::new());

lazy_static! {
    static ref KEYBOARD: Mutex<Keyboard> = Mutex::new(Keyboard::new());
}

/// A decoded key event consumed by the shell. Chars carry the Shift-applied
/// ASCII character; everything else is a control/navigation action.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum KeyEvent {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    CtrlC,
    CtrlL,
}

pub struct Keyboard {
    shift_pressed: bool,
    ctrl_pressed: bool,
    /// Set by an 0xE0 prefix: the next scancode is decoded in extended
    /// context (arrow/Home/End/Delete keys, keypad Enter, right Ctrl, fake
    /// shifts) and the flag clears afterwards.
    extended: bool,
}

impl Keyboard {
    pub fn new() -> Self {
        Keyboard {
            shift_pressed: false,
            ctrl_pressed: false,
            extended: false,
        }
    }

    /// Decode one scancode (set 1, including 0xE0-prefixed extended codes)
    /// into an optional KeyEvent.
    pub fn process_scancode(&mut self, scancode: u8) -> Option<KeyEvent> {
        if scancode == 0xE0 {
            // Extended prefix: only meaningful together with the next code.
            self.extended = true;
            return None;
        }
        // Consume the prefix flag before decoding; every path below leaves
        // it cleared so a lost pair can never corrupt later normal keys.
        let extended = core::mem::take(&mut self.extended);

        if extended {
            return self.process_extended(scancode);
        }

        match scancode {
            0x2A | 0x36 => { // Left/Right Shift pressed
                self.shift_pressed = true;
                None
            }
            0xAA | 0xB6 => { // Left/Right Shift released
                self.shift_pressed = false;
                None
            }
            0x1D => { // Left Ctrl pressed
                self.ctrl_pressed = true;
                None
            }
            0x9D => { // Left Ctrl released
                self.ctrl_pressed = false;
                None
            }
            // Only process key presses, not releases (bit 7 set = release).
            _ if scancode & 0x80 != 0 => None,
            0x1C => Some(KeyEvent::Enter),
            0x0E => Some(KeyEvent::Backspace),
            // Keypad navigation cluster. Caveat: with NumLock on these are
            // the arrow/Home/End/Delete codes; with NumLock off real
            // keyboards send digits here. We accept them as navigation
            // unconditionally, which matches QEMU's default (NumLock on).
            0x47 => Some(KeyEvent::Home),
            0x48 => Some(KeyEvent::Up),
            0x4B => Some(KeyEvent::Left),
            0x4D => Some(KeyEvent::Right),
            0x4F => Some(KeyEvent::End),
            0x50 => Some(KeyEvent::Down),
            0x53 => Some(KeyEvent::Delete),
            _ => self.scancode_to_ascii(scancode).and_then(|c| self.char_event(c)),
        }
    }

    /// Decode an 0xE0-prefixed scancode (the prefix flag already consumed).
    fn process_extended(&mut self, scancode: u8) -> Option<KeyEvent> {
        match scancode {
            // Fake shifts emitted around extended keys (2A/36 before, AA/B6
            // after): ignore them entirely so they never corrupt the real
            // shift state.
            0x2A | 0x36 | 0xAA | 0xB6 => None,
            0x1D => { // Right Ctrl pressed
                self.ctrl_pressed = true;
                None
            }
            0x9D => { // Right Ctrl released
                self.ctrl_pressed = false;
                None
            }
            0x48 => Some(KeyEvent::Up),
            0x50 => Some(KeyEvent::Down),
            0x4B => Some(KeyEvent::Left),
            0x4D => Some(KeyEvent::Right),
            0x47 => Some(KeyEvent::Home),
            0x4F => Some(KeyEvent::End),
            0x53 => Some(KeyEvent::Delete),
            0x1C => Some(KeyEvent::Enter), // keypad Enter
            // Everything else — in particular the break codes of the
            // navigation keys (E0 C8/D0/CB/CD/C7/CF/D3) and keypad Enter
            // release (E0 9C) — produces no event.
            _ => None,
        }
    }

    /// Wrap a base ASCII char into an event, applying Shift and resolving
    /// Ctrl combos. Ctrl+<char> yields an event only for C and L; Ctrl with
    /// any other character produces no event.
    fn char_event(&self, c: char) -> Option<KeyEvent> {
        let c = if self.shift_pressed {
            self.apply_shift(c)
        } else {
            c
        };
        if self.ctrl_pressed {
            return match c.to_ascii_lowercase() {
                'c' => Some(KeyEvent::CtrlC),
                'l' => Some(KeyEvent::CtrlL),
                _ => None,
            };
        }
        Some(KeyEvent::Char(c))
    }
    
    fn scancode_to_ascii(&self, scancode: u8) -> Option<char> {
        // US QWERTY keyboard layout (scancode set 1)
        match scancode {
            0x01 => None, // ESC
            0x02 => Some('1'),
            0x03 => Some('2'),
            0x04 => Some('3'),
            0x05 => Some('4'),
            0x06 => Some('5'),
            0x07 => Some('6'),
            0x08 => Some('7'),
            0x09 => Some('8'),
            0x0A => Some('9'),
            0x0B => Some('0'),
            0x0C => Some('-'),
            0x0D => Some('='),
            0x0E => None, // Backspace — decoded as KeyEvent::Backspace above
            0x0F => Some('\t'), // Tab
            0x10 => Some('q'),
            0x11 => Some('w'),
            0x12 => Some('e'),
            0x13 => Some('r'),
            0x14 => Some('t'),
            0x15 => Some('y'),
            0x16 => Some('u'),
            0x17 => Some('i'),
            0x18 => Some('o'),
            0x19 => Some('p'),
            0x1A => Some('['),
            0x1B => Some(']'),
            0x1C => None, // Enter — decoded as KeyEvent::Enter above
            0x1E => Some('a'),
            0x1F => Some('s'),
            0x20 => Some('d'),
            0x21 => Some('f'),
            0x22 => Some('g'),
            0x23 => Some('h'),
            0x24 => Some('j'),
            0x25 => Some('k'),
            0x26 => Some('l'),
            0x27 => Some(';'),
            0x28 => Some('\''),
            0x29 => Some('`'),
            0x2B => Some('\\'),
            0x2C => Some('z'),
            0x2D => Some('x'),
            0x2E => Some('c'),
            0x2F => Some('v'),
            0x30 => Some('b'),
            0x31 => Some('n'),
            0x32 => Some('m'),
            0x33 => Some(','),
            0x34 => Some('.'),
            0x35 => Some('/'),
            0x39 => Some(' '), // Space
            _ => None,
        }
    }
    
    fn apply_shift(&self, c: char) -> char {
        match c {
            '1' => '!',
            '2' => '@',
            '3' => '#',
            '4' => '$',
            '5' => '%',
            '6' => '^',
            '7' => '&',
            '8' => '*',
            '9' => '(',
            '0' => ')',
            '-' => '_',
            '=' => '+',
            '[' => '{',
            ']' => '}',
            ';' => ':',
            '\'' => '"',
            '`' => '~',
            '\\' => '|',
            ',' => '<',
            '.' => '>',
            '/' => '?',
            'a'..='z' => (c as u8 - 32) as char, // Convert to uppercase
            _ => c,
        }
    }
}

/// Called from the keyboard interrupt handler: enqueue only, no decoding.
pub fn add_scancode(scancode: u8) {
    SCANCODE_QUEUE.lock().push(scancode);
}

/// Called from the main loop. The caller must have interrupts disabled,
/// because the keyboard ISR takes the same queue lock.
pub fn try_pop_scancode() -> Option<u8> {
    SCANCODE_QUEUE.lock().pop()
}

/// Decode a scancode and feed the resulting key event to the shell.
/// Runs in normal thread context, never inside an ISR.
pub fn handle_scancode(scancode: u8) {
    let event = KEYBOARD.lock().process_scancode(scancode);
    if let Some(event) = event {
        crate::shell::handle_key_event(event);
    }
}