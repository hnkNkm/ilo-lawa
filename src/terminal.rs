use spin::Mutex;
use lazy_static::lazy_static;
use crate::kernel::{FramebufferInfo, KernelGraphics};

const CHAR_WIDTH: usize = 8;
const CHAR_HEIGHT: usize = 8;
const LINE_SPACING: usize = 2;

lazy_static! {
    static ref TERMINAL: Mutex<Option<Terminal>> = Mutex::new(None);
}

pub struct Terminal {
    gfx: KernelGraphics,
    cursor_x: usize,
    cursor_y: usize,
    cols: usize,
    rows: usize,
    fg_color: u32,
    bg_color: u32,
}

impl Terminal {
    pub fn new(fb_info: FramebufferInfo) -> Self {
        let gfx = KernelGraphics::new(fb_info);
        let cols = fb_info.width / CHAR_WIDTH;
        let rows = fb_info.height / (CHAR_HEIGHT + LINE_SPACING);
        
        // Clear the screen first
        gfx.clear(0x000000);
        
        Terminal {
            gfx,
            cursor_x: 0,
            cursor_y: 0,
            cols,
            rows,
            fg_color: 0x00FF00, // Green
            bg_color: 0x000000, // Black
        }
    }
    
    pub fn write_char(&mut self, c: char) {
        match c {
            '\n' => {
                self.newline();
            }
            '\r' => { // Carriage return
                self.cursor_x = 0;
            }
            '\x08' => { // Backspace - Terminal level handling (not used by shell)
                if self.cursor_x > 0 {
                    self.cursor_x -= 1;
                    self.draw_char_at_cursor(' ');
                    // Keep cursor at the backspaced position
                }
            }
            '\t' => {
                // Tab: advance to next multiple of 4
                let spaces = 4 - (self.cursor_x % 4);
                for _ in 0..spaces {
                    if self.cursor_x >= self.cols {
                        break;
                    }
                    self.write_char(' ');
                }
            }
            _ => {
                self.draw_char_at_cursor(c);
                self.cursor_x += 1;
                
                if self.cursor_x >= self.cols {
                    self.newline();
                }
            }
        }
        
        // Draw cursor (temporarily disabled for debugging)
        // self.draw_cursor();
    }
    
    fn draw_char_at_cursor(&mut self, c: char) {
        let x = self.cursor_x * CHAR_WIDTH;
        let y = self.cursor_y * (CHAR_HEIGHT + LINE_SPACING);
        self.gfx.draw_char(x, y, c, self.fg_color, Some(self.bg_color));
    }
    
    fn newline(&mut self) {
        self.cursor_x = 0;
        self.cursor_y += 1;
        
        if self.cursor_y >= self.rows {
            self.scroll();
            self.cursor_y = self.rows - 1;
        }
    }
    
    fn scroll(&mut self) {
        // For now, just clear the screen when we need to scroll
        // TODO: Implement proper scrolling by copying framebuffer memory
        self.gfx.clear(self.bg_color);
        self.cursor_x = 0;
        self.cursor_y = 0;
    }
    
    fn draw_cursor(&mut self) {
        // Draw a blinking cursor (for now just a solid block)
        let x = self.cursor_x * CHAR_WIDTH;
        let y = self.cursor_y * (CHAR_HEIGHT + LINE_SPACING);
        self.gfx.draw_rect(x, y, CHAR_WIDTH, CHAR_HEIGHT, 0x808080);
    }
    
    pub fn write_string(&mut self, s: &str) {
        for c in s.chars() {
            self.write_char(c);
        }
    }
    
    pub fn clear_screen(&mut self) {
        self.gfx.clear(self.bg_color);
        self.cursor_x = 0;
        self.cursor_y = 0;
    }
}

impl core::fmt::Write for Terminal {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

pub fn init(fb_info: FramebufferInfo) {
    let terminal = Terminal::new(fb_info);
    *TERMINAL.lock() = Some(terminal);
}

pub fn print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;
    if let Some(ref mut terminal) = *TERMINAL.lock() {
        let _ = terminal.write_fmt(args);
    }
}

/// Recover the terminal lock on the panic path: the interrupted context may
/// still hold it and will never resume. Only safe to call with interrupts
/// disabled and when the lock holder cannot run again.
pub unsafe fn force_unlock() {
    TERMINAL.force_unlock();
}

pub fn print_char(c: char) {
    if let Some(ref mut terminal) = *TERMINAL.lock() {
        terminal.write_char(c);
    }
}

pub fn print(s: &str) {
    if let Some(ref mut terminal) = *TERMINAL.lock() {
        terminal.write_string(s);
    }
}

pub fn clear() {
    if let Some(ref mut terminal) = *TERMINAL.lock() {
        terminal.clear_screen();
    }
}