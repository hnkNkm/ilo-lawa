use x86_64::instructions::port::Port;
use spin::Mutex;

// Global tick counter for uptime tracking
pub static mut TICKS: u64 = 0;

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

pub static PICS: Mutex<ChainedPics> = Mutex::new(unsafe {
    ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET)
});

const CMD_INIT: u8 = 0x11;
const CMD_END_OF_INTERRUPT: u8 = 0x20;
const MODE_8086: u8 = 0x01;

struct Pic {
    offset: u8,
    command: Port<u8>,
    data: Port<u8>,
}

impl Pic {
    unsafe fn end_of_interrupt(&mut self) {
        self.command.write(CMD_END_OF_INTERRUPT);
    }
    
    unsafe fn read_mask(&mut self) -> u8 {
        self.data.read()
    }
    
    unsafe fn write_mask(&mut self, mask: u8) {
        self.data.write(mask);
    }
}

pub struct ChainedPics {
    pics: [Pic; 2],
}

impl ChainedPics {
    pub const unsafe fn new(offset1: u8, offset2: u8) -> ChainedPics {
        ChainedPics {
            pics: [
                Pic {
                    offset: offset1,
                    command: Port::new(0x20),
                    data: Port::new(0x21),
                },
                Pic {
                    offset: offset2,
                    command: Port::new(0xA0),
                    data: Port::new(0xA1),
                },
            ],
        }
    }
    
    pub unsafe fn initialize(&mut self) {
        let mut wait_port: Port<u8> = Port::new(0x80);
        let mut wait = || wait_port.write(0);
        
        // Save masks
        let mask1 = self.pics[0].read_mask();
        let mask2 = self.pics[1].read_mask();
        
        // Start initialization sequence
        self.pics[0].command.write(CMD_INIT);
        wait();
        self.pics[1].command.write(CMD_INIT);
        wait();
        
        // Set offsets
        self.pics[0].data.write(self.pics[0].offset);
        wait();
        self.pics[1].data.write(self.pics[1].offset);
        wait();
        
        // Configure chaining
        self.pics[0].data.write(4);
        wait();
        self.pics[1].data.write(2);
        wait();
        
        // Set mode
        self.pics[0].data.write(MODE_8086);
        wait();
        self.pics[1].data.write(MODE_8086);
        wait();
        
        // Enable keyboard and timer interrupts only
        // 0xFC = 11111100 - enables IRQ0 (timer) and IRQ1 (keyboard)
        self.pics[0].write_mask(0xFC);
        self.pics[1].write_mask(0xFF);
    }
    
    pub unsafe fn notify_end_of_interrupt(&mut self, interrupt_id: u8) {
        if interrupt_id >= self.pics[1].offset {
            self.pics[1].end_of_interrupt();
        }
        self.pics[0].end_of_interrupt();
    }
    
    pub unsafe fn disable(&mut self) {
        self.pics[0].write_mask(0xff);
        self.pics[1].write_mask(0xff);
    }
}