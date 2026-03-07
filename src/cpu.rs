// CPU initialization and control register management

use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

pub fn init() {
    enable_sse();
    enable_fpu();
}

// Enable SSE (Streaming SIMD Extensions)
fn enable_sse() {
    unsafe {
        // Enable OSFXSR and OSXMMEXCPT in CR4
        let mut cr4 = Cr4::read();
        cr4 |= Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE;
        Cr4::write(cr4);
    }
}

// Enable FPU (Floating Point Unit) 
fn enable_fpu() {
    unsafe {
        // Clear EM (Emulation) bit and set MP (Monitor Coprocessor) bit
        let mut cr0 = Cr0::read();
        cr0.remove(Cr0Flags::EMULATE_COPROCESSOR);
        cr0.insert(Cr0Flags::MONITOR_COPROCESSOR);
        Cr0::write(cr0);
        
        // Initialize FPU with FNINIT instruction
        core::arch::asm!("fninit");
    }
}