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

    /// Move the text cursor to cell (x, y) without touching the
    /// framebuffer. Caller must ensure y < self.rows.
    fn set_cursor(&mut self, x: usize, y: usize) {
        self.cursor_x = x;
        self.cursor_y = y;
    }

    /// Redraw an input line (prompt + editable buffer) in place without
    /// ANSI escapes: clear `max(previous_cells, prompt.len() +
    /// buffer.len())` cells starting at column 0 of `start_y` by writing
    /// spaces through `write_char` (so wrapping behaves exactly like
    /// normal text), move back to (0, start_y), redraw prompt + buffer,
    /// then park the cursor at buffer offset `cursor_pos`.
    ///
    /// Thread context only. If output scrolled since the line started and
    /// `start_y` is no longer on screen, it is clamped to the last row
    /// rather than corrupting the screen. Limitation: a line long enough
    /// to wrap past the final row can still scroll during the redraw.
    fn redraw_input(
        &mut self,
        prompt: &str,
        buffer: &str,
        cursor_pos: usize,
        start_y: usize,
        previous_cells: usize,
    ) {
        // Degenerate framebuffer (zero-width or zero-height GOP mode):
        // there is nothing to draw and the cursor row/col math below
        // would divide by zero, so do nothing.
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        let start_y = start_y.min(self.rows - 1);
        let cells = previous_cells.max(prompt.len() + buffer.len());

        // Clear whatever the previous render left on those cells.
        self.set_cursor(0, start_y);
        for _ in 0..cells {
            self.write_char(' ');
        }

        // Redraw prompt and buffer from the start of the line.
        self.set_cursor(0, start_y);
        for c in prompt.chars() {
            self.write_char(c);
        }
        for c in buffer.chars() {
            self.write_char(c);
        }

        // Position the logical cursor inside the buffer.
        let offset = prompt.len() + cursor_pos;
        let row = start_y + offset / self.cols;
        self.set_cursor(offset % self.cols, row.min(self.rows.saturating_sub(1)));
    }
    
    fn newline(&mut self) {
        self.cursor_x = 0;
        self.cursor_y += 1;
        
        if self.cursor_y >= self.rows {
            if self.rows == 0 {
                // Display shorter than one text row: nothing to scroll into.
                self.cursor_y = 0;
                return;
            }
            self.scroll();
            self.cursor_y = self.rows - 1;
        }
    }
    
    fn scroll(&mut self) {
        // Shift the framebuffer up by one text row and clear only the newly
        // exposed bottom row, keeping earlier output readable.
        self.gfx.scroll_up(CHAR_HEIGHT + LINE_SPACING, self.bg_color);
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

// All console output is mirrored to COM1. The serial lock is taken and
// released BEFORE the terminal lock (sequential, never nested), and
// serial-first so output emitted before init() still reaches the host.
pub fn print_fmt(args: core::fmt::Arguments) {
    use core::fmt::Write;
    crate::serial::print_fmt(args);
    if let Some(ref mut terminal) = *TERMINAL.lock() {
        let _ = terminal.write_fmt(args);
    }
}

/// Best-effort print for exception context: never spins on the console
/// locks (the interrupted code may hold them), so output may be skipped.
pub fn try_print(s: &str) {
    crate::serial::try_print(s);
    if let Some(mut guard) = TERMINAL.try_lock() {
        if let Some(ref mut terminal) = *guard {
            terminal.write_string(s);
        }
    }
}

/// Recover the terminal lock on the panic path: the interrupted context may
/// still hold it and will never resume. Only safe to call with interrupts
/// disabled and when the lock holder cannot run again.
pub unsafe fn force_unlock() {
    TERMINAL.force_unlock();
}

pub fn print_char(c: char) {
    crate::serial::print_char(c);
    if let Some(ref mut terminal) = *TERMINAL.lock() {
        terminal.write_char(c);
    }
}

pub fn print(s: &str) {
    crate::serial::print(s);
    if let Some(ref mut terminal) = *TERMINAL.lock() {
        terminal.write_string(s);
    }
}

pub fn clear() {
    if let Some(ref mut terminal) = *TERMINAL.lock() {
        terminal.clear_screen();
    }
}

/// Current text row of the terminal cursor (0-based). 0 before init().
/// Thread context only (takes the terminal lock).
pub fn cursor_y() -> usize {
    match &*TERMINAL.lock() {
        Some(terminal) => terminal.cursor_y,
        None => 0,
    }
}

/// Redraw the shell input line in place. See [`Terminal::redraw_input`] for
/// the semantics; thread context only (takes the terminal lock). Redraws
/// are not mirrored to serial — the framebuffer is the source of truth for
/// the edited line.
pub fn redraw_input(
    prompt: &str,
    buffer: &str,
    cursor_pos: usize,
    start_y: usize,
    previous_cells: usize,
) {
    if let Some(terminal) = &mut *TERMINAL.lock() {
        terminal.redraw_input(prompt, buffer, cursor_pos, start_y, previous_cells);
    }
}