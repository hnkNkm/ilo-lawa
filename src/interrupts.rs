use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use lazy_static::lazy_static;
use crate::kernel::KernelGraphics;
use crate::pic;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        unsafe {
            idt.double_fault.set_handler_fn(double_fault_handler)
                .set_stack_index(crate::gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt[InterruptIndex::Timer.as_usize()]
            .set_handler_fn(timer_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_usize()]
            .set_handler_fn(keyboard_interrupt_handler);
        idt
    };
}

pub fn init() {
    IDT.load();
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    // Breakpoint exception handler
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame, 
    _error_code: u64
) -> ! {
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        pic::PICS.lock().notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use x86_64::instructions::port::Port;
    
    // Read scan code from PS/2 data port
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    
    // Debug: Show raw scancode
    crate::terminal::print("[");
    let hex_chars = "0123456789ABCDEF";
    let high = ((scancode >> 4) & 0xF) as usize;
    let low = (scancode & 0xF) as usize;
    crate::terminal::print(&hex_chars[high..high+1]);
    crate::terminal::print(&hex_chars[low..low+1]);
    crate::terminal::print("] ");
    
    // Process the scan code
    crate::keyboard::add_scancode(scancode);
    
    unsafe {
        pic::PICS.lock().notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = pic::PIC_1_OFFSET,
    Keyboard = pic::PIC_1_OFFSET + 1,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
    
    fn as_usize(self) -> usize {
        usize::from(self.as_u8())
    }
}