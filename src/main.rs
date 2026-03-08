#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]

extern crate alloc;

use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;

mod font;
mod kernel;
mod gdt;
mod interrupts;
mod pic;
mod keyboard;
mod terminal;
mod cpu;
mod allocator;
mod shell;
mod fs;
mod drivers;

use kernel::{FramebufferInfo, kernel_main};
use core::panic::PanicInfo;

#[entry]
fn main(_image_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    
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
        let bt = system_table.boot_services();
        let gop_handle = bt.get_handle_for_protocol::<GraphicsOutput>()
            .expect("Failed to get GOP handle");
        let mut gop = bt.open_protocol_exclusive::<GraphicsOutput>(gop_handle)
            .expect("Failed to open GOP");
        
        let mode_info = gop.current_mode_info();
        let mut framebuffer = gop.frame_buffer();
        
        // Save framebuffer info that will survive ExitBootServices
        FramebufferInfo {
            base_address: framebuffer.as_mut_ptr() as u64,
            width: mode_info.resolution().0,
            height: mode_info.resolution().1,
            stride: mode_info.stride(),
        }
    };
    
    system_table
        .stdout()
        .output_string(cstr16!("Framebuffer info saved.\r\n"))
        .unwrap();
    system_table
        .stdout()
        .output_string(cstr16!("Calling ExitBootServices...\r\n"))
        .unwrap();
    
    // EXIT BOOT SERVICES - After this, no UEFI services available!
    let (_system_table_runtime, _memory_map) = system_table
        .exit_boot_services(uefi::table::boot::MemoryType::LOADER_DATA);
    
    // WE ARE NOW IN KERNEL MODE!
    // No more UEFI services, we're on our own
    kernel_main(fb_info);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Try to print panic info if terminal is available
    crate::terminal::print("\n\n!!! KERNEL PANIC !!!\n");
    
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        crate::terminal::print(s);
    }
    
    if let Some(location) = info.location() {
        crate::terminal::print("\nPanic occurred at: ");
        crate::terminal::print(location.file());
    }
    
    // Halt the system
    loop {
        x86_64::instructions::hlt();
    }
}
