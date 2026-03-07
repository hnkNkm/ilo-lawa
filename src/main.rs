#![no_std]
#![no_main]

extern crate alloc;

use uefi::prelude::*;
use log::info;

#[entry]
fn main(_image_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    uefi::helpers::init(&mut system_table).unwrap();
    
    info!("Hello UEFI World!");
    
    system_table.stdout().clear().unwrap();
    system_table
        .stdout()
        .output_string(cstr16!("Hello from Rust UEFI OS!\r\n"))
        .unwrap();
    
    system_table
        .stdout()
        .output_string(cstr16!("ilo-lawa - Minimal UEFI OS in Rust\r\n"))
        .unwrap();
    
    system_table
        .stdout()
        .output_string(cstr16!("Press any key to continue...\r\n"))
        .unwrap();
    
    system_table
        .stdin()
        .reset(false)
        .unwrap();
    
    loop {
        if let Ok(Some(_key)) = system_table.stdin().read_key() {
            break;
        }
    }
    
    system_table
        .stdout()
        .output_string(cstr16!("Shutting down...\r\n"))
        .unwrap();
    
    Status::SUCCESS
}
