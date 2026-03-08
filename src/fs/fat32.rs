// FAT32 filesystem implementation

use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};
use super::{FileSystem as FSTrait, FileSystemError, DirEntry};

// FAT32 Constants
const SECTOR_SIZE: usize = 512;
const FAT32_EOC: u32 = 0x0FFFFFFF;  // End of cluster chain
const FAT32_BAD: u32 = 0x0FFFFFF7;  // Bad cluster
const FAT32_FREE: u32 = 0x00000000; // Free cluster

// FAT32 Boot Sector structure
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Fat32BootSector {
    // Common BPB structure
    pub jmp_boot: [u8; 3],           // Jump instruction to boot code
    pub oem_name: [u8; 8],           // OEM name (e.g., "MSWIN4.1")
    pub bytes_per_sector: u16,       // Bytes per sector (usually 512)
    pub sectors_per_cluster: u8,      // Sectors per cluster
    pub reserved_sectors: u16,        // Number of reserved sectors
    pub num_fats: u8,                // Number of FAT copies (usually 2)
    pub root_entries: u16,            // 0 for FAT32
    pub total_sectors_16: u16,        // 0 for FAT32
    pub media: u8,                   // Media descriptor
    pub fat_size_16: u16,            // 0 for FAT32
    pub sectors_per_track: u16,      // Sectors per track
    pub num_heads: u16,              // Number of heads
    pub hidden_sectors: u32,         // Hidden sectors
    pub total_sectors_32: u32,       // Total sectors (FAT32)
    
    // FAT32 specific
    pub fat_size_32: u32,            // Sectors per FAT
    pub ext_flags: u16,              // Extended flags
    pub fs_version: u16,             // File system version
    pub root_cluster: u32,           // Root directory cluster
    pub fs_info: u16,                // FSInfo sector
    pub backup_boot_sector: u16,     // Backup boot sector
    pub reserved: [u8; 12],          // Reserved
    pub drive_number: u8,            // Drive number
    pub reserved1: u8,               // Reserved
    pub boot_signature: u8,          // Boot signature (0x29)
    pub volume_id: u32,              // Volume ID
    pub volume_label: [u8; 11],      // Volume label
    pub fs_type: [u8; 8],           // File system type ("FAT32   ")
}

// FAT32 Directory Entry
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Fat32DirEntry {
    pub name: [u8; 11],              // 8.3 filename
    pub attributes: u8,              // File attributes
    pub reserved: u8,                // Reserved
    pub creation_time_tenth: u8,     // Creation time (tenths of second)
    pub creation_time: u16,          // Creation time
    pub creation_date: u16,          // Creation date
    pub last_access_date: u16,       // Last access date
    pub first_cluster_high: u16,     // High 16 bits of first cluster
    pub write_time: u16,             // Last write time
    pub write_date: u16,             // Last write date
    pub first_cluster_low: u16,      // Low 16 bits of first cluster
    pub file_size: u32,              // File size in bytes
}

// File attributes
const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;

pub struct Fat32FileSystem {
    // For now, we'll use a simulated disk in memory
    disk: Vec<u8>,
    boot_sector: Fat32BootSector,
    fat_start_sector: u32,
    data_start_sector: u32,
    current_dir_cluster: u32,
}

impl Fat32FileSystem {
    pub fn new() -> Self {
        // Create a minimal FAT32 filesystem in memory
        // This is just for demonstration - real implementation would read from disk
        let mut disk = vec![0u8; 32 * 1024 * 1024]; // 32MB disk
        
        // Create boot sector
        let boot_sector = Fat32BootSector {
            jmp_boot: [0xEB, 0x58, 0x90],
            oem_name: *b"ILOLAWA ",
            bytes_per_sector: 512,
            sectors_per_cluster: 8,  // 4KB clusters
            reserved_sectors: 32,
            num_fats: 2,
            root_entries: 0,
            total_sectors_16: 0,
            media: 0xF8,
            fat_size_16: 0,
            sectors_per_track: 63,
            num_heads: 255,
            hidden_sectors: 0,
            total_sectors_32: 65536, // 32MB / 512
            fat_size_32: 256,
            ext_flags: 0,
            fs_version: 0,
            root_cluster: 2,
            fs_info: 1,
            backup_boot_sector: 6,
            reserved: [0; 12],
            drive_number: 0x80,
            reserved1: 0,
            boot_signature: 0x29,
            volume_id: 0x12345678,
            volume_label: *b"ILO-LAWA   ",
            fs_type: *b"FAT32   ",
        };
        
        // Write boot sector to disk
        unsafe {
            let boot_bytes = core::slice::from_raw_parts(
                &boot_sector as *const _ as *const u8,
                core::mem::size_of::<Fat32BootSector>()
            );
            disk[..boot_bytes.len()].copy_from_slice(boot_bytes);
        }
        
        // Mark boot sector signature
        disk[510] = 0x55;
        disk[511] = 0xAA;
        
        // Initialize FAT tables
        let fat_start = boot_sector.reserved_sectors as usize * SECTOR_SIZE;
        let fat_size = boot_sector.fat_size_32 as usize * SECTOR_SIZE;
        
        // First two FAT entries are special
        disk[fat_start..fat_start+4].copy_from_slice(&0x0FFFFFF8u32.to_le_bytes());
        disk[fat_start+4..fat_start+8].copy_from_slice(&FAT32_EOC.to_le_bytes());
        // Root directory cluster (cluster 2)
        disk[fat_start+8..fat_start+12].copy_from_slice(&FAT32_EOC.to_le_bytes());
        
        // Copy FAT to second FAT
        let fat2_start = fat_start + fat_size;
        let fat_data = disk[fat_start..fat_start+12].to_vec();
        disk[fat2_start..fat2_start+12].copy_from_slice(&fat_data);
        
        Fat32FileSystem {
            disk,
            boot_sector,
            fat_start_sector: boot_sector.reserved_sectors as u32,
            data_start_sector: boot_sector.reserved_sectors as u32 + 
                              (boot_sector.num_fats as u32 * boot_sector.fat_size_32),
            current_dir_cluster: 2, // Root directory
        }
    }
    
    fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.data_start_sector + (cluster - 2) * self.boot_sector.sectors_per_cluster as u32
    }
    
    fn read_cluster(&self, cluster: u32) -> Vec<u8> {
        let sector = self.cluster_to_sector(cluster);
        let offset = sector as usize * SECTOR_SIZE;
        let size = self.boot_sector.sectors_per_cluster as usize * SECTOR_SIZE;
        self.disk[offset..offset + size].to_vec()
    }
    
    fn get_fat_entry(&self, cluster: u32) -> u32 {
        let fat_offset = cluster * 4;
        let fat_sector = self.fat_start_sector + (fat_offset / SECTOR_SIZE as u32);
        let fat_entry_offset = (fat_offset % SECTOR_SIZE as u32) as usize;
        
        let sector_offset = fat_sector as usize * SECTOR_SIZE;
        let bytes = &self.disk[sector_offset + fat_entry_offset..sector_offset + fat_entry_offset + 4];
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) & 0x0FFFFFFF
    }
    
    fn parse_dir_entry(&self, entry_bytes: &[u8]) -> Option<Fat32DirEntry> {
        if entry_bytes.len() < 32 || entry_bytes[0] == 0x00 || entry_bytes[0] == 0xE5 {
            return None;
        }
        
        unsafe {
            Some(*(entry_bytes.as_ptr() as *const Fat32DirEntry))
        }
    }
}

impl FSTrait for Fat32FileSystem {
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, FileSystemError> {
        // Get filename from path
        let filename = path.split('/').last().unwrap_or(path);
        
        // Read directory entries from current cluster
        let cluster_data = self.read_cluster(self.current_dir_cluster);
        
        // Find the file entry
        for i in 0..(cluster_data.len() / 32) {
            let entry_bytes = &cluster_data[i * 32..(i + 1) * 32];
            
            if let Some(entry) = self.parse_dir_entry(entry_bytes) {
                // Skip directories and special entries
                if entry.attributes & (ATTR_DIRECTORY | ATTR_VOLUME_ID) != 0 {
                    continue;
                }
                
                // Compare filename (8.3 format)
                let mut entry_name = String::new();
                for &b in &entry.name[..8] {
                    if b != 0x20 {
                        entry_name.push(b as char);
                    }
                }
                if entry.name[8] != 0x20 {
                    entry_name.push('.');
                    for &b in &entry.name[8..11] {
                        if b != 0x20 {
                            entry_name.push(b as char);
                        }
                    }
                }
                
                // Check if this is our file
                if entry_name.to_lowercase() == filename.to_lowercase() {
                    // Get first cluster
                    let first_cluster = ((entry.first_cluster_high as u32) << 16) | entry.first_cluster_low as u32;
                    
                    // Read file data following cluster chain
                    let mut file_data = Vec::new();
                    let mut current = first_cluster;
                    
                    while current >= 2 && current < FAT32_EOC {
                        let cluster_content = self.read_cluster(current);
                        file_data.extend_from_slice(&cluster_content);
                        current = self.get_fat_entry(current);
                    }
                    
                    // Truncate to actual file size
                    file_data.truncate(entry.file_size as usize);
                    return Ok(file_data);
                }
            }
        }
        
        Err(FileSystemError::NotFound)
    }
    
    fn write_file(&mut self, _path: &str, _data: &[u8]) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotImplemented)
    }
    
    fn create_file(&mut self, _path: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotImplemented)
    }
    
    fn delete_file(&mut self, _path: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotImplemented)
    }
    
    fn create_directory(&mut self, _path: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotImplemented)
    }
    
    fn delete_directory(&mut self, _path: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotImplemented)
    }
    
    fn list_directory(&mut self, _path: &str) -> Result<Vec<DirEntry>, FileSystemError> {
        let mut entries = Vec::new();
        let cluster_data = self.read_cluster(self.current_dir_cluster);
        
        // Parse directory entries (32 bytes each)
        for i in 0..(cluster_data.len() / 32) {
            let entry_bytes = &cluster_data[i * 32..(i + 1) * 32];
            
            if let Some(entry) = self.parse_dir_entry(entry_bytes) {
                // Skip long name entries and volume labels
                if entry.attributes & (ATTR_LONG_NAME | ATTR_VOLUME_ID) != 0 {
                    continue;
                }
                
                // Convert 8.3 name to string
                let mut name = String::new();
                for &b in &entry.name[..8] {
                    if b != 0x20 { // Skip spaces
                        name.push(b as char);
                    }
                }
                if entry.name[8] != 0x20 {
                    name.push('.');
                    for &b in &entry.name[8..11] {
                        if b != 0x20 {
                            name.push(b as char);
                        }
                    }
                }
                
                entries.push(DirEntry {
                    name,
                    is_directory: (entry.attributes & ATTR_DIRECTORY) != 0,
                    size: entry.file_size as u64,
                });
            }
        }
        
        Ok(entries)
    }
    
    fn exists(&mut self, _path: &str) -> bool {
        false
    }
    
    fn is_directory(&mut self, _path: &str) -> bool {
        false
    }
    
    fn is_file(&mut self, _path: &str) -> bool {
        false
    }
    
    fn get_current_directory(&self) -> String {
        "/".into() // Simplified
    }
    
    fn set_current_directory(&mut self, _path: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotImplemented)
    }
}