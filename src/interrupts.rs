use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use lazy_static::lazy_static;
use crate::kernel::KernelGraphics;
use crate::pic;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        
        // CPU exceptions
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault.set_handler_fn(general_protection_fault_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        
        unsafe {
            idt.double_fault.set_handler_fn(double_fault_handler)
                .set_stack_index(crate::gdt::DOUBLE_FAULT_IST_INDEX);
        }
        
        // Hardware interrupts
        idt[InterruptIndex::Timer.as_usize()]
            .set_handler_fn(timer_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_usize()]
            .set_handler_fn(keyboard_interrupt_handler);

        // Every PIC vector needs a present entry: spurious IRQ7/IRQ15 fire
        // even when masked, and a non-present gate would fault (issue #6)
        idt[34].set_handler_fn(irq2_handler);
        idt[35].set_handler_fn(irq3_handler);
        idt[36].set_handler_fn(irq4_handler);
        idt[37].set_handler_fn(irq5_handler);
        idt[38].set_handler_fn(irq6_handler);
        idt[39].set_handler_fn(spurious_irq7_handler);
        idt[40].set_handler_fn(irq8_handler);
        idt[41].set_handler_fn(irq9_handler);
        idt[42].set_handler_fn(irq10_handler);
        idt[43].set_handler_fn(irq11_handler);
        idt[44].set_handler_fn(irq12_handler);
        idt[45].set_handler_fn(irq13_handler);
        idt[46].set_handler_fn(irq14_handler);
        idt[47].set_handler_fn(spurious_irq15_handler);
        idt
    };
}

pub fn init() {
    IDT.load();
}

// Exception handlers must not spin on the console locks: the fault may
// have interrupted code that holds them (issue #17). Fatal exceptions
// panic! directly — the panic handler force-unlocks before printing.
// The resumable breakpoint handler uses best-effort try_print instead.
extern "x86-interrupt" fn breakpoint_handler(_stack_frame: InterruptStackFrame) {
    crate::terminal::try_print("EXCEPTION: BREAKPOINT\n");
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame, 
    _error_code: u64
) -> ! {
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: x86_64::structures::idt::PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;

    panic!(
        "EXCEPTION: PAGE FAULT at {:?} ({:?})\n{:#?}",
        Cr2::read(),
        error_code,
        stack_frame
    );
}

extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    panic!(
        "EXCEPTION: GENERAL PROTECTION FAULT (error code: {})\n{:#?}",
        error_code, stack_frame
    );
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    panic!("EXCEPTION: INVALID OPCODE\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        // Increment tick counter for uptime tracking
        pic::TICKS += 1;
        pic::PICS.lock().notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use x86_64::instructions::port::Port;
    
    // Read scan code from PS/2 data port
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    
    // Process the scan code
    crate::keyboard::add_scancode(scancode);
    
    unsafe {
        pic::PICS.lock().notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}

// extern "x86-interrupt" fns cannot be generic, so expand a concrete fn
// per vector. Handlers must only ack the PIC (see concurrency rules).
macro_rules! pic_eoi_handlers {
    ($($name:ident => $vector:expr),+ $(,)?) => {
        $(
            extern "x86-interrupt" fn $name(_stack_frame: InterruptStackFrame) {
                unsafe {
                    pic::PICS.lock().notify_end_of_interrupt($vector);
                }
            }
        )+
    };
}

pic_eoi_handlers! {
    irq2_handler => 34,
    irq3_handler => 35,
    irq4_handler => 36,
    irq5_handler => 37,
    irq6_handler => 38,
    irq8_handler => 40,
    irq9_handler => 41,
    irq10_handler => 42,
    irq11_handler => 43,
    irq12_handler => 44,
    irq13_handler => 45,
    irq14_handler => 46,
}

extern "x86-interrupt" fn spurious_irq7_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        let mut pics = pic::PICS.lock();
        // Spurious IRQ7: master ISR bit 7 clear means nothing is actually
        // in service, so no EOI must be sent
        if pics.read_isr() & (1 << 7) != 0 {
            pics.notify_end_of_interrupt(39);
        }
    }
}

extern "x86-interrupt" fn spurious_irq15_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        let mut pics = pic::PICS.lock();
        // Spurious IRQ15: slave ISR bit 7 (bit 15 here) clear, but the
        // master's cascade IRQ2 was in service, so EOI the master only
        if pics.read_isr() & (1 << 15) != 0 {
            pics.notify_end_of_interrupt(47);
        } else {
            pics.notify_end_of_interrupt_master();
        }
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