// Simple shell implementation for ilo-lawa OS

use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;
use lazy_static::lazy_static;

const MAX_COMMAND_LENGTH: usize = 256;
const HISTORY_SIZE: usize = 10;

lazy_static! {
    static ref SHELL: Mutex<Shell> = Mutex::new(Shell::new());
}

pub struct Shell {
    command_buffer: String,
    history: Vec<String>,
    history_index: usize,
    cursor_position: usize,
}

impl Shell {
    pub fn new() -> Self {
        Shell {
            command_buffer: String::new(),
            history: Vec::with_capacity(HISTORY_SIZE),
            history_index: 0,
            cursor_position: 0,
        }
    }
    
    pub fn handle_key(&mut self, c: char) {
        match c {
            '\n' => {
                // Execute command
                if !self.command_buffer.is_empty() {
                    crate::terminal::print("\n");
                    self.execute_command();
                    self.command_buffer.clear();
                    self.cursor_position = 0;
                }
                self.print_prompt();
            }
            '\x08' => { // Backspace
                if self.cursor_position > 0 && !self.command_buffer.is_empty() {
                    self.cursor_position -= 1;
                    self.command_buffer.remove(self.cursor_position);
                    // Redraw the entire line to handle backspace properly
                    crate::terminal::print("\r");
                    // Clear the entire line
                    for _ in 0..80 {  // Clear 80 characters
                        crate::terminal::print_char(' ');
                    }
                    crate::terminal::print("\r");
                    // Reprint prompt and command buffer
                    self.print_prompt();
                    crate::terminal::print(&self.command_buffer);
                }
            }
            '\x7F' => { // Delete
                if self.cursor_position < self.command_buffer.len() {
                    self.command_buffer.remove(self.cursor_position);
                    self.redraw_line();
                }
            }
            _ if c.is_ascii() && !c.is_control() => {
                if self.command_buffer.len() < MAX_COMMAND_LENGTH {
                    self.command_buffer.insert(self.cursor_position, c);
                    self.cursor_position += 1;
                    crate::terminal::print_char(c);
                }
            }
            _ => {} // Ignore other characters
        }
    }
    
    fn execute_command(&mut self) {
        let command = self.command_buffer.trim();
        
        // Add to history
        if !command.is_empty() && (self.history.is_empty() || self.history.last() != Some(&command.into())) {
            if self.history.len() >= HISTORY_SIZE {
                self.history.remove(0);
            }
            self.history.push(command.into());
        }
        self.history_index = self.history.len();
        
        // Parse and execute command
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            return;
        }
        
        let cmd_name = parts[0];
        let args = &parts[1..];
        
        match cmd_name {
            "help" => self.cmd_help(),
            "clear" => self.cmd_clear(),
            "echo" => self.cmd_echo(args),
            "version" => self.cmd_version(),
            "uptime" => self.cmd_uptime(),
            "history" => self.cmd_history(),
            "shutdown" => self.cmd_shutdown(),
            "reboot" => self.cmd_reboot(),
            // Filesystem commands with unified argument handling
            "ls" => self.cmd_ls(self.get_first_arg(args, "")),
            "cd" => self.cmd_cd(self.get_first_arg(args, "/")),
            "pwd" => self.cmd_pwd(),
            "cat" => self.cmd_with_required_path("cat", args, Self::cmd_cat),
            "mkdir" => self.cmd_with_required_path("mkdir", args, Self::cmd_mkdir),
            "rm" => self.cmd_with_required_path("rm", args, Self::cmd_rm),
            "write" => self.cmd_write(args),
            "" => {}, // Empty command
            _ => {
                crate::terminal::print("Unknown command: ");
                crate::terminal::print(cmd_name);
                crate::terminal::print("\nType 'help' for available commands.\n");
            }
        }
    }
    
    // Helper methods for unified argument handling
    fn get_first_arg<'a>(&self, args: &'a [&str], default: &'a str) -> &'a str {
        if !args.is_empty() && !args[0].is_empty() {
            args[0]
        } else {
            default
        }
    }
    
    fn cmd_with_required_path(&self, cmd_name: &str, args: &[&str], f: fn(&Self, &str)) {
        if args.is_empty() || args[0].is_empty() {
            crate::terminal::print("Usage: ");
            crate::terminal::print(cmd_name);
            crate::terminal::print(" <path>\n");
        } else {
            f(self, args[0]);
        }
    }
    
    fn print_prompt(&self) {
        crate::terminal::print("ilo-lawa> ");
    }
    
    fn redraw_line(&self) {
        // Move cursor back and reprint the line
        crate::terminal::print("\r");
        // Clear the line with spaces
        for _ in 0..(10 + self.command_buffer.len()) {
            crate::terminal::print_char(' ');
        }
        crate::terminal::print("\r");
        // Reprint prompt and buffer
        self.print_prompt();
        crate::terminal::print(&self.command_buffer);
    }
    
    // Built-in commands
    fn cmd_help(&self) {
        crate::terminal::print("Available commands:\n");
        crate::terminal::print("\nSystem Commands:\n");
        crate::terminal::print("  help     - Show this help message\n");
        crate::terminal::print("  clear    - Clear the screen\n");
        crate::terminal::print("  echo     - Print arguments to screen\n");
        crate::terminal::print("  version  - Show OS version\n");
        crate::terminal::print("  uptime   - Show system uptime\n");
        crate::terminal::print("  history  - Show command history\n");
        crate::terminal::print("  shutdown - Shutdown the system\n");
        crate::terminal::print("  reboot   - Reboot the system\n");
        crate::terminal::print("\nFilesystem Commands:\n");
        crate::terminal::print("  ls       - List directory contents\n");
        crate::terminal::print("  cd       - Change directory\n");
        crate::terminal::print("  pwd      - Print working directory\n");
        crate::terminal::print("  cat      - Display file contents\n");
        crate::terminal::print("  mkdir    - Create directory\n");
        crate::terminal::print("  rm       - Remove file\n");
        crate::terminal::print("  write    - Create file with content\n");
    }
    
    fn cmd_clear(&self) {
        // Clear screen implementation will be in terminal module
        crate::terminal::clear();
    }
    
    fn cmd_echo(&self, args: &[&str]) {
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                crate::terminal::print(" ");
            }
            crate::terminal::print(arg);
        }
        crate::terminal::print("\n");
    }
    
    fn cmd_version(&self) {
        crate::terminal::print("ilo-lawa OS v0.4.0\n");
        crate::terminal::print("Built with Rust and UEFI\n");
    }
    
    fn cmd_uptime(&self) {
        // TODO: Implement actual uptime tracking
        crate::terminal::print("System has been running for: [not implemented]\n");
    }
    
    fn cmd_history(&self) {
        if self.history.is_empty() {
            crate::terminal::print("No command history.\n");
        } else {
            for (i, cmd) in self.history.iter().enumerate() {
                crate::terminal::print("  ");
                // Print index
                let mut num = i + 1;
                let mut digits = [0u8; 3];
                let mut idx = 2;
                while num > 0 && idx < 3 {
                    digits[idx] = (num % 10) as u8 + b'0';
                    num /= 10;
                    if idx > 0 {
                        idx -= 1;
                    } else {
                        break;
                    }
                }
                for d in &digits[idx+1..] {
                    crate::terminal::print_char(*d as char);
                }
                crate::terminal::print("  ");
                crate::terminal::print(cmd);
                crate::terminal::print("\n");
            }
        }
    }
    
    fn cmd_shutdown(&self) {
        crate::terminal::print("Shutting down...\n");
        // TODO: Proper shutdown sequence
        loop {
            x86_64::instructions::hlt();
        }
    }
    
    fn cmd_reboot(&self) {
        crate::terminal::print("Rebooting...\n");
        // Trigger a reboot via the keyboard controller
        use x86_64::instructions::port::Port;
        unsafe {
            let mut port = Port::new(0x64);
            port.write(0xFEu8);
        }
        // If that doesn't work, just halt
        loop {
            x86_64::instructions::hlt();
        }
    }
    
    // Filesystem commands
    fn cmd_ls(&self, path: &str) {
        let files = crate::fs::list_directory(path);
        if files.is_empty() {
            crate::terminal::print("(empty directory)\n");
        } else {
            for file in files {
                crate::terminal::print(&file);
                crate::terminal::print("\n");
            }
        }
    }
    
    fn cmd_cd(&self, path: &str) {
        match crate::fs::change_directory(path) {
            Ok(_) => {},
            Err(e) => {
                crate::terminal::print("cd: ");
                crate::terminal::print(&e);
                crate::terminal::print("\n");
            }
        }
    }
    
    fn cmd_pwd(&self) {
        let path = crate::fs::get_current_path();
        crate::terminal::print(&path);
        crate::terminal::print("\n");
    }
    
    fn cmd_cat(&self, filename: &str) {
        if filename.is_empty() {
            crate::terminal::print("Usage: cat <filename>\n");
            return;
        }
        
        match crate::fs::read_file(filename) {
            Ok(data) => {
                // Convert bytes to string for display
                if let Ok(content) = String::from_utf8(data) {
                    crate::terminal::print(&content);
                    if !content.ends_with('\n') {
                        crate::terminal::print("\n");
                    }
                } else {
                    crate::terminal::print("(binary file)\n");
                }
            }
            Err(e) => {
                crate::terminal::print("cat: ");
                crate::terminal::print(&e);
                crate::terminal::print("\n");
            }
        }
    }
    
    fn cmd_mkdir(&self, dirname: &str) {
        if dirname.is_empty() {
            crate::terminal::print("Usage: mkdir <dirname>\n");
            return;
        }
        
        match crate::fs::create_directory(dirname) {
            Ok(_) => {},
            Err(e) => {
                crate::terminal::print("mkdir: ");
                crate::terminal::print(&e);
                crate::terminal::print("\n");
            }
        }
    }
    
    fn cmd_rm(&self, filename: &str) {
        if filename.is_empty() {
            crate::terminal::print("Usage: rm <filename>\n");
            return;
        }
        
        match crate::fs::remove_file(filename) {
            Ok(_) => {},
            Err(e) => {
                crate::terminal::print("rm: ");
                crate::terminal::print(&e);
                crate::terminal::print("\n");
            }
        }
    }
    
    fn cmd_write(&self, args: &[&str]) {
        if args.len() < 2 {
            crate::terminal::print("Usage: write <filename> <content>\n");
            return;
        }
        
        let filename = args[0];
        let content = args[1..].join(" ");
        
        match crate::fs::create_file(filename, content.into_bytes()) {
            Ok(_) => {
                crate::terminal::print("File created: ");
                crate::terminal::print(filename);
                crate::terminal::print("\n");
            }
            Err(e) => {
                crate::terminal::print("write: ");
                crate::terminal::print(&e);
                crate::terminal::print("\n");
            }
        }
    }
}

// Public interface
pub fn init() {
    crate::terminal::print("\n");
    crate::terminal::print("ilo-lawa Shell v0.1.0\n");
    crate::terminal::print("Type 'help' for available commands.\n");
    crate::terminal::print("\n");
    SHELL.lock().print_prompt();
}

pub fn handle_input(c: char) {
    SHELL.lock().handle_key(c);
}