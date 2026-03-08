// Simple RAM-based filesystem implementation with trait

use alloc::string::String;
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
        
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = &mut self.root;
        
        for part in parts {
            let found = current.children.iter_mut()
                .find(|e| e.name == part);
            match found {
                Some(entry) => current = entry,
                None => return None,
            }
        }
        
        Some(current)
    }
}

impl FSTrait for MemoryFileSystem {
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, FileSystemError> {
        let entry = self.find_entry(path).ok_or(FileSystemError::NotFound)?;
        match entry.file_type {
            FileType::Regular => Ok(entry.data.clone()),
            FileType::Directory => Err(FileSystemError::NotAFile),
        }
    }
    
    fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), FileSystemError> {
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
        let filename = path.split('/').last().unwrap_or(path);
        
        // Get the current directory
        let current = if self.current_dir == "/" {
            &mut self.root
        } else {
            let dir_name = self.current_dir.trim_start_matches('/');
            self.root.children.iter_mut()
                .find(|e| e.name == dir_name && matches!(e.file_type, FileType::Directory))
                .ok_or(FileSystemError::NotFound)?
        };
        
        if current.children.iter().any(|e| e.name == filename) {
            return Err(FileSystemError::AlreadyExists);
        }
        
        current.children.push(Box::new(FileEntry::new_file(filename.into(), Vec::new())));
        Ok(())
    }
    
    fn delete_file(&mut self, path: &str) -> Result<(), FileSystemError> {
        let filename = path.split('/').last().unwrap_or(path);
        let pos = self.root.children.iter()
            .position(|e| e.name == filename && matches!(e.file_type, FileType::Regular))
            .ok_or(FileSystemError::NotFound)?;
        
        self.root.children.remove(pos);
        Ok(())
    }
    
    fn create_directory(&mut self, path: &str) -> Result<(), FileSystemError> {
        let dirname = path.split('/').last().unwrap_or(path);
        
        // Get the current directory
        let current = if self.current_dir == "/" {
            &mut self.root
        } else {
            let dir_name = self.current_dir.trim_start_matches('/');
            self.root.children.iter_mut()
                .find(|e| e.name == dir_name && matches!(e.file_type, FileType::Directory))
                .ok_or(FileSystemError::NotFound)?
        };
        
        if current.children.iter().any(|e| e.name == dirname) {
            return Err(FileSystemError::AlreadyExists);
        }
        
        current.children.push(Box::new(FileEntry::new_directory(dirname.into())));
        Ok(())
    }
    
    fn delete_directory(&mut self, _path: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotImplemented)
    }
    
    fn list_directory(&mut self, _path: &str) -> Result<Vec<DirEntry>, FileSystemError> {
        // Get the current directory based on current_dir
        let dir_to_list = if self.current_dir == "/" {
            &self.root
        } else {
            // Find the directory from current_dir path
            let dir_name = self.current_dir.trim_start_matches('/');
            self.root.children.iter()
                .find(|e| e.name == dir_name && matches!(e.file_type, FileType::Directory))
                .ok_or(FileSystemError::NotFound)?
        };
        
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
        if path == "/" {
            self.current_dir = "/".into();
            Ok(())
        } else if path == ".." {
            // Simple parent directory handling
            self.current_dir = "/".into();
            Ok(())
        } else {
            // Check if directory exists
            if self.root.children.iter().any(|e| {
                e.name == path && matches!(e.file_type, FileType::Directory)
            }) {
                self.current_dir = format!("/{}", path);
                Ok(())
            } else {
                Err(FileSystemError::NotFound)
            }
        }
    }
}