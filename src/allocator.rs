// Simple bump allocator for heap memory management

use alloc::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use spin::Mutex;

pub const HEAP_START: usize = 0x_4444_4444_0000;
pub const HEAP_SIZE: usize = 100 * 1024; // 100 KiB

struct BumpAllocator {
    heap_start: usize,
    heap_end: usize,
    next: usize,
    allocations: usize,
}

impl BumpAllocator {
    pub const fn new() -> Self {
        BumpAllocator {
            heap_start: 0,
            heap_end: 0,
            next: 0,
            allocations: 0,
        }
    }

    pub unsafe fn init(&mut self, heap_start: usize, heap_size: usize) {
        self.heap_start = heap_start;
        self.heap_end = heap_start + heap_size;
        self.next = heap_start;
    }
}

// Wrapper struct to avoid orphan rule
pub struct LockedHeap(Mutex<BumpAllocator>);

impl LockedHeap {
    pub const fn new() -> Self {
        LockedHeap(Mutex::new(BumpAllocator::new()))
    }
}

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut allocator = self.0.lock();

        // Align the allocation
        let alloc_start = align_up(allocator.next, layout.align());
        let alloc_end = match alloc_start.checked_add(layout.size()) {
            Some(end) => end,
            None => return null_mut(),
        };

        if alloc_end > allocator.heap_end {
            null_mut() // Out of memory
        } else {
            allocator.next = alloc_end;
            allocator.allocations += 1;
            alloc_start as *mut u8
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        let mut allocator = self.0.lock();
        allocator.allocations -= 1;
        
        // Simple bump allocator doesn't actually free memory
        // Reset only when all allocations are freed
        if allocator.allocations == 0 {
            allocator.next = allocator.heap_start;
        }
    }
}

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::new();

pub fn init_heap() {
    // Create a simple heap in a static buffer
    static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
    
    unsafe {
        ALLOCATOR.0.lock().init(
            &raw const HEAP as usize,
            HEAP_SIZE,
        );
    }
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

#[alloc_error_handler]
fn alloc_error_handler(layout: alloc::alloc::Layout) -> ! {
    panic!("Allocation error: {:?}", layout)
}