#![allow(dead_code)]

use core::arch::asm;
use crate::font::get_char_bitmap;

// Framebuffer information that survives ExitBootServices
#[derive(Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

// Simple kernel graphics driver
pub struct KernelGraphics {
    fb_info: FramebufferInfo,
}

impl KernelGraphics {
    pub fn new(fb_info: FramebufferInfo) -> Self {
        KernelGraphics { fb_info }
    }
    
    pub fn clear(&self, color: u32) {
        unsafe {
            let fb = self.fb_info.base_address as *mut u32;
            for i in 0..(self.fb_info.height * self.fb_info.stride) {
                *fb.add(i) = color;
            }
        }
    }
    
    pub fn draw_pixel(&self, x: usize, y: usize, color: u32) {
        if x >= self.fb_info.width || y >= self.fb_info.height {
            return;
        }
        
        unsafe {
            let fb = self.fb_info.base_address as *mut u32;
            let offset = y * self.fb_info.stride + x;
            *fb.add(offset) = color;
        }
    }
    
    pub fn draw_rect(&self, x: usize, y: usize, width: usize, height: usize, color: u32) {
        for dy in 0..height {
            for dx in 0..width {
                self.draw_pixel(x + dx, y + dy, color);
            }
        }
    }
    
    pub fn draw_char(&self, x: usize, y: usize, ch: char, color: u32, bg_color: Option<u32>) {
        let bitmap = get_char_bitmap(ch);
        
        for (row_idx, &row_data) in bitmap.iter().enumerate() {
            for bit in 0..8 {
                let pixel_x = x + bit;
                let pixel_y = y + row_idx;
                
                if pixel_x >= self.fb_info.width || pixel_y >= self.fb_info.height {
                    continue;
                }
                
                // Check if bit is set (pixel should be drawn)
                // Font data uses bit 0 as leftmost pixel, so use (1 << bit)
                if row_data & (1 << bit) != 0 {
                    self.draw_pixel(pixel_x, pixel_y, color);
                } else if let Some(bg) = bg_color {
                    self.draw_pixel(pixel_x, pixel_y, bg);
                }
            }
        }
    }
    
    pub fn draw_text(&self, x: usize, y: usize, text: &str, color: u32, bg_color: Option<u32>) {
        let mut cx = x;
        for ch in text.chars() {
            if ch == '\n' {
                return; // Simple newline handling
            }
            self.draw_char(cx, y, ch, color, bg_color);
            cx += 8; // Move to next character position (8 pixels)
        }
    }
}

// The actual kernel entry point after ExitBootServices
pub fn kernel_main(fb_info: FramebufferInfo) -> ! {
    // We are now running without UEFI!
    let gfx = KernelGraphics::new(fb_info);
    
    // Clear screen to dark background
    gfx.clear(0x0A0A0A);
    
    // Draw title bar
    gfx.draw_rect(0, 0, fb_info.width, 40, 0x1A1A2E);
    gfx.draw_text(10, 10, "ilo-lawa OS v0.2.0 - Independent Kernel Mode", 0xFFFFFF, None);
    
    // Draw welcome message with proper font rendering
    gfx.draw_text(10, 60, "Welcome to ilo-lawa OS!", 0x00FF00, None);
    gfx.draw_text(10, 80, "================================", 0x00FF00, None);
    
    // System status
    gfx.draw_text(10, 110, "System Status:", 0xFFFF00, None);
    gfx.draw_text(10, 130, "[OK] UEFI ExitBootServices completed", 0x00FF00, None);
    gfx.draw_text(10, 150, "[OK] Framebuffer initialized", 0x00FF00, None);
    gfx.draw_text(10, 170, "[OK] Font rendering system active", 0x00FF00, None);
    gfx.draw_text(10, 190, "[OK] Running in kernel mode", 0x00FF00, None);
    
    // Test different characters
    gfx.draw_text(10, 230, "Character Test:", 0xFFFF00, None);
    gfx.draw_text(10, 250, "ABCDEFGHIJKLMNOPQRSTUVWXYZ", 0xFFFFFF, None);
    gfx.draw_text(10, 270, "abcdefghijklmnopqrstuvwxyz", 0xFFFFFF, None);
    gfx.draw_text(10, 290, "0123456789 !@#$%^&*()_+-=", 0xFFFFFF, None);
    gfx.draw_text(10, 310, "[]{}\\|;:'\",.<>/?`~", 0xFFFFFF, None);
    
    // Footer
    gfx.draw_text(10, 350, "Press any key to continue... (not implemented yet)", 0x808080, None);
    
    // Halt the CPU
    halt_loop();
}

pub fn halt_loop() -> ! {
    loop {
        unsafe {
            asm!("hlt");
        }
    }
}