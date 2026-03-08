// Filesystem module with pluggable implementations

pub mod fat32;
pub mod memory; // Current RAM-based filesystem

use alloc::vec::Vec;
use alloc::string::String;
use alloc::boxed::Box;
use alloc::format;

#[derive(Debug, Clone)]
pub enum FileSystemError {
    NotFound,
    PermissionDenied,
    DiskFull,
    InvalidPath,
    IoError,
    NotImplemented,
    AlreadyExists,
    NotADirectory,
    NotAFile,
    Custom(String),
}

impl From<&str> for FileSystemError {
    fn from(s: &str) -> Self {
        FileSystemError::Custom(s.into())
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_directory: bool,
    pub size: u64,
}

// Common filesystem trait that all implementations must follow
pub trait FileSystem: Send + Sync {
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, FileSystemError>;
    fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), FileSystemError>;
    fn create_file(&mut self, path: &str) -> Result<(), FileSystemError>;
    fn delete_file(&mut self, path: &str) -> Result<(), FileSystemError>;
    fn create_directory(&mut self, path: &str) -> Result<(), FileSystemError>;
    fn delete_directory(&mut self, path: &str) -> Result<(), FileSystemError>;
    fn list_directory(&mut self, path: &str) -> Result<Vec<DirEntry>, FileSystemError>;
    fn exists(&mut self, path: &str) -> bool;
    fn is_directory(&mut self, path: &str) -> bool;
    fn is_file(&mut self, path: &str) -> bool;
    fn get_current_directory(&self) -> String;
    fn set_current_directory(&mut self, path: &str) -> Result<(), FileSystemError>;
}

// Global filesystem instance (currently using memory implementation)
use spin::Mutex;
use lazy_static::lazy_static;

lazy_static! {
    static ref FILESYSTEM: Mutex<Box<dyn FileSystem>> = {
        // Start with memory filesystem, can be replaced with FAT32 later
        Mutex::new(Box::new(memory::MemoryFileSystem::new()))
    };
}

// Public API functions (unchanged interface for shell)
pub fn init() {
    // Filesystem is initialized lazily via lazy_static
    let _ = FILESYSTEM.lock();
}

pub fn list_directory() -> Vec<String> {
    match FILESYSTEM.lock().list_directory(".") {
        Ok(entries) => {
            entries.iter().map(|e| {
                let type_char = if e.is_directory { "d" } else { "-" };
                format!("{} {} {} bytes", type_char, e.name, e.size)
            }).collect()
        }
        Err(_) => Vec::new(),
    }
}

pub fn change_directory(path: &str) -> Result<(), String> {
    FILESYSTEM.lock().set_current_directory(path)
        .map_err(|e| format!("{:?}", e))
}

pub fn create_file(name: &str, content: Vec<u8>) -> Result<(), String> {
    let mut fs = FILESYSTEM.lock();
    fs.create_file(name).map_err(|e| format!("{:?}", e))?;
    fs.write_file(name, &content)
        .map_err(|e| format!("{:?}", e))
}

pub fn read_file(name: &str) -> Result<Vec<u8>, String> {
    FILESYSTEM.lock().read_file(name)
        .map_err(|e| format!("{:?}", e))
}

pub fn create_directory(name: &str) -> Result<(), String> {
    FILESYSTEM.lock().create_directory(name)
        .map_err(|e| format!("{:?}", e))
}

pub fn remove_file(name: &str) -> Result<(), String> {
    FILESYSTEM.lock().delete_file(name)
        .map_err(|e| format!("{:?}", e))
}

pub fn get_current_path() -> String {
    FILESYSTEM.lock().get_current_directory()
}