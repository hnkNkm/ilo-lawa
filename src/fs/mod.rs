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

impl core::fmt::Display for FileSystemError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FileSystemError::NotFound => write!(f, "No such file or directory"),
            FileSystemError::PermissionDenied => write!(f, "Permission denied"),
            FileSystemError::DiskFull => write!(f, "No space left on device"),
            FileSystemError::InvalidPath => write!(f, "Invalid path"),
            FileSystemError::IoError => write!(f, "I/O error"),
            FileSystemError::NotImplemented => write!(f, "Not implemented"),
            FileSystemError::AlreadyExists => write!(f, "File or directory already exists"),
            FileSystemError::NotADirectory => write!(f, "Not a directory"),
            FileSystemError::NotAFile => write!(f, "Is a directory"),
            FileSystemError::Custom(msg) => write!(f, "{}", msg),
        }
    }
}

/// Format a filesystem error as a user-facing message including the path.
fn fs_error(path: &str, err: FileSystemError) -> String {
    format!("{}: {}", path, err)
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

/// List a directory. Errors (missing path, not a directory) are returned so
/// the shell can show a clear message instead of "(empty directory)".
pub fn list_directory(path: &str) -> Result<Vec<String>, String> {
    let path_to_list = if path.is_empty() { "." } else { path };
    let entries = FILESYSTEM
        .lock()
        .list_directory(path_to_list)
        .map_err(|e| fs_error(path_to_list, e))?;
    Ok(entries
        .iter()
        .map(|e| {
            let type_char = if e.is_directory { "d" } else { "-" };
            format!("{} {} {} bytes", type_char, e.name, e.size)
        })
        .collect())
}

pub fn change_directory(path: &str) -> Result<(), String> {
    FILESYSTEM
        .lock()
        .set_current_directory(path)
        .map_err(|e| fs_error(path, e))
}

/// Create a file with content. Overwrites the contents of an existing regular
/// file; creates a new file only when the path does not exist yet.
pub fn create_file(name: &str, content: Vec<u8>) -> Result<(), String> {
    let mut fs = FILESYSTEM.lock();
    let result = if fs.is_file(name) {
        fs.write_file(name, &content)
    } else if fs.is_directory(name) {
        Err(FileSystemError::NotAFile)
    } else {
        fs.create_file(name)
            .and_then(|()| fs.write_file(name, &content))
    };
    result.map_err(|e| fs_error(name, e))
}

pub fn read_file(name: &str) -> Result<Vec<u8>, String> {
    FILESYSTEM
        .lock()
        .read_file(name)
        .map_err(|e| fs_error(name, e))
}

pub fn create_directory(name: &str) -> Result<(), String> {
    FILESYSTEM
        .lock()
        .create_directory(name)
        .map_err(|e| fs_error(name, e))
}

pub fn remove_file(name: &str) -> Result<(), String> {
    FILESYSTEM
        .lock()
        .delete_file(name)
        .map_err(|e| fs_error(name, e))
}

/// Remove an empty directory (shell `rmdir`).
pub fn remove_directory(name: &str) -> Result<(), String> {
    FILESYSTEM
        .lock()
        .delete_directory(name)
        .map_err(|e| fs_error(name, e))
}

pub fn get_current_path() -> String {
    FILESYSTEM.lock().get_current_directory()
}