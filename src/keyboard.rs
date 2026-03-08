use spin::Mutex;
use lazy_static::lazy_static;

lazy_static! {
    static ref KEYBOARD: Mutex<Keyboard> = Mutex::new(Keyboard::new());
}

pub struct Keyboard {
    buffer: [u8; 256],
    read_pos: usize,
    write_pos: usize,
    shift_pressed: bool,
    ctrl_pressed: bool,
}

impl Keyboard {
    pub fn new() -> Self {
        Keyboard {
            buffer: [0; 256],
            read_pos: 0,
            write_pos: 0,
            shift_pressed: false,
            ctrl_pressed: false,
        }
    }
    
    pub fn process_scancode(&mut self, scancode: u8) -> Option<char> {
        // Handle special keys
        match scancode {
            0x2A | 0x36 => { // Left/Right Shift pressed
                self.shift_pressed = true;
                return None;
            }
            0xAA | 0xB6 => { // Left/Right Shift released
                self.shift_pressed = false;
                return None;
            }
            0x1D => { // Ctrl pressed
                self.ctrl_pressed = true;
                return None;
            }
            0x9D => { // Ctrl released
                self.ctrl_pressed = false;
                return None;
            }
            _ => {}
        }
        
        // Only process key press, not release (bit 7 set = release)
        if scancode & 0x80 != 0 {
            return None;
        }
        
        // Convert scancode to ASCII
        let ascii = self.scancode_to_ascii(scancode);
        ascii.map(|c| {
            if self.shift_pressed {
                self.apply_shift(c)
            } else {
                c
            }
        })
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
            0x0E => Some('\x08'), // Backspace
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
            0x1C => Some('\n'), // Enter
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

pub fn add_scancode(scancode: u8) {
    let mut keyboard = KEYBOARD.lock();
    if let Some(c) = keyboard.process_scancode(scancode) {
        // Pass character to shell
        crate::shell::handle_input(c);
    }
}