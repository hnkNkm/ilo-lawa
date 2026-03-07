#![allow(dead_code)]

use core::arch::asm;

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
    
    pub fn draw_text(&self, x: usize, y: usize, text: &str, color: u32) {
        // Simple 8x8 block characters for now
        let mut cx = x;
        for _ch in text.chars() {
            self.draw_rect(cx, y, 6, 8, color);
            cx += 8;
        }
    }
}

// The actual kernel entry point after ExitBootServices
pub fn kernel_main(fb_info: FramebufferInfo) -> ! {
    // We are now running without UEFI!
    let gfx = KernelGraphics::new(fb_info);
    
    // Clear screen to dark blue
    gfx.clear(0x001122);
    
    // Draw UI
    gfx.draw_rect(0, 0, fb_info.width, 30, 0x003366);  // Title bar
    gfx.draw_text(10, 10, "ilo-lawa Kernel - Running independently!", 0xFFFFFF);
    
    // Draw some shapes to show we're working
    gfx.draw_rect(50, 100, 200, 150, 0x884422);   // Brown
    gfx.draw_rect(300, 100, 200, 150, 0x228844);  // Green
    gfx.draw_rect(550, 100, 200, 150, 0x224488);  // Blue
    
    // Status message
    gfx.draw_text(50, 300, "UEFI ExitBootServices completed successfully", 0x00FF00);
    gfx.draw_text(50, 320, "Kernel is running independently", 0x00FF00);
    gfx.draw_text(50, 340, "No UEFI services available", 0xFFFF00);
    
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