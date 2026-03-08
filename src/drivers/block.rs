// Block device abstraction layer

use alloc::vec::Vec;
use alloc::vec;
use alloc::string::String;

/// Size of a disk sector in bytes
pub const SECTOR_SIZE: usize = 512;

/// Error types for block device operations
#[derive(Debug, Clone)]
pub enum BlockError {
    InvalidSector,
    DeviceNotReady,
    IoError,
    OutOfBounds,
    Custom(String),
}

/// Result type for block device operations
pub type BlockResult<T> = Result<T, BlockError>;

/// Trait for block device drivers
pub trait BlockDevice: Send + Sync {
    /// Read sectors from the device
    /// 
    /// # Arguments
    /// * `sector` - Starting sector number
    /// * `count` - Number of sectors to read
    /// 
    /// # Returns
    /// Data read from the device
    fn read_sectors(&mut self, sector: u64, count: usize) -> BlockResult<Vec<u8>>;
    
    /// Write sectors to the device
    /// 
    /// # Arguments
    /// * `sector` - Starting sector number  
    /// * `data` - Data to write (must be multiple of SECTOR_SIZE)
    /// 
    /// # Returns
    /// Number of sectors written
    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> BlockResult<usize>;
    
    /// Get the total number of sectors on the device
    fn sector_count(&self) -> u64;
    
    /// Get the size of the device in bytes
    fn capacity(&self) -> u64 {
        self.sector_count() * SECTOR_SIZE as u64
    }
    
    /// Flush any pending writes to the device
    fn flush(&mut self) -> BlockResult<()> {
        Ok(())
    }
}

/// A simple in-memory block device for testing
pub struct MemoryBlockDevice {
    data: Vec<u8>,
    sectors: u64,
}

impl MemoryBlockDevice {
    /// Create a new memory block device with the given size in MB
    pub fn new(size_mb: usize) -> Self {
        let bytes = size_mb * 1024 * 1024;
        let sectors = (bytes / SECTOR_SIZE) as u64;
        
        MemoryBlockDevice {
            data: vec![0; bytes],
            sectors,
        }
    }
}

impl BlockDevice for MemoryBlockDevice {
    fn read_sectors(&mut self, sector: u64, count: usize) -> BlockResult<Vec<u8>> {
        if sector >= self.sectors {
            return Err(BlockError::OutOfBounds);
        }
        
        let start = (sector as usize) * SECTOR_SIZE;
        let end = start + (count * SECTOR_SIZE);
        
        if end > self.data.len() {
            return Err(BlockError::OutOfBounds);
        }
        
        Ok(self.data[start..end].to_vec())
    }
    
    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> BlockResult<usize> {
        if sector >= self.sectors {
            return Err(BlockError::OutOfBounds);
        }
        
        if data.len() % SECTOR_SIZE != 0 {
            return Err(BlockError::InvalidSector);
        }
        
        let start = (sector as usize) * SECTOR_SIZE;
        let end = start + data.len();
        
        if end > self.data.len() {
            return Err(BlockError::OutOfBounds);
        }
        
        self.data[start..end].copy_from_slice(data);
        Ok(data.len() / SECTOR_SIZE)
    }
    
    fn sector_count(&self) -> u64 {
        self.sectors
    }
}