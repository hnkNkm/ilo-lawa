#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]

extern crate alloc;

use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::proto::loaded_image::LoadedImage;

mod font;
mod kernel;
mod gdt;
mod interrupts;
mod pic;
mod time;
mod keyboard;
mod terminal;
mod serial;
mod cpu;
mod allocator;
mod shell;
mod fs;
mod memory;
mod paging;
mod acpi;
mod stacks;
#[cfg(feature = "test-harness")]
mod test_harness;

use kernel::{FramebufferInfo, kernel_main};
use core::panic::PanicInfo;

#[entry]
fn main(image_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    
    // Display boot message
    system_table.stdout().clear().unwrap();
    system_table
        .stdout()
        .output_string(cstr16!("ilo-lawa bootloader\r\n"))
        .unwrap();
    system_table
        .stdout()
        .output_string(cstr16!("===================\r\n"))
        .unwrap();
    
    // Get GOP and framebuffer info BEFORE ExitBootServices
    let fb_info = {
        use uefi::proto::console::gop::PixelFormat as UefiPixelFormat;
        let bt = system_table.boot_services();
        let gop_handle = bt.get_handle_for_protocol::<GraphicsOutput>()
            .expect("Failed to get GOP handle");
        let mut gop = bt.open_protocol_exclusive::<GraphicsOutput>(gop_handle)
            .expect("Failed to open GOP");
        
        let mode_info = gop.current_mode_info();

        // Kernel-owned copy of the pixel format (the `uefi` crate type is
        // unusable after ExitBootServices). Bitmask modes carry the
        // per-channel masks in the mode info; a missing mask is a firmware
        // defect, handled by encoding with all-zero (no-op) masks.
        let pixel_format = match mode_info.pixel_format() {
            UefiPixelFormat::Rgb => kernel::PixelFormat::Rgb,
            UefiPixelFormat::Bgr => kernel::PixelFormat::Bgr,
            UefiPixelFormat::Bitmask => match mode_info.pixel_bitmask() {
                Some(mask) => kernel::PixelFormat::Bitmask {
                    red_mask: mask.red,
                    green_mask: mask.green,
                    blue_mask: mask.blue,
                },
                None => kernel::PixelFormat::Bitmask {
                    red_mask: 0,
                    green_mask: 0,
                    blue_mask: 0,
                },
            },
            UefiPixelFormat::BltOnly => kernel::PixelFormat::BltOnly,
        };

        // In a Blt-only mode no linear framebuffer exists at all; never
        // dereference one. (uefi's frame_buffer() would also assert.)
        let base_address = if pixel_format == kernel::PixelFormat::BltOnly {
            0
        } else {
            gop.frame_buffer().as_mut_ptr() as u64
        };

        // Save framebuffer info that will survive ExitBootServices
        FramebufferInfo {
            base_address,
            width: mode_info.resolution().0,
            height: mode_info.resolution().1,
            stride: mode_info.stride(),
            pixel_format,
        }
    };

    // Without a linear framebuffer the kernel cannot draw; stop here on the
    // firmware's stdout instead of entering the kernel and faulting on the
    // first clear().
    if fb_info.pixel_format == kernel::PixelFormat::BltOnly {
        system_table
            .stdout()
            .output_string(cstr16!(
                "ERROR: GOP reports BltOnly (no linear framebuffer). \
                ilo-lawa requires direct framebuffer access.\r\n"
            ))
            .ok();
        return Status::UNSUPPORTED;
    }
    
    system_table
        .stdout()
        .output_string(cstr16!("Framebuffer info saved.\r\n"))
        .unwrap();
    // Loaded-image bounds, captured BEFORE ExitBootServices: paging::init
    // needs them for the W^X section mapping. (0/0) means unavailable;
    // W^X is then skipped with a log.
    let image_info = {
        let bt = system_table.boot_services();
        match bt.open_protocol_exclusive::<LoadedImage>(image_handle) {
            Ok(li) => {
                let (base, size) = li.info();
                kernel::ImageInfo { base: base as u64, size }
            }
            Err(_) => kernel::ImageInfo { base: 0, size: 0 },
        }
    };

    // ACPI RSDP physical address, captured BEFORE ExitBootServices: the
    // UEFI configuration table disappears with boot services. Prefer the
    // ACPI 2.0+ entry (XSDT-capable RSDP), fall back to the ACPI 1.0 one;
    // 0 when the firmware publishes no ACPI tables (acpi::init then just
    // disables power control).
    let acpi_info = {
        use uefi::table::cfg;
        let mut rsdp_addr: u64 = 0;
        for entry in system_table.config_table() {
            if entry.guid == cfg::ACPI2_GUID {
                rsdp_addr = entry.address as u64;
                break;
            }
        }
        if rsdp_addr == 0 {
            for entry in system_table.config_table() {
                if entry.guid == cfg::ACPI_GUID {
                    rsdp_addr = entry.address as u64;
                    break;
                }
            }
        }
        kernel::AcpiInfo { rsdp_addr }
    };

    system_table
        .stdout()
        .output_string(cstr16!("Calling ExitBootServices...\r\n"))
        .unwrap();
    
    // EXIT BOOT SERVICES - After this, no UEFI services available!
    // The returned memory map is 'static (its backing allocation survives
    // ExitBootServices) and is handed to the kernel to retain.
    let (_system_table_runtime, memory_map) = system_table
        .exit_boot_services(uefi::table::boot::MemoryType::LOADER_DATA);

    // WE ARE NOW IN KERNEL MODE!
    // No more UEFI services, we're on our own
    kernel_main(fb_info, memory_map, image_info, acpi_info);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // A panicked kernel must not keep servicing IRQs (the shell would
    // continue running over corrupted state)
    x86_64::instructions::interrupts::disable();

    // The panic may have interrupted code that held the terminal lock;
    // that context will never resume, so break the lock before printing
    unsafe { crate::terminal::force_unlock() };
    unsafe { crate::serial::force_unlock() };

    crate::terminal::print("\n\n!!! KERNEL PANIC !!!\n");

    if let Some(location) = info.location() {
        crate::terminal::print_fmt(format_args!(
            "at {}:{}:{}\n",
            location.file(),
            location.line(),
            location.column()
        ));
    }

    crate::terminal::print_fmt(format_args!("{}\n", info.message()));

    // Halt the system
    loop {
        x86_64::instructions::hlt();
    }
}
