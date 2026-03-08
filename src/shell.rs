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
                    self.redraw_line();
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
        
        match parts[0] {
            "help" => self.cmd_help(),
            "clear" => self.cmd_clear(),
            "echo" => self.cmd_echo(&parts[1..]),
            "version" => self.cmd_version(),
            "uptime" => self.cmd_uptime(),
            "history" => self.cmd_history(),
            "shutdown" => self.cmd_shutdown(),
            "reboot" => self.cmd_reboot(),
            "" => {}, // Empty command
            _ => {
                crate::terminal::print("Unknown command: ");
                crate::terminal::print(parts[0]);
                crate::terminal::print("\nType 'help' for available commands.\n");
            }
        }
    }
    
    fn print_prompt(&self) {
        crate::terminal::print("ilo-lawa> ");
    }
    
    fn redraw_line(&self) {
        // Move cursor to beginning of line
        crate::terminal::print("\r");
        self.print_prompt();
        crate::terminal::print(&self.command_buffer);
        crate::terminal::print(" "); // Clear any remaining characters
        crate::terminal::print("\r");
        self.print_prompt();
        
        // Move cursor to correct position
        for i in 0..self.cursor_position {
            crate::terminal::print_char(self.command_buffer.chars().nth(i).unwrap_or(' '));
        }
    }
    
    // Built-in commands
    fn cmd_help(&self) {
        crate::terminal::print("Available commands:\n");
        crate::terminal::print("  help     - Show this help message\n");
        crate::terminal::print("  clear    - Clear the screen\n");
        crate::terminal::print("  echo     - Print arguments to screen\n");
        crate::terminal::print("  version  - Show OS version\n");
        crate::terminal::print("  uptime   - Show system uptime\n");
        crate::terminal::print("  history  - Show command history\n");
        crate::terminal::print("  shutdown - Shutdown the system\n");
        crate::terminal::print("  reboot   - Reboot the system\n");
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