// Simple RAM-based filesystem implementation with trait

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::boxed::Box;
use alloc::format;
use super::{FileSystem as FSTrait, FileSystemError, DirEntry};

#[derive(Debug, Clone)]
enum FileType {
    Regular,
    Directory,
}

#[derive(Debug, Clone)]
struct FileEntry {
    name: String,
    file_type: FileType,
    data: Vec<u8>,
    children: Vec<Box<FileEntry>>,
}

impl FileEntry {
    fn new_file(name: String, data: Vec<u8>) -> Self {
        FileEntry {
            name,
            file_type: FileType::Regular,
            data,
            children: Vec::new(),
        }
    }
    
    fn new_directory(name: String) -> Self {
        FileEntry {
            name,
            file_type: FileType::Directory,
            data: Vec::new(),
            children: Vec::new(),
        }
    }
}

pub struct MemoryFileSystem {
    root: FileEntry,
    current_dir: String,
}

impl MemoryFileSystem {
    pub fn new() -> Self {
        let mut root = FileEntry::new_directory(String::new());
        
        // Create default directories
        root.children.push(Box::new(FileEntry::new_directory("bin".into())));
        root.children.push(Box::new(FileEntry::new_directory("home".into())));
        root.children.push(Box::new(FileEntry::new_directory("etc".into())));
        
        // Add a welcome file
        let welcome = b"Welcome to ilo-lawa OS!\n".to_vec();
        root.children.push(Box::new(FileEntry::new_file("README.txt".into(), welcome)));
        
        MemoryFileSystem {
            root,
            current_dir: "/".into(),
        }
    }
    
    fn find_entry(&mut self, path: &str) -> Option<&mut FileEntry> {
        if path == "/" || path.is_empty() {
            return Some(&mut self.root);
        }
        
        let normalized = self.normalize_path(path);
        if normalized == "/" {
            return Some(&mut self.root);
        }
        
        let parts: Vec<&str> = normalized.trim_start_matches('/')
            .split('/').filter(|s| !s.is_empty()).collect();
        
        Self::find_entry_recursive(&mut self.root, &parts)
    }
    
    fn find_entry_recursive<'a>(current: &'a mut FileEntry, parts: &[&str]) -> Option<&'a mut FileEntry> {
        if parts.is_empty() {
            return Some(current);
        }
        
        let first = parts[0];
        let remaining = &parts[1..];
        
        for child in &mut current.children {
            if child.name == first {
                if remaining.is_empty() {
                    return Some(child);
                } else if matches!(child.file_type, FileType::Directory) {
                    return Self::find_entry_recursive(child, remaining);
                } else {
                    return None; // Not a directory
                }
            }
        }
        None
    }
    
    fn normalize_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            // Absolute path
            path.into()
        } else {
            // Relative path
            let mut result = self.current_dir.clone();
            
            let parts: Vec<&str> = path.split('/').collect();
            for part in parts {
                if part == "." || part.is_empty() {
                    continue;
                } else if part == ".." {
                    // Go up one directory
                    if result != "/" {
                        let pos = result.rfind('/').unwrap_or(0);
                        result = if pos == 0 { "/".into() } else { result[..pos].into() };
                    }
                } else {
                    if !result.ends_with('/') {
                        result.push('/');
                    }
                    result.push_str(part);
                }
            }
            result
        }
    }
}

// Memory limits
const MAX_FILE_SIZE: usize = 10 * 1024 * 1024; // 10MB limit
const MAX_TOTAL_SIZE: usize = 100 * 1024 * 1024; // 100MB total limit

impl FSTrait for MemoryFileSystem {
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, FileSystemError> {
        let entry = self.find_entry(path).ok_or(FileSystemError::NotFound)?;
        match entry.file_type {
            FileType::Regular => Ok(entry.data.clone()),
            FileType::Directory => Err(FileSystemError::NotAFile),
        }
    }
    
    fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), FileSystemError> {
        if data.len() > MAX_FILE_SIZE {
            return Err(FileSystemError::Custom(format!("File too large: {} bytes (max: {} bytes)", data.len(), MAX_FILE_SIZE)));
        }
        
        let entry = self.find_entry(path).ok_or(FileSystemError::NotFound)?;
        match entry.file_type {
            FileType::Regular => {
                entry.data = data.to_vec();
                Ok(())
            }
            FileType::Directory => Err(FileSystemError::NotAFile),
        }
    }
    
    fn create_file(&mut self, path: &str) -> Result<(), FileSystemError> {
        let normalized = self.normalize_path(path);
        let (dir_path, filename) = if let Some(pos) = normalized.rfind('/') {
            if pos == 0 {
                ("/".to_string(), &normalized[1..])
            } else {
                (normalized[..pos].to_string(), &normalized[pos + 1..])
            }
        } else {
            (self.current_dir.clone(), path)
        };
        
        if filename.is_empty() {
            return Err(FileSystemError::InvalidPath);
        }
        
        let parent = self.find_entry(&dir_path).ok_or(FileSystemError::NotFound)?;
        
        if !matches!(parent.file_type, FileType::Directory) {
            return Err(FileSystemError::NotADirectory);
        }
        
        if parent.children.iter().any(|e| e.name == filename) {
            return Err(FileSystemError::AlreadyExists);
        }
        
        parent.children.push(Box::new(FileEntry::new_file(filename.into(), Vec::new())));
        Ok(())
    }
    
    fn delete_file(&mut self, path: &str) -> Result<(), FileSystemError> {
        let normalized = self.normalize_path(path);
        let (dir_path, filename) = if let Some(pos) = normalized.rfind('/') {
            if pos == 0 {
                ("/".to_string(), &normalized[1..])
            } else {
                (normalized[..pos].to_string(), &normalized[pos + 1..])
            }
        } else {
            (self.current_dir.clone(), path)
        };
        
        let parent = self.find_entry(&dir_path).ok_or(FileSystemError::NotFound)?;
        
        let pos = parent.children.iter()
            .position(|e| e.name == filename && matches!(e.file_type, FileType::Regular))
            .ok_or(FileSystemError::NotFound)?;
        
        parent.children.remove(pos);
        Ok(())
    }
    
    fn create_directory(&mut self, path: &str) -> Result<(), FileSystemError> {
        let normalized = self.normalize_path(path);
        let (dir_path, dirname) = if let Some(pos) = normalized.rfind('/') {
            if pos == 0 {
                ("/".to_string(), &normalized[1..])
            } else {
                (normalized[..pos].to_string(), &normalized[pos + 1..])
            }
        } else {
            (self.current_dir.clone(), path)
        };
        
        if dirname.is_empty() {
            return Err(FileSystemError::InvalidPath);
        }
        
        let parent = self.find_entry(&dir_path).ok_or(FileSystemError::NotFound)?;
        
        if !matches!(parent.file_type, FileType::Directory) {
            return Err(FileSystemError::NotADirectory);
        }
        
        if parent.children.iter().any(|e| e.name == dirname) {
            return Err(FileSystemError::AlreadyExists);
        }
        
        parent.children.push(Box::new(FileEntry::new_directory(dirname.into())));
        Ok(())
    }
    
    fn delete_directory(&mut self, path: &str) -> Result<(), FileSystemError> {
        let normalized = self.normalize_path(path);
        
        // Don't allow deleting root
        if normalized == "/" {
            return Err(FileSystemError::PermissionDenied);
        }
        
        let (dir_path, dirname) = if let Some(pos) = normalized.rfind('/') {
            if pos == 0 {
                ("/".to_string(), &normalized[1..])
            } else {
                (normalized[..pos].to_string(), &normalized[pos + 1..])
            }
        } else {
            (self.current_dir.clone(), path)
        };
        
        let parent = self.find_entry(&dir_path).ok_or(FileSystemError::NotFound)?;
        
        let pos = parent.children.iter()
            .position(|e| e.name == dirname && matches!(e.file_type, FileType::Directory))
            .ok_or(FileSystemError::NotFound)?;
        
        // Check if directory is empty
        if !parent.children[pos].children.is_empty() {
            return Err(FileSystemError::Custom("Directory not empty".into()));
        }
        
        parent.children.remove(pos);
        Ok(())
    }
    
    fn list_directory(&mut self, path: &str) -> Result<Vec<DirEntry>, FileSystemError> {
        let dir_path = if path == "." || path.is_empty() {
            self.current_dir.clone()
        } else {
            self.normalize_path(path)
        };
        
        let dir_to_list = self.find_entry(&dir_path).ok_or(FileSystemError::NotFound)?;
        
        if !matches!(dir_to_list.file_type, FileType::Directory) {
            return Err(FileSystemError::NotADirectory);
        }
        
        let entries = dir_to_list.children.iter().map(|e| {
            DirEntry {
                name: e.name.clone(),
                is_directory: matches!(e.file_type, FileType::Directory),
                size: e.data.len() as u64,
            }
        }).collect();
        
        Ok(entries)
    }
    
    fn exists(&mut self, path: &str) -> bool {
        self.find_entry(path).is_some()
    }
    
    fn is_directory(&mut self, path: &str) -> bool {
        self.find_entry(path)
            .map(|e| matches!(e.file_type, FileType::Directory))
            .unwrap_or(false)
    }
    
    fn is_file(&mut self, path: &str) -> bool {
        self.find_entry(path)
            .map(|e| matches!(e.file_type, FileType::Regular))
            .unwrap_or(false)
    }
    
    fn get_current_directory(&self) -> String {
        self.current_dir.clone()
    }
    
    fn set_current_directory(&mut self, path: &str) -> Result<(), FileSystemError> {
        let new_dir = self.normalize_path(path);
        
        // Check if path exists and is a directory
        let entry = self.find_entry(&new_dir).ok_or(FileSystemError::NotFound)?;
        
        if !matches!(entry.file_type, FileType::Directory) {
            return Err(FileSystemError::NotADirectory);
        }
        
        self.current_dir = new_dir;
        Ok(())
    }
}