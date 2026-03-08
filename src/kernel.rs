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
    // CRITICAL: Disable interrupts immediately after ExitBootServices
    x86_64::instructions::interrupts::disable();
    
    // Initialize terminal first
    crate::terminal::init(fb_info);
    
    // Clear screen and show boot messages
    crate::terminal::print("ilo-lawa OS v0.4.0\n");
    crate::terminal::print("===================\n\n");
    crate::terminal::print("System Initialization:\n");
    crate::terminal::print("[OK] Interrupts disabled\n");
    
    // Initialize heap allocator
    crate::terminal::print("Initializing heap allocator...");
    crate::allocator::init_heap();
    crate::terminal::print(" [OK]\n");
    
    // Initialize CPU features (FPU, SSE)
    crate::terminal::print("Initializing CPU features...");
    crate::cpu::init();
    crate::terminal::print(" [OK]\n");
    
    // Initialize GDT
    crate::terminal::print("Initializing GDT...");
    crate::gdt::init();
    crate::terminal::print(" [OK]\n");
    
    // Initialize IDT
    crate::terminal::print("Initializing IDT...");
    crate::interrupts::init();
    crate::terminal::print(" [OK]\n");
    
    // Initialize PIC
    crate::terminal::print("Initializing PIC...");
    unsafe { crate::pic::PICS.lock().initialize() };
    crate::terminal::print(" [OK]\n");
    
    crate::terminal::print("\nInterrupt system ready.\n");
    crate::terminal::print("Enabling interrupts...");
    
    // Enable interrupts
    x86_64::instructions::interrupts::enable();
    crate::terminal::print(" [OK]\n");
    
    // Initialize filesystem
    crate::terminal::print("Initializing filesystem...");
    crate::fs::init();
    crate::terminal::print(" [OK]\n");
    
    // Initialize shell
    crate::terminal::print("Starting shell...");
    crate::shell::init();
    crate::terminal::print(" [OK]\n");
    
    // Main kernel loop
    loop {
        x86_64::instructions::hlt();
    }
}

pub fn halt_loop() -> ! {
    loop {
        unsafe {
            asm!("hlt");
        }
    }
}