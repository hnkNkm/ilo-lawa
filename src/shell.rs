// Simple shell implementation for ilo-lawa OS

use crate::keyboard::KeyEvent;
use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;
use lazy_static::lazy_static;

const MAX_COMMAND_LENGTH: usize = 256;
const HISTORY_SIZE: usize = 10;
const PROMPT: &str = "ilo-lawa> ";

lazy_static! {
    static ref SHELL: Mutex<Shell> = Mutex::new(Shell::new());
}

pub struct Shell {
    command_buffer: String,
    history: Vec<String>,
    history_index: usize,
    /// Byte index into command_buffer where the next edit lands (commands
    /// are ASCII, so bytes and chars coincide).
    cursor_position: usize,
    /// What was being typed before the first Up recalled a history entry;
    /// restored when Down moves past the newest entry.
    draft: String,
    /// Screen cells the last render of the input line occupied (prompt +
    /// buffer), so a redraw clears exactly what is on screen.
    rendered_cells: usize,
    /// Terminal row where the current input line starts.
    input_start_y: usize,
}

impl Shell {
    pub fn new() -> Self {
        Shell {
            command_buffer: String::new(),
            history: Vec::with_capacity(HISTORY_SIZE),
            history_index: 0,
            cursor_position: 0,
            draft: String::new(),
            rendered_cells: 0,
            input_start_y: 0,
        }
    }

    /// Handle one decoded key event. Thread context only: every path below
    /// takes the terminal lock, so this must never run inside an ISR.
    pub fn handle_key_event(&mut self, event: KeyEvent) {
        match event {
            KeyEvent::Char(c) => self.handle_char(c),
            KeyEvent::Enter => self.handle_enter(),
            KeyEvent::Backspace => {
                if self.cursor_position > 0 && !self.command_buffer.is_empty() {
                    self.cursor_position -= 1;
                    self.command_buffer.remove(self.cursor_position);
                    self.redraw_input();
                }
            }
            KeyEvent::Delete => {
                if self.cursor_position < self.command_buffer.len() {
                    self.command_buffer.remove(self.cursor_position);
                    self.redraw_input();
                }
            }
            KeyEvent::Left => {
                if self.cursor_position > 0 {
                    self.cursor_position -= 1;
                    self.redraw_input();
                }
            }
            KeyEvent::Right => {
                if self.cursor_position < self.command_buffer.len() {
                    self.cursor_position += 1;
                    self.redraw_input();
                }
            }
            KeyEvent::Home => {
                self.cursor_position = 0;
                self.redraw_input();
            }
            KeyEvent::End => {
                self.cursor_position = self.command_buffer.len();
                self.redraw_input();
            }
            KeyEvent::Up => self.history_previous(),
            KeyEvent::Down => self.history_next(),
            KeyEvent::CtrlC => {
                crate::terminal::print("^C\n");
                self.reset_input_line();
                self.print_prompt();
            }
            KeyEvent::CtrlL => {
                crate::terminal::clear();
                self.print_prompt();
                if !self.command_buffer.is_empty() {
                    self.redraw_input();
                }
            }
        }
    }

    fn handle_char(&mut self, c: char) {
        // ASCII-only semantics; control characters are ignored.
        if !c.is_ascii() || c.is_control() {
            return;
        }
        if self.command_buffer.len() >= MAX_COMMAND_LENGTH {
            return;
        }
        self.command_buffer.insert(self.cursor_position, c);
        self.cursor_position += 1;
        if self.cursor_position == self.command_buffer.len() {
            // Typed at the end of the line: just echo the character.
            crate::terminal::print_char(c);
            self.rendered_cells += 1;
        } else {
            // Inserted mid-line: redraw the shifted remainder.
            self.redraw_input();
        }
    }

    fn handle_enter(&mut self) {
        // Execute command (empty input just moves to a fresh line, so the
        // prompt is never doubled).
        crate::terminal::print("\n");
        self.execute_command();
        self.reset_input_line();
        self.print_prompt();
    }

    /// Clear the editing state and rewind history navigation (after a
    /// command executed or Ctrl+C aborted the line).
    fn reset_input_line(&mut self) {
        self.command_buffer.clear();
        self.cursor_position = 0;
        self.draft.clear();
        self.history_index = self.history.len();
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() || self.history_index == 0 {
            return;
        }
        // Leaving the fresh line: remember what was being typed so Down can
        // restore it when navigating back past the newest entry.
        if self.history_index == self.history.len() {
            self.draft = self.command_buffer.clone();
        }
        self.history_index -= 1;
        self.recall_history_entry();
    }

    fn history_next(&mut self) {
        if self.history_index >= self.history.len() {
            return; // Already at the fresh-line position.
        }
        self.history_index += 1;
        if self.history_index == self.history.len() {
            // Past the newest entry: restore the draft (empty when the
            // first Up started from a fresh line).
            let draft = core::mem::take(&mut self.draft);
            self.set_buffer(draft);
        } else {
            self.recall_history_entry();
        }
    }

    fn recall_history_entry(&mut self) {
        let entry = self.history[self.history_index].clone();
        self.set_buffer(entry);
    }

    /// Replace the buffer with `s` and put the cursor at its end.
    fn set_buffer(&mut self, s: String) {
        self.command_buffer = s;
        self.cursor_position = self.command_buffer.len();
        self.redraw_input();
    }

    /// Redraw the input line in place and remember how many screen cells
    /// the render occupies. The framebuffer is updated without ANSI
    /// escapes; the serial mirror does not see redraws.
    fn redraw_input(&mut self) {
        let start_y = self.input_start_y;
        let previous_cells = self.rendered_cells;
        crate::terminal::redraw_input(
            PROMPT,
            &self.command_buffer,
            self.cursor_position,
            start_y,
            previous_cells,
        );
        self.rendered_cells = PROMPT.len() + self.command_buffer.len();
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
            "sleep" => self.cmd_sleep(args),
            "history" => self.cmd_history(),
            "shutdown" => self.cmd_shutdown(),
            "reboot" => self.cmd_reboot(),
            "lspci" => self.cmd_lspci(args),
            "tasks" => self.cmd_tasks(),
            "taskdemo" => self.cmd_taskdemo(),
            // Filesystem commands with unified argument handling
            "ls" => self.cmd_ls(self.get_first_arg(args, "")),
            "cd" => self.cmd_cd(self.get_first_arg(args, "/")),
            "pwd" => self.cmd_pwd(),
            "cat" => self.cmd_with_required_path("cat", args, Self::cmd_cat),
            "mkdir" => self.cmd_with_required_path("mkdir", args, Self::cmd_mkdir),
            "rm" => self.cmd_with_required_path("rm", args, Self::cmd_rm),
            "rmdir" => self.cmd_with_required_path("rmdir", args, Self::cmd_rmdir),
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
    
    fn print_prompt(&mut self) {
        // Record where the input line starts and reset the rendered-cells
        // count so later redraws clear exactly what is on screen.
        self.input_start_y = crate::terminal::cursor_y();
        self.rendered_cells = PROMPT.len();
        crate::terminal::print(PROMPT);
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
        crate::terminal::print("  sleep    - Sleep for <ms> milliseconds\n");
        crate::terminal::print("  history  - Show command history\n");
        crate::terminal::print("  shutdown - Shutdown the system\n");
        crate::terminal::print("  reboot   - Reboot the system\n");
        crate::terminal::print("  lspci    - List PCI devices (-v for details)\n");
        crate::terminal::print("  tasks    - List tasks (id, name, state)\n");
        crate::terminal::print("  taskdemo - Spawn demo/stress/counter/sleeper tasks\n");
        crate::terminal::print("\nFilesystem Commands:\n");
        crate::terminal::print("  ls       - List directory contents\n");
        crate::terminal::print("  cd       - Change directory\n");
        crate::terminal::print("  pwd      - Print working directory\n");
        crate::terminal::print("  cat      - Display file contents\n");
        crate::terminal::print("  mkdir    - Create directory\n");
        crate::terminal::print("  rm       - Remove file\n");
        crate::terminal::print("  rmdir    - Remove an empty directory\n");
        crate::terminal::print("  write    - Create file with content (overwrites existing file)\n");
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
        crate::terminal::print(concat!("ilo-lawa OS v", env!("CARGO_PKG_VERSION"), "\n"));
        crate::terminal::print("Built with Rust and UEFI\n");
    }

    // List the devices found by pci::init() at boot (issue #23). Default
    // output is one line per function: bus:dev.func vendor:device
    // class:subclass prog_if, with virtio-blk devices marked; they also
    // get their BAR0/IRQ printed since that is what the block-driver work
    // (issue #14) needs. `lspci -v` prints every non-zero BAR plus IRQ
    // routing for all devices.
    fn cmd_lspci(&self, args: &[&str]) {
        let verbose = args.iter().any(|arg| *arg == "-v" || *arg == "--verbose");
        let mut count = 0usize;
        crate::drivers::pci::for_each_device(|dev| {
            count += 1;
            let marker = if dev.is_virtio_block() { "  [virtio-blk]" } else { "" };
            crate::terminal::print_fmt(format_args!(
                "{:02x}:{:02x}.{} {:04x}:{:04x} {:02x}:{:02x} {:02x}{}\n",
                dev.bus,
                dev.device,
                dev.function,
                dev.vendor_id,
                dev.device_id,
                dev.class_code,
                dev.subclass,
                dev.prog_if,
                marker,
            ));
            if verbose || dev.is_virtio_block() {
                let bar_count = if verbose { dev.bars.len() } else { 1 };
                // Step by hand: a 64-bit memory BAR consumes two slots and
                // is printed as one joined address instead of two raw
                // halves.
                let mut i = 0;
                while i < bar_count {
                    let bar = dev.bars[i];
                    if bar == 0 {
                        i += 1;
                        continue;
                    }
                    if dev.bar_is_64bit(i) && i + 1 < dev.bars.len() {
                        let base =
                            (u64::from(dev.bars[i + 1]) << 32) | u64::from(dev.bar_base(i));
                        crate::terminal::print_fmt(format_args!(
                            "    BAR{} mem64 @ {:#x}\n",
                            i, base,
                        ));
                        i += 2;
                    } else {
                        let kind = if dev.bar_is_io(i) { "io" } else { "mem" };
                        crate::terminal::print_fmt(format_args!(
                            "    BAR{} {} @ {:#x}\n",
                            i,
                            kind,
                            dev.bar_base(i),
                        ));
                        i += 1;
                    }
                }
                crate::terminal::print_fmt(format_args!(
                    "    IRQ line {} pin {}\n",
                    dev.interrupt_line,
                    dev.interrupt_pin,
                ));
            }
        });
        if crate::drivers::pci::overflowed() {
            crate::terminal::print("(device table full, some functions not listed)\n");
        }
        crate::terminal::print_fmt(format_args!("{} device(s)\n", count));
    }
    
    fn cmd_uptime(&self) {
        // days + HH:MM:SS, decimal digits printed directly — no floats,
        // no format! allocations (issue #7).
        let secs = crate::time::uptime_seconds();
        let days = secs / 86_400;
        let hours = (secs % 86_400) / 3_600;
        let mins = (secs % 3_600) / 60;
        let secs = secs % 60;
        crate::terminal::print("System uptime: ");
        print_u64(days);
        crate::terminal::print_char('d');
        crate::terminal::print_char(' ');
        print_two_digits(hours);
        crate::terminal::print_char(':');
        print_two_digits(mins);
        crate::terminal::print_char(':');
        print_two_digits(secs);
        crate::terminal::print_char('\n');
    }

    fn cmd_sleep(&self, args: &[&str]) {
        let arg = self.get_first_arg(args, "");
        if arg.is_empty() {
            crate::terminal::print("Usage: sleep <ms>\n");
            return;
        }
        match arg.parse::<u64>() {
            Ok(ms) => {
                // Thread-context sleep on the PIT tick; wakes at the next
                // tick boundary at or after `ms` (10 ms resolution).
                crate::time::sleep_ms(ms);
                crate::terminal::print("Slept ");
                crate::terminal::print(arg);
                crate::terminal::print(" ms\n");
            }
            Err(_) => {
                crate::terminal::print("sleep: invalid duration '");
                crate::terminal::print(arg);
                crate::terminal::print("'\nUsage: sleep <ms>\n");
            }
        }
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
        // ACPI S5 power-off (see src/acpi.rs); halts when no usable FADT.
        crate::acpi::shutdown();
    }
    
    fn cmd_reboot(&self) {
        crate::terminal::print("Rebooting...\n");
        // FADT reset register, then the 8042 keyboard controller (see
        // src/acpi.rs); halts when both fail.
        crate::acpi::reboot();
    }

    /// List every task in the scheduler table (id, name, state), marking the
    /// currently running one. Thread context; the snapshot allocates, which
    /// is fine here but would be forbidden in an ISR.
    fn cmd_tasks(&self) {
        let (tasks, current) = crate::task::snapshot();
        if tasks.is_empty() {
            crate::terminal::print("No tasks (scheduler not initialized?).\n");
            return;
        }
        crate::terminal::print("ID   NAME        STATE\n");
        for (id, name, state) in tasks {
            let marker = if id == current { "*" } else { " " };
            crate::terminal::print_fmt(format_args!(
                "{}{:<4} {:<11} {:?}\n",
                marker, id, name, state
            ));
        }
    }

    /// Spawn the demo task suite (demo/stress/counter/sleeper, see
    /// task::demo) and return immediately: the tasks are only queued here —
    /// the main loop runs them once this command handler has returned and the
    /// SHELL lock is released, which is why nothing here may wait for them.
    fn cmd_taskdemo(&self) {
        match crate::task::demo::spawn_suite(true) {
            Ok(()) => {
                crate::terminal::print(
                    "taskdemo: spawned demo, stress, counter, sleeper — 4 tasks ready\n",
                );
            }
            Err(e) => {
                crate::terminal::print("taskdemo: spawn failed: ");
                crate::terminal::print(e);
                crate::terminal::print("\n");
            }
        }
    }
    
    // Filesystem commands
    fn cmd_ls(&self, path: &str) {
        match crate::fs::list_directory(path) {
            Ok(files) => {
                if files.is_empty() {
                    crate::terminal::print("(empty directory)\n");
                } else {
                    for file in files {
                        crate::terminal::print(&file);
                        crate::terminal::print("\n");
                    }
                }
            }
            Err(e) => {
                crate::terminal::print("ls: ");
                crate::terminal::print(&e);
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
            Ok(_) => {}
            Err(e) => {
                crate::terminal::print("rm: ");
                crate::terminal::print(&e);
                crate::terminal::print("\n");
            }
        }
    }

    fn cmd_rmdir(&self, dirname: &str) {
        if dirname.is_empty() {
            crate::terminal::print("Usage: rmdir <dirname>\n");
            return;
        }

        match crate::fs::remove_directory(dirname) {
            Ok(_) => {}
            Err(e) => {
                crate::terminal::print("rmdir: ");
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
                crate::terminal::print("File written: ");
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

// Print a u64 in decimal directly to the terminal — no allocator use.
fn print_u64(mut value: u64) {
    if value == 0 {
        crate::terminal::print_char('0');
        return;
    }
    let mut digits = [0u8; 20]; // enough for u64::MAX
    let mut i = digits.len();
    while value > 0 && i > 0 {
        i -= 1;
        digits[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    for d in &digits[i..] {
        crate::terminal::print_char(*d as char);
    }
}

// Print a value zero-padded to two digits (HH:MM:SS fields).
fn print_two_digits(value: u64) {
    crate::terminal::print_char((b'0' + (value / 10) as u8) as char);
    crate::terminal::print_char((b'0' + (value % 10) as u8) as char);
}

// Public interface
pub fn init() {
    crate::terminal::print("\n");
    crate::terminal::print(concat!("ilo-lawa Shell v", env!("CARGO_PKG_VERSION"), "\n"));
    crate::terminal::print("Type 'help' for available commands.\n");
    crate::terminal::print("\n");
    SHELL.lock().print_prompt();
}

pub fn handle_key_event(event: KeyEvent) {
    SHELL.lock().handle_key_event(event);
}