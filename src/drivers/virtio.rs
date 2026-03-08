// VirtIO driver implementation using virtio-drivers crate

use core::ptr::NonNull;
use alloc::vec::Vec;
use alloc::vec;
use virtio_drivers::{BufferDirection, Hal, PhysAddr, PAGE_SIZE};
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
use spin::Mutex;
use lazy_static::lazy_static;
use crate::drivers::block::{BlockDevice, BlockResult, BlockError, SECTOR_SIZE};

/// Implementation of the HAL trait required by virtio-drivers
pub struct VirtioHal;

unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        // Allocate contiguous physical memory for DMA
        // For simplicity, we'll use the heap allocator
        let size = pages * PAGE_SIZE;
        let layout = core::alloc::Layout::from_size_align(size, PAGE_SIZE).unwrap();
        
        let ptr = unsafe { alloc::alloc::alloc(layout) };
        if ptr.is_null() {
            panic!("Failed to allocate DMA buffer");
        }
        
        // In a real implementation, we'd need to get the physical address
        // For now, we assume identity mapping (virtual == physical)
        let phys_addr = ptr as usize;
        let non_null = NonNull::new(ptr).unwrap();
        
        (phys_addr, non_null)
    }
    
    unsafe fn dma_dealloc(paddr: PhysAddr, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        let size = pages * PAGE_SIZE;
        let layout = core::alloc::Layout::from_size_align(size, PAGE_SIZE).unwrap();
        let ptr = paddr as *mut u8;
        
        alloc::alloc::dealloc(ptr, layout);
        0
    }
    
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        // Assume identity mapping for MMIO regions
        NonNull::new(paddr as *mut u8).unwrap()
    }
    
    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        // Get physical address of the buffer
        // In identity-mapped memory, virtual == physical
        buffer.as_ptr() as *mut u8 as usize
    }
    
    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // No-op for identity-mapped memory
    }
}

/// VirtIO block device wrapper
pub struct VirtioBlockDevice {
    inner: Option<virtio_drivers::device::blk::VirtIOBlk<VirtioHal, MmioTransport>>,
}

impl VirtioBlockDevice {
    /// Create a new VirtIO block device
    /// 
    /// # Arguments
    /// * `header` - Physical address of the VirtIO device header
    pub unsafe fn new(header: usize) -> Result<Self, &'static str> {
        // Create MMIO transport for the device
        let header_ptr = NonNull::new(header as *mut VirtIOHeader)
            .ok_or("Invalid header address")?;
        let transport = MmioTransport::new(header_ptr)
            .map_err(|_| "Failed to create MMIO transport")?;
        
        // Try to create the VirtIO block device
        match virtio_drivers::device::blk::VirtIOBlk::<VirtioHal, MmioTransport>::new(
            transport
        ) {
            Ok(blk) => Ok(VirtioBlockDevice { inner: Some(blk) }),
            Err(_) => Err("Failed to initialize VirtIO block device"),
        }
    }
    
    /// Check if the device is initialized
    pub fn is_initialized(&self) -> bool {
        self.inner.is_some()
    }
}

impl BlockDevice for VirtioBlockDevice {
    fn read_sectors(&mut self, sector: u64, count: usize) -> BlockResult<Vec<u8>> {
        let blk = self.inner.as_mut().ok_or(BlockError::DeviceNotReady)?;
        
        let mut buffer = vec![0u8; count * SECTOR_SIZE];
        
        // VirtIO uses block numbers, we need to convert sector to block
        let block_id = sector as usize;
        
        // Read the blocks
        match blk.read_blocks(block_id, &mut buffer) {
            Ok(_) => Ok(buffer),
            Err(_) => Err(BlockError::IoError),
        }
    }
    
    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> BlockResult<usize> {
        let blk = self.inner.as_mut().ok_or(BlockError::DeviceNotReady)?;
        
        if data.len() % SECTOR_SIZE != 0 {
            return Err(BlockError::InvalidSector);
        }
        
        let block_id = sector as usize;
        let blocks = data.len() / SECTOR_SIZE;
        
        // Write the blocks
        match blk.write_blocks(block_id, data) {
            Ok(_) => Ok(blocks),
            Err(_) => Err(BlockError::IoError),
        }
    }
    
    fn sector_count(&self) -> u64 {
        self.inner.as_ref()
            .map(|blk| blk.capacity() as u64)
            .unwrap_or(0)
    }
    
    fn flush(&mut self) -> BlockResult<()> {
        // VirtIO block devices don't require explicit flush
        Ok(())
    }
}

// Global block device instance
lazy_static! {
    pub static ref BLOCK_DEVICE: Mutex<Option<VirtioBlockDevice>> = Mutex::new(None);
}

/// Initialize the VirtIO block device
/// 
/// This should be called during kernel initialization
/// after PCI enumeration finds a VirtIO block device
pub unsafe fn init_virtio_block(device_address: usize) -> Result<(), &'static str> {
    let device = VirtioBlockDevice::new(device_address)?;
    *BLOCK_DEVICE.lock() = Some(device);
    Ok(())
}