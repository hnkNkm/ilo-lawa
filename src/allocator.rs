// Heap allocator: linked_list_allocator's `LockedHeap` over a static,
// page-aligned 1 MiB area. Freed blocks are returned to the allocator's
// free list and coalesced with neighbours, so shell churn (per-command
// Strings, history, FS tree edits) no longer leaks — the previous bump
// allocator only recovered the whole heap when the allocation count hit
// zero, which never happened with persistent allocations alive.

use alloc::alloc::Layout;
use linked_list_allocator::LockedHeap;

/// 1 MiB static heap. Lives in .bss: it costs no bytes in the EFI image
/// file, but adds 1 MiB to the kernel's memory footprint at boot.
pub const HEAP_SIZE: usize = 1024 * 1024;

/// Page-aligned backing area; `Heap::init` requires its bottom pointer to
/// be suitably aligned for the largest hole header. The bytes are never
/// read by kernel code — only handed to the allocator.
#[repr(C, align(4096))]
#[allow(dead_code)]
struct HeapArea([u8; HEAP_SIZE]);

static mut HEAP_AREA: HeapArea = HeapArea([0; HEAP_SIZE]);

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Hand the static heap area to the allocator. Must be called exactly
/// once, before any allocation, from kernel_main's init sequence.
pub fn init_heap() {
    unsafe {
        let bottom = &raw mut HEAP_AREA as *mut u8;
        ALLOCATOR.lock().init(bottom, HEAP_SIZE);
    }
}

#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    panic!("Allocation error: {:?}", layout)
}
