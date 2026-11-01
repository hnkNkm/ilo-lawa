//! PCI bus enumeration and configuration-space access via the legacy
//! 0xCF8/0xCFC mechanism (no MMCONFIG/ECAM; that needs ACPI MCFG and is
//! future work). This is the discovery layer for virtio-blk (issue #14),
//! NIC, NVMe and xHCI drivers. Enumeration is read-only; the only
//! configuration-space *write* is [`enable_device`], which flips the
//! command-register bits a driver needs (I/O + memory decode, bus
//! mastering for DMA). BAR mapping stays with the device drivers: the
//! legacy virtio transport uses an I/O-port BAR and needs no page-table
//! work at all.
//!
//! Locking: `PCI_BUS` is thread-context only (kernel init and the `lspci`
//! shell command both run in the main loop), exactly like `SHELL` and
//! `FILESYSTEM`. Never take it from an interrupt handler.
//!
//! The device table is a fixed static array, not a `Vec`: enumeration must
//! be safe before the heap exists (it only needs port I/O), and this way
//! init stays allocation-free whichever init order future kernels pick.

use spin::Mutex;
use x86_64::instructions::port::Port;

// Legacy PCI configuration access: write the BDF address dword to
// CONFIG_ADDRESS, then read/write the dword window at CONFIG_DATA.
const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

// Fixed capacity of the discovered-device table.
pub const MAX_DEVICES: usize = 64;

// virtio PCI identifiers (vendor 0x1AF4 "Red Hat, Inc."). 0x1001 is the
// transitional virtio-blk device (legacy I/O-bar layout); 0x1042 is the
// modern (virtio 1.0, non-transitional) block device.
const VIRTIO_VENDOR_ID: u16 = 0x1AF4;
const VIRTIO_BLK_LEGACY_DEVICE_ID: u16 = 0x1001;
const VIRTIO_BLK_MODERN_DEVICE_ID: u16 = 0x1042;

// Header type bit 7 marks a multifunction device; only then do functions
// 1..=7 exist.
const MULTIFUNCTION_BIT: u8 = 0x80;

// Configuration-space reads for absent slots return vendor 0xFFFF.
const NO_DEVICE: u16 = 0xFFFF;

/// One discovered PCI function (bus/device/function triple plus the
/// header fields drivers care about). Raw values only — interpretation
/// (BAR sizing, capability parsing) belongs to the future device drivers.
#[derive(Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    /// Raw header-type byte; bit 7 is the multifunction marker, low bits
    /// the layout (0x00 = generic, 0x01 = PCI-to-PCI bridge, ...).
    pub header_type: u8,
    /// Raw BAR0..BAR5 dwords. Bit 0 set = 32-bit I/O-port BAR, otherwise a
    /// memory BAR (low 4 bits are flags). 64-bit BARs occupy two slots;
    /// the upper half is stored raw in the next slot.
    pub bars: [u32; 6],
    /// IRQ line the device will use (PIC vector offset), 0xFF if unrouted.
    pub interrupt_line: u8,
    /// Interrupt pin A..D as 1..=4, 0 = none.
    pub interrupt_pin: u8,
}

impl PciDevice {
    /// virtio-blk, legacy/transitional (0x1001) or modern (0x1042).
    pub fn is_virtio_block(&self) -> bool {
        self.vendor_id == VIRTIO_VENDOR_ID
            && (self.device_id == VIRTIO_BLK_LEGACY_DEVICE_ID
                || self.device_id == VIRTIO_BLK_MODERN_DEVICE_ID)
    }

    /// True if BAR `index` maps I/O ports rather than memory.
    pub fn bar_is_io(&self, index: usize) -> bool {
        self.bars[index] & 0x1 == 0x1
    }

    /// True if BAR `index` is the lower half of a 64-bit memory BAR
    /// (bits 2:1 == 0b10); the upper 32 bits live in the next slot.
    pub fn bar_is_64bit(&self, index: usize) -> bool {
        !self.bar_is_io(index) && self.bars[index] & 0x6 == 0x4
    }

    /// Base address of BAR `index` with the flag bits stripped (I/O BARs
    /// keep bit 1..=31, memory BARs bit 4..=31). 64-bit BARs: the low
    /// dword's base only; join with the next slot when needed.
    pub fn bar_base(&self, index: usize) -> u32 {
        if self.bar_is_io(index) {
            self.bars[index] & 0xFFFF_FFFC
        } else {
            self.bars[index] & 0xFFFF_FFF0
        }
    }
}

// Discovered devices plus counters, behind the thread-context-only lock.
struct PciBus {
    devices: [Option<PciDevice>; MAX_DEVICES],
    count: usize,
    /// Set when enumeration found more than MAX_DEVICES functions and the
    /// rest were dropped.
    overflow: bool,
}

static PCI_BUS: Mutex<PciBus> = Mutex::new(PciBus {
    devices: [None; MAX_DEVICES],
    count: 0,
    overflow: false,
});

/// Encode the CONFIG_ADDRESS dword: enable bit 31, then bus (8 bits),
/// device (5), function (3) and the 4-byte-aligned register offset.
fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | (u32::from(bus) << 16)
        | (u32::from(device) << 11)
        | (u32::from(function) << 8)
        | u32::from(offset & 0xFC)
}

/// Read one aligned dword of configuration space. The only unsafe in this
/// module is these two port accesses.
fn config_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let mut address = Port::new(CONFIG_ADDRESS);
    let mut data = Port::new(CONFIG_DATA);
    unsafe {
        address.write(config_address(bus, device, function, offset));
        data.read()
    }
}

// Command register (offset 0x04) bits a driver needs before it may touch
// the device: BAR decode enable and DMA permission.
const COMMAND_IO_SPACE: u32 = 1 << 0;
const COMMAND_MEMORY_SPACE: u32 = 1 << 1;
const COMMAND_BUS_MASTER: u32 = 1 << 2;

/// Write one aligned dword of configuration space.
fn config_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let mut address = Port::new(CONFIG_ADDRESS);
    let mut data = Port::new(CONFIG_DATA);
    unsafe {
        address.write(config_address(bus, device, function, offset));
        data.write(value);
    }
}

/// Enable `device` for driver use: read-modify-write the command register
/// so I/O-space and memory-space BAR decode are on and the device may act
/// as bus master (required for DMA; virtio queues and data buffers are
/// device-accessed memory). Unrelated command bits (e.g. SERR#, INTx
/// disable) are preserved. Thread context only, like all of this module.
pub fn enable_device(device: &PciDevice) {
    let bdf = (device.bus, device.device, device.function);
    let command = config_read32(bdf.0, bdf.1, bdf.2, 0x04);
    let enabled =
        command | COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER;
    config_write32(bdf.0, bdf.1, bdf.2, 0x04, enabled);
}

/// Read one function's header into a `PciDevice`, or `None` for an absent
/// slot (vendor 0xFFFF).
fn read_function(bus: u8, device: u8, function: u8) -> Option<PciDevice> {
    let id = config_read32(bus, device, function, 0x00);
    let vendor_id = id as u16;
    if vendor_id == NO_DEVICE {
        return None;
    }

    let class_rev = config_read32(bus, device, function, 0x08);
    let header_latency = config_read32(bus, device, function, 0x0C);

    let mut bars = [0u32; 6];
    for (i, bar) in bars.iter_mut().enumerate() {
        *bar = config_read32(bus, device, function, 0x10 + (i as u8) * 4);
    }

    let irq = config_read32(bus, device, function, 0x3C);

    Some(PciDevice {
        bus,
        device,
        function,
        vendor_id,
        device_id: (id >> 16) as u16,
        class_code: (class_rev >> 24) as u8,
        subclass: (class_rev >> 16) as u8,
        prog_if: (class_rev >> 8) as u8,
        header_type: (header_latency >> 16) as u8,
        bars,
        interrupt_line: irq as u8,
        interrupt_pin: (irq >> 8) as u8,
    })
}

/// Append a discovered function to the table, or flag overflow.
fn record(bus_state: &mut PciBus, device: PciDevice) {
    if bus_state.count >= MAX_DEVICES {
        bus_state.overflow = true;
        return;
    }
    bus_state.devices[bus_state.count] = Some(device);
    bus_state.count += 1;
}

/// Enumerate every bus/device/function and fill the device table.
/// Flat scan of all 256 buses: with the legacy mechanism every bus is
/// reachable directly, so no bridge walking is needed. Runs in thread
/// context, uses no heap and takes no ISR-shared lock. Returns the number
/// of functions found.
pub fn init() -> usize {
    let mut bus_state = PCI_BUS.lock();
    bus_state.devices = [None; MAX_DEVICES];
    bus_state.count = 0;
    bus_state.overflow = false;

    for bus in 0..=255u8 {
        for device in 0..32u8 {
            // Function 0 must exist for the device to exist at all; a
            // non-multifunction device has no functions beyond it.
            let first = match read_function(bus, device, 0) {
                Some(first) => first,
                None => continue,
            };
            record(&mut bus_state, first);

            if first.header_type & MULTIFUNCTION_BIT != 0 {
                for function in 1..8u8 {
                    if let Some(found) = read_function(bus, device, function) {
                        record(&mut bus_state, found);
                    }
                }
            }
        }
    }

    bus_state.count
}

/// Run `visitor` over every discovered device, in enumeration order.
/// Thread context only (see module docs).
pub fn for_each_device(mut visitor: impl FnMut(&PciDevice)) {
    let bus_state = PCI_BUS.lock();
    for slot in bus_state.devices.iter().take(bus_state.count) {
        if let Some(device) = slot {
            visitor(device);
        }
    }
}

/// True if enumeration hit more devices than MAX_DEVICES.
pub fn overflowed() -> bool {
    PCI_BUS.lock().overflow
}
