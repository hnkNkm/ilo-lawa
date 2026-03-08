// PCI and VirtIO device detection

use crate::drivers::virtio;
use x86_64::instructions::port::{Port, PortReadOnly, PortWriteOnly};

// QEMU virtio-mmio device addresses for x86_64
// These are standard addresses used by QEMU when virtio-mmio devices are specified
const VIRTIO_MMIO_BASE: usize = 0xfeb00000;
const VIRTIO_MMIO_SIZE: usize = 0x200;
const VIRTIO_MMIO_MAX_DEVICES: usize = 8;

// VirtIO device IDs
const VIRTIO_ID_BLOCK: u32 = 2;

// PCI configuration space ports
const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

// PCI vendor and device IDs
const PCI_VENDOR_ID_REDHAT: u16 = 0x1AF4;
const PCI_DEVICE_ID_VIRTIO_BLOCK_TRANSITIONAL: u16 = 0x1001;
const PCI_DEVICE_ID_VIRTIO_BLOCK: u16 = 0x1042;

/// Probe for VirtIO MMIO devices at known addresses
pub unsafe fn probe_virtio_mmio_devices() -> Result<(), &'static str> {
    crate::terminal::print("Probing for VirtIO MMIO devices...\n");
    
    for i in 0..VIRTIO_MMIO_MAX_DEVICES {
        let addr = VIRTIO_MMIO_BASE + (i * VIRTIO_MMIO_SIZE);
        
        // Check magic value at offset 0x000
        let magic = core::ptr::read_volatile(addr as *const u32);
        if magic != 0x74726976 { // "virt" in little-endian
            continue;
        }
        
        // Check version at offset 0x004
        let version = core::ptr::read_volatile((addr + 0x004) as *const u32);
        if version != 2 { // We only support version 2
            continue;
        }
        
        // Check device ID at offset 0x008
        let device_id = core::ptr::read_volatile((addr + 0x008) as *const u32);
        
        match device_id {
            VIRTIO_ID_BLOCK => {
                crate::terminal::print("Found VirtIO block device at 0x");
                print_hex(addr);
                crate::terminal::print("\n");
                
                // Initialize the block device
                match virtio::init_virtio_block(addr) {
                    Ok(_) => {
                        crate::terminal::print("VirtIO block device initialized successfully\n");
                        return Ok(());
                    }
                    Err(e) => {
                        crate::terminal::print("Failed to initialize VirtIO block device: ");
                        crate::terminal::print(e);
                        crate::terminal::print("\n");
                    }
                }
            }
            0 => {}, // No device
            _ => {
                crate::terminal::print("Found unknown VirtIO device with ID ");
                print_dec(device_id);
                crate::terminal::print("\n");
            }
        }
    }
    
    Err("No VirtIO block devices found")
}

fn print_hex(val: usize) {
    let hex_chars = b"0123456789abcdef";
    let mut buffer = [0u8; 16];
    let mut pos = 15;
    let mut n = val;
    
    loop {
        buffer[pos] = hex_chars[n & 0xf];
        n >>= 4;
        if n == 0 || pos == 0 {
            break;
        }
        pos -= 1;
    }
    
    for i in pos..16 {
        crate::terminal::print_char(buffer[i] as char);
    }
}

fn print_dec(val: u32) {
    if val == 0 {
        crate::terminal::print("0");
        return;
    }
    
    let mut buffer = [0u8; 10];
    let mut pos = 9;
    let mut n = val;
    
    while n > 0 && pos > 0 {
        buffer[pos] = b'0' + (n % 10) as u8;
        n /= 10;
        pos -= 1;
    }
    
    for i in (pos + 1)..10 {
        crate::terminal::print_char(buffer[i] as char);
    }
}

/// Read PCI configuration space
unsafe fn pci_config_read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let address = (1u32 << 31) // Enable bit
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC);
    
    let mut addr_port = Port::<u32>::new(PCI_CONFIG_ADDRESS);
    let mut data_port = PortReadOnly::<u32>::new(PCI_CONFIG_DATA);
    
    addr_port.write(address);
    data_port.read()
}

/// Probe for PCI VirtIO devices
pub unsafe fn probe_virtio_pci_devices() -> Result<(), &'static str> {
    crate::terminal::print("Scanning PCI bus for VirtIO devices...\n");
    
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let vendor_device = pci_config_read_u32(bus, device, 0, 0);
            let vendor_id = (vendor_device & 0xFFFF) as u16;
            let device_id = ((vendor_device >> 16) & 0xFFFF) as u16;
            
            // Skip non-existent devices
            if vendor_id == 0xFFFF {
                continue;
            }
            
            // Check for VirtIO PCI devices (Red Hat vendor ID)
            if vendor_id == PCI_VENDOR_ID_REDHAT {
                match device_id {
                    PCI_DEVICE_ID_VIRTIO_BLOCK | PCI_DEVICE_ID_VIRTIO_BLOCK_TRANSITIONAL => {
                        crate::terminal::print("Found VirtIO block device on PCI ");
                        print_dec(bus as u32);
                        crate::terminal::print(":");
                        print_dec(device as u32);
                        crate::terminal::print(":0\n");
                        
                        // Get BAR0 (base address register)
                        let bar0 = pci_config_read_u32(bus, device, 0, 0x10);
                        
                        // Check if it's MMIO (bit 0 = 0) or I/O (bit 0 = 1)
                        if bar0 & 1 == 0 {
                            // Memory mapped I/O
                            let addr = (bar0 & 0xFFFFFFF0) as usize;
                            crate::terminal::print("  BAR0 (MMIO): 0x");
                            print_hex(addr);
                            crate::terminal::print("\n");
                            
                            // Try to initialize the device
                            // Note: This address needs to be mapped properly
                            // For now, we'll just report it
                            crate::terminal::print("  Note: PCI device found but initialization not yet implemented\n");
                        }
                        
                        return Ok(());
                    }
                    _ => {
                        crate::terminal::print("Found unknown VirtIO device: 0x");
                        print_hex(device_id as usize);
                        crate::terminal::print("\n");
                    }
                }
            }
        }
    }
    
    Err("No VirtIO PCI devices found")
}