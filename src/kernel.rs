#![allow(dead_code)]

use core::arch::asm;
use crate::font::get_char_bitmap;
use spin::Once;

// Kernel-owned copy of the UEFI GOP pixel format. Exists so kernel drawing
// never depends on the `uefi` crate type (which is unusable after
// ExitBootServices anyway). Captured in src/main.rs before EBS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    /// 32 bpp; memory byte order B,G,R,reserved (QEMU's default GOP format).
    /// Writing a little-endian u32 0x00RRGGBB as-is is correct here.
    Bgr,
    /// 32 bpp; memory byte order R,G,B,reserved. Red and blue are swapped
    /// relative to Bgr.
    Rgb,
    /// Custom format described by per-channel bitmasks. Zero-width masks
    /// (no bits for a channel) are allowed and simply contribute nothing.
    Bitmask {
        red_mask: u32,
        green_mask: u32,
        blue_mask: u32,
    },
    /// No linear framebuffer exists (Blt-only GOP mode). The bootloader
    /// refuses to enter the kernel in this mode, so drawing never sees it.
    BltOnly,
}

/// Encode a 0x00RRGGBB color into the native pixel value `format` expects.
///
/// - `Bgr`: the value is written unchanged (byte 0 = blue, matching the
///   current 32 bpp BGRX behavior).
/// - `Rgb`: red and blue bytes are swapped.
/// - `Bitmask`: each 8-bit channel is extracted, aligned to its mask's
///   lowest set bit, and scaled from 0..=255 to the mask's full width.
///   Zero masks contribute 0 (safe, no shift-by-64 or division by zero).
/// - `BltOnly`: unreachable in the kernel; returns 0.
pub fn encode_pixel(format: &PixelFormat, rgb: u32) -> u32 {
    let r = ((rgb >> 16) & 0xFF) as u8;
    let g = ((rgb >> 8) & 0xFF) as u8;
    let b = (rgb & 0xFF) as u8;
    match *format {
        PixelFormat::Bgr => rgb & 0x00FF_FFFF,
        PixelFormat::Rgb => {
            (u32::from(b) << 16) | (rgb & 0x0000_FF00) | u32::from(r)
        }
        PixelFormat::Bitmask {
            red_mask,
            green_mask,
            blue_mask,
        } => {
            encode_channel(r, red_mask)
                | encode_channel(g, green_mask)
                | encode_channel(b, blue_mask)
        }
        PixelFormat::BltOnly => 0,
    }
}

/// Scale an 8-bit `channel` onto the contiguous bit `mask` (per the UEFI
/// spec masks are contiguous), aligned to the mask's lowest set bit.
/// Returns 0 for a zero-width mask.
fn encode_channel(channel: u8, mask: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    // Width of the contiguous run starting at the lowest set bit: the
    // shifted mask's bit width.
    let width = 32 - (mask >> shift).leading_zeros();
    let max = if width >= 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    };
    // Best-fit scale 0..=255 -> 0..=max (exact identity for max == 255);
    // 64-bit math because channel * max can exceed u32.
    (((u32::from(channel) as u64 * max as u64) / 255) as u32) << shift
}

// Framebuffer information that survives ExitBootServices
#[derive(Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub pixel_format: PixelFormat,
}

// Loaded-image bounds captured before ExitBootServices (LoadedImage
// protocol). Zeroed when the protocol could not be opened; paging::init
// skips W^X in that case.
#[derive(Clone, Copy)]
pub struct ImageInfo {
    pub base: u64,
    pub size: u64,
}

// ACPI RSDP physical address captured from the UEFI configuration table
// BEFORE ExitBootServices (0 = firmware published no ACPI tables). Tables
// are identity-mapped after paging::init, so acpi::init can parse them
// from this bare physical address with no heap.
#[derive(Clone, Copy)]
pub struct AcpiInfo {
    pub rsdp_addr: u64,
}

// Simple kernel graphics driver
pub struct KernelGraphics {
    fb_info: FramebufferInfo,
}

impl KernelGraphics {
    pub fn new(fb_info: FramebufferInfo) -> Self {
        KernelGraphics { fb_info }
    }
    
    pub fn clear(&self, color: u32) {
        // Encode once; every written pixel gets the same native value.
        let color = encode_pixel(&self.fb_info.pixel_format, color);
        let fb = self.fb_info.base_address as *mut u32;
        for i in 0..(self.fb_info.height * self.fb_info.stride) {
            // Volatile: the framebuffer is UC-mapped MMIO; the compiler
            // must not elide or reorder these stores.
            unsafe { fb.add(i).write_volatile(color) };
        }
    }

    /// Shift the whole framebuffer up by `pixels` scanlines and fill the
    /// newly exposed bottom strip with `fill_color`. Used for text scrolling:
    /// the caller passes CHAR_HEIGHT + LINE_SPACING so exactly one text row
    /// moves up. Everything above the strip is moved one u32 at a time with
    /// volatile reads/writes — the framebuffer is UC-mapped MMIO, so
    /// `core::ptr::copy` (non-volatile memmove) must not be used.
    pub fn scroll_up(&self, pixels: usize, fill_color: u32) {
        let width = self.fb_info.width;
        let height = self.fb_info.height;
        let stride = self.fb_info.stride;

        // Degenerate framebuffer or oversized scroll: nothing sane to move.
        if pixels == 0 || width == 0 || height == 0 || stride == 0 {
            return;
        }
        if pixels >= height {
            self.clear(fill_color);
            return;
        }

        let fb = self.fb_info.base_address as *mut u32;
        // Move everything below the scrolled-off strip up, pixel by pixel.
        // One u32 per pixel; `stride` covers the padding between rows too.
        // Scrolling up means destination < source, so a forward walk copies
        // correctly even with the overlapping ranges.
        unsafe {
            let src = fb.add(pixels * stride);
            for i in 0..(height - pixels) * stride {
                let pixel = src.add(i).read_volatile();
                fb.add(i).write_volatile(pixel);
            }
        }

        // Clear the strip that was exposed at the bottom.
        self.draw_rect(0, height - pixels, width, pixels, fill_color);
    }
    
    pub fn draw_pixel(&self, x: usize, y: usize, color: u32) {
        if x >= self.fb_info.width || y >= self.fb_info.height {
            return;
        }
        
        let native = encode_pixel(&self.fb_info.pixel_format, color);
        unsafe {
            let fb = self.fb_info.base_address as *mut u32;
            let offset = y * self.fb_info.stride + x;
            fb.add(offset).write_volatile(native);
        }
    }
    
    pub fn draw_rect(&self, x: usize, y: usize, width: usize, height: usize, color: u32) {
        for dy in 0..height {
            for dx in 0..width {
                self.draw_pixel(x + dx, y + dy, color);
            }
        }
    }
    
    pub fn draw_char(&self, x: usize, y: usize, ch: char, color: u32, bg_color: Option<u32>) {
        let bitmap = get_char_bitmap(ch);
        
        for (row_idx, &row_data) in bitmap.iter().enumerate() {
            for bit in 0..8 {
                let pixel_x = x + bit;
                let pixel_y = y + row_idx;
                
                if pixel_x >= self.fb_info.width || pixel_y >= self.fb_info.height {
                    continue;
                }
                
                // Check if bit is set (pixel should be drawn)
                // Font data uses bit 0 as leftmost pixel, so use (1 << bit)
                if row_data & (1 << bit) != 0 {
                    self.draw_pixel(pixel_x, pixel_y, color);
                } else if let Some(bg) = bg_color {
                    self.draw_pixel(pixel_x, pixel_y, bg);
                }
            }
        }
    }
    
    pub fn draw_text(&self, x: usize, y: usize, text: &str, color: u32, bg_color: Option<u32>) {
        let mut cx = x;
        for ch in text.chars() {
            if ch == '\n' {
                return; // Simple newline handling
            }
            self.draw_char(cx, y, ch, color, bg_color);
            cx += 8; // Move to next character position (8 pixels)
        }
    }
}

// Boot arguments parked by `kernel_main` before it abandons the firmware
// stack; consumed by `kernel_main_owned` on the kernel-owned stack. The
// memory map is moved exactly once — into this slot — and afterwards only
// read through the shared reference; it is never dropped (the slot lives for
// the kernel's lifetime, matching the "map is retained" contract).
struct BootArgs {
    fb_info: FramebufferInfo,
    memory_map: uefi::table::boot::MemoryMap<'static>,
    image: ImageInfo,
    acpi: AcpiInfo,
}

static BOOT_ARGS: Once<BootArgs> = Once::new();

// The kernel entry point after ExitBootServices. Its only jobs are to park
// the boot arguments and to switch from the firmware stack to the
// kernel-owned stack (src/stacks.rs); the whole init sequence lives in
// `kernel_main_owned`, so not a single init step runs on firmware stack
// memory. No UEFI memory is reserved for the abandoned stack, so the frame
// allocator (brought up later, in memory::init) starts from a clean slate.
pub fn kernel_main(
    fb_info: FramebufferInfo,
    memory_map: uefi::table::boot::MemoryMap<'static>,
    image: ImageInfo,
    acpi: AcpiInfo,
) -> ! {
    // CRITICAL: Disable interrupts immediately after ExitBootServices
    x86_64::instructions::interrupts::disable();

    // Bring up COM1 first so boot output reaches the host even before
    // the framebuffer terminal exists
    crate::serial::init();

    // Park the boot arguments, then leave the firmware stack for good.
    let firmware_rsp = crate::stacks::current_stack_pointer();
    BOOT_ARGS.call_once(|| BootArgs { fb_info, memory_map, image, acpi });
    let kernel_top = crate::stacks::main_stack_top();
    crate::serial::print_fmt(format_args!(
        "[stack] firmware rsp={:#x} -> kernel-owned stack top={:#x}, switching\n",
        firmware_rsp, kernel_top
    ));

    // Safety: the boot arguments are parked above, and the continuation
    // never returns.
    unsafe { crate::stacks::switch_to_kernel_stack() }
}

// Continuation of kernel_main, running on the kernel-owned main stack
// (entered via a `call` from stacks::switch_to_kernel_stack, so RSP follows
// the normal call contract). The UEFI memory map is retained in BOOT_ARGS
// (see memory::init) rather than discarded.
// `unreachable_code`: with feature `test-harness` the harness call below
// diverges, so the remainder of the init sequence is unreachable there.
#[allow(unreachable_code)]
pub extern "C" fn kernel_main_owned() -> ! {
    let args = BOOT_ARGS
        .get()
        .expect("boot args must be parked before the stack switch");
    let fb_info = args.fb_info;

    crate::serial::print_fmt(format_args!(
        "[stack] running on kernel-owned stack, rsp={:#x}\n",
        crate::stacks::current_stack_pointer()
    ));

    // Initialize terminal first
    crate::terminal::init(fb_info);
    
    // Clear screen and show boot messages
    crate::terminal::print(concat!("ilo-lawa OS v", env!("CARGO_PKG_VERSION"), "\n"));
    crate::terminal::print("===================\n\n");
    crate::terminal::print("System Initialization:\n");
    crate::terminal::print("[OK] Interrupts disabled\n");

    // Install the kernel-owned identity page tables: guards the stack
    // regions and marks the framebuffer MMIO uncached, before anything else
    // allocates or maps memory (see src/paging.rs).
    crate::terminal::print("Installing kernel page tables...");
    crate::paging::init(fb_info, &args.memory_map, args.image);
    crate::terminal::print(" [OK]\n");

    // Parse the ACPI RSDP -> RSDT/XSDT -> FADT chain for shutdown/reboot.
    // The tables are identity-mapped by paging::init, so this needs no
    // heap and runs before the allocator exists; a malformed table only
    // disables ACPI power control (halt/8042 fallbacks stay available).
    crate::terminal::print("Initializing ACPI (RSDP/FADT)...");
    crate::acpi::init(args.acpi.rsdp_addr);
    crate::terminal::print(" [OK]\n");

    // Retain the UEFI memory map and bring up the physical frame
    // allocator BEFORE the heap (thread context, interrupts still
    // disabled): the frame summary/self-check must run on the untouched
    // pool, and the heap below is carved out of that same pool.
    crate::terminal::print("Initializing memory management...\n");
    crate::memory::init(&args.memory_map);
    crate::terminal::print(" [OK]\n");

    // Size the heap from conventional RAM and reserve its backing frames
    // (static 1 MiB fallback if no contiguous run exists). This must
    // happen before the filesystem and shell below — the first heap
    // users — ever allocate.
    crate::terminal::print("Initializing heap allocator...");
    crate::allocator::init_heap_from_memory();
    crate::terminal::print(" [OK]\n");
    
    // Initialize CPU features (FPU, SSE)
    crate::terminal::print("Initializing CPU features...");
    crate::cpu::init();
    crate::terminal::print(" [OK]\n");
    
    // Initialize GDT
    crate::terminal::print("Initializing GDT...");
    crate::gdt::init();
    crate::terminal::print(" [OK]\n");
    
    // Initialize IDT
    crate::terminal::print("Initializing IDT...");
    crate::interrupts::init();
    crate::terminal::print(" [OK]\n");

    // Initialize PIC
    crate::terminal::print("Initializing PIC...");
    unsafe { crate::pic::PICS.lock().initialize() };
    crate::terminal::print(" [OK]\n");

    // Initialize filesystem BEFORE enabling interrupts: its allocations
    // take locks that must never be held when an IRQ can arrive mid-init
    crate::terminal::print("Initializing filesystem...");
    crate::fs::init();
    crate::terminal::print(" [OK]\n");

    // Program the PIT (100 Hz) after the PIC (IRQ0 routed and unmasked)
    // and before enabling interrupts, so the first tick is serviced by a
    // fully set-up timer.
    crate::terminal::print("Initializing PIT timer (100 Hz)...");
    crate::time::init();
    crate::terminal::print(" [OK]\n");

    // Test harness (feature "test-harness"): with the filesystem and PIT
    // up and interrupts still disabled, run the kernel self-tests and exit
    // QEMU through isa-debug-exit before interrupts, shell, or the main
    // loop ever start.
    #[cfg(feature = "test-harness")]
    crate::test_harness::run();

    crate::terminal::print("\nInterrupt system ready.\n");
    crate::terminal::print("Enabling interrupts...");

    // Enable interrupts. The keyboard ISR only enqueues scancodes, so any
    // key pressed from here on is buffered until the main loop drains it.
    x86_64::instructions::interrupts::enable();
    crate::terminal::print(" [OK]\n");

    // Initialize shell
    crate::shell::init();

    // Main kernel loop: drain the scancode and serial RX queues in thread
    // context. Both queue locks are shared with ISRs, so they may only be
    // taken with interrupts disabled; both are popped and released before
    // any handler runs, and one item of EACH kind per iteration keeps a
    // backlog on one input from starving the other. enable_and_hlt
    // (sti; hlt) closes the window where input could arrive between the
    // empty check and the halt.
    loop {
        x86_64::instructions::interrupts::disable();
        let scancode = crate::keyboard::try_pop_scancode();
        let serial_byte = crate::serial::try_pop_input();
        match (scancode, serial_byte) {
            (None, None) => x86_64::instructions::interrupts::enable_and_hlt(),
            (scancode, serial_byte) => {
                x86_64::instructions::interrupts::enable();
                if let Some(scancode) = scancode {
                    crate::keyboard::handle_scancode(scancode);
                }
                if let Some(byte) = serial_byte {
                    crate::serial::handle_input(byte);
                }
            }
        }
    }
}

pub fn halt_loop() -> ! {
    loop {
        unsafe {
            asm!("hlt");
        }
    }
}