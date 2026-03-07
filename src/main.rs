#![no_std]
#![no_main]

extern crate alloc;

use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;
use log::info;

mod font;
mod kernel;
use kernel::{FramebufferInfo, kernel_main};

#[entry]
fn main(image_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    uefi::helpers::init(&mut system_table).unwrap();
    
    info!("Preparing to exit boot services and enter kernel mode...");
    
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
