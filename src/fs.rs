// Simple RAM-based filesystem for ilo-lawa OS

use alloc::string::String;
use alloc::vec::Vec;
use alloc::boxed::Box;
use alloc::format;
use spin::Mutex;
use lazy_static::lazy_static;

// Maximum filename length
const MAX_FILENAME_LEN: usize = 255;
// Maximum file size (1MB for now)
const MAX_FILE_SIZE: usize = 1024 * 1024;

lazy_static! {
    static ref FILESYSTEM: Mutex<FileSystem> = Mutex::new(FileSystem::new());
}

#[derive(Debug, Clone)]
pub enum FileType {
    Regular,
    Directory,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub file_type: FileType,
    pub size: usize,
    pub data: Vec<u8>,
    pub children: Vec<Box<FileEntry>>,  // For directories
}

impl FileEntry {
    pub fn new_file(name: String, data: Vec<u8>) -> Self {
        let size = data.len();
        FileEntry {
            name,
            file_type: FileType::Regular,
            size,
            data,
            children: Vec::new(),
        }
    }
    
    pub fn new_directory(name: String) -> Self {
        FileEntry {
            name,
            file_type: FileType::Directory,
            size: 0,
            data: Vec::new(),
            children: Vec::new(),
        }
    }
}

pub struct FileSystem {
    root: FileEntry,
    current_dir: Vec<String>,  // Path to current directory
}

impl FileSystem {
    pub fn new() -> Self {
        let mut root = FileEntry::new_directory("/".into());
        
        // Create some default directories
        root.children.push(Box::new(FileEntry::new_directory("bin".into())));
        root.children.push(Box::new(FileEntry::new_directory("home".into())));
        root.children.push(Box::new(FileEntry::new_directory("etc".into())));
        
        // Add a welcome file
        let welcome_content = b"Welcome to ilo-lawa OS!\nThis is a simple RAM filesystem.\n".to_vec();
        root.children.push(Box::new(FileEntry::new_file("README.txt".into(), welcome_content)));
        
        FileSystem {
            root,
            current_dir: Vec::new(),
        }
    }
    
    fn get_current_dir(&mut self) -> Option<&mut FileEntry> {
        let mut current = &mut self.root;
        
        for dir_name in &self.current_dir {
            let found = current.children.iter_mut()
                .find(|entry| {
                    entry.name == *dir_name && 
                    matches!(entry.file_type, FileType::Directory)
                });
            
            match found {
                Some(dir) => current = dir,
                None => return None,
            }
        }
        
        Some(current)
    }
    
    pub fn list_directory(&mut self) -> Vec<String> {
        let mut result = Vec::new();
        
        if let Some(dir) = self.get_current_dir() {
            for entry in &dir.children {
                let type_char = match entry.file_type {
                    FileType::Directory => "d",
                    FileType::Regular => "-",
                };
                result.push(format!("{} {} {} bytes", type_char, entry.name, entry.size));
            }
        }
        
        result
    }
    
    pub fn change_directory(&mut self, path: &str) -> Result<(), String> {
        if path == "/" {
            self.current_dir.clear();
            return Ok(());
        }
        
        if path == ".." {
            if !self.current_dir.is_empty() {
                self.current_dir.pop();
            }
            return Ok(());
        }
        
        // Check if directory exists
        let mut test_path = self.current_dir.clone();
        test_path.push(path.into());
        
        // Verify the path exists
        let mut current = &self.root;
        for dir_name in &test_path {
            let found = current.children.iter()
                .find(|entry| {
                    entry.name == *dir_name && 
                    matches!(entry.file_type, FileType::Directory)
                });
            
            match found {
                Some(dir) => current = dir,
                None => return Err(format!("Directory not found: {}", path)),
            }
        }
        
        self.current_dir = test_path;
        Ok(())
    }
    
    pub fn create_file(&mut self, name: &str, content: Vec<u8>) -> Result<(), String> {
        if name.len() > MAX_FILENAME_LEN {
            return Err("Filename too long".into());
        }
        
        if content.len() > MAX_FILE_SIZE {
            return Err("File too large".into());
        }
        
        if let Some(dir) = self.get_current_dir() {
            // Check if file already exists
            if dir.children.iter().any(|e| e.name == name) {
                return Err(format!("File already exists: {}", name));
            }
            
            dir.children.push(Box::new(FileEntry::new_file(name.into(), content)));
            Ok(())
        } else {
            Err("Cannot access current directory".into())
        }
    }
    
    pub fn read_file(&mut self, name: &str) -> Result<Vec<u8>, String> {
        if let Some(dir) = self.get_current_dir() {
            let file = dir.children.iter()
                .find(|e| e.name == name && matches!(e.file_type, FileType::Regular));
            
            match file {
                Some(f) => Ok(f.data.clone()),
                None => Err(format!("File not found: {}", name)),
            }
        } else {
            Err("Cannot access current directory".into())
        }
    }
    
    pub fn create_directory(&mut self, name: &str) -> Result<(), String> {
        if name.len() > MAX_FILENAME_LEN {
            return Err("Directory name too long".into());
        }
        
        if let Some(dir) = self.get_current_dir() {
            // Check if already exists
            if dir.children.iter().any(|e| e.name == name) {
                return Err(format!("Already exists: {}", name));
            }
            
            dir.children.push(Box::new(FileEntry::new_directory(name.into())));
            Ok(())
        } else {
            Err("Cannot access current directory".into())
        }
    }
    
    pub fn remove_file(&mut self, name: &str) -> Result<(), String> {
        if let Some(dir) = self.get_current_dir() {
            let index = dir.children.iter()
                .position(|e| e.name == name && matches!(e.file_type, FileType::Regular));
            
            match index {
                Some(idx) => {
                    dir.children.remove(idx);
                    Ok(())
                }
                None => Err(format!("File not found: {}", name)),
            }
        } else {
            Err("Cannot access current directory".into())
        }
    }
    
    pub fn get_current_path(&self) -> String {
        if self.current_dir.is_empty() {
            "/".into()
        } else {
            format!("/{}", self.current_dir.join("/"))
        }
    }
}

// Public API functions
pub fn init() {
    // Filesystem is initialized lazily via lazy_static
    let _ = FILESYSTEM.lock();
}

pub fn list_directory() -> Vec<String> {
    FILESYSTEM.lock().list_directory()
}

pub fn change_directory(path: &str) -> Result<(), String> {
    FILESYSTEM.lock().change_directory(path)
}

pub fn create_file(name: &str, content: Vec<u8>) -> Result<(), String> {
    FILESYSTEM.lock().create_file(name, content)
}

pub fn read_file(name: &str) -> Result<Vec<u8>, String> {
    FILESYSTEM.lock().read_file(name)
}

pub fn create_directory(name: &str) -> Result<(), String> {
    FILESYSTEM.lock().create_directory(name)
}

pub fn remove_file(name: &str) -> Result<(), String> {
    FILESYSTEM.lock().remove_file(name)
}

pub fn get_current_path() -> String {
    FILESYSTEM.lock().get_current_path()
}