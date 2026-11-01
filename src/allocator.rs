// Heap allocator: `linked_list_allocator::LockedHeap`, sized at boot from
// the physical frame allocator (issue #5): the target is free conventional
// RAM / 8, clamped to [1 MiB, 32 MiB], reserved as one contiguous run of
// 4 KiB frames and initialized in place — the heap is identity-mapped
// conventional RAM (paging maps 0..6 GiB and W^X marks non-image leaves
// RW|NX, so no extra page-table work is needed). The reserved frames stay
// out of the frame allocator's free pool. If no contiguous run of at least
// 1 MiB exists, the static 1 MiB .bss area below is used as a fallback
// instead of panicking.
//
// Freed blocks are returned to the allocator's free list and coalesced
// with neighbours, so shell churn (per-command Strings, history, FS tree
// edits) no longer leaks — the previous bump allocator only recovered the
// whole heap when the allocation count hit zero, which never happened with
// persistent allocations alive.

use alloc::alloc::Layout;
use linked_list_allocator::LockedHeap;

use crate::memory;

/// Lower bound (and static fallback size): 1 MiB.
pub const HEAP_SIZE: usize = 1024 * 1024;
/// Upper bound for the dynamically sized heap: 32 MiB.
const MAX_HEAP_SIZE: usize = 32 * 1024 * 1024;
/// The heap targets this fraction of free conventional RAM.
const FREE_RAM_FRACTION: u64 = 8;

/// Static 1 MiB fallback heap. Lives in .bss: it costs no bytes in the
/// EFI image file, but adds 1 MiB to the kernel's memory footprint at
/// boot. Only initialized when contiguous reservation fails.
#[repr(C, align(4096))]
#[allow(dead_code)]
struct HeapArea([u8; HEAP_SIZE]);

static mut HEAP_AREA: HeapArea = HeapArea([0; HEAP_SIZE]);

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Size the heap from the frame allocator's pool and reserve its frames.
///
/// Target size = current free conventional bytes / 8, clamped to
/// [1 MiB, 32 MiB] and page-aligned. That many frames are reserved
/// contiguously; on a fragmented map the size is halved down to 1 MiB
/// before giving up. Must be called exactly once, before any allocation,
/// from kernel_main's init sequence — and after `memory::init`, which
/// must be the one to report the frame summary (its self-check runs
/// before any frames are reserved here).
///
/// On success the backing frames are handed to the allocator and stay
/// reserved in the frame bitmap. On total failure (no contiguous 1 MiB
/// anywhere in the pool) the static 1 MiB fallback is initialized and the
/// condition logged — the kernel never panics here.
pub fn init_heap_from_memory() {
    let free_bytes = memory::free_frame_count() * memory::FRAME_SIZE;
    let target = (free_bytes / FREE_RAM_FRACTION) as usize;
    let mut size = target.clamp(HEAP_SIZE, MAX_HEAP_SIZE);
    // Clamp bounds are page-aligned; the mask keeps a mid-range target
    // (e.g. free/8 = 3.5 MiB) on a frame boundary too.
    size &= !(memory::FRAME_SIZE as usize - 1);

    while size >= HEAP_SIZE {
        let frames = size / memory::FRAME_SIZE as usize;
        if let Some(phys) = memory::reserve_frames(frames) {
            unsafe { ALLOCATOR.lock().init(phys as *mut u8, size) };
            let free_mib =
                memory::free_frame_count() * memory::FRAME_SIZE / (1024 * 1024);
            crate::terminal::print_fmt(format_args!(
                "[heap] {} MiB at {:#x} ({} frames reserved, {} MiB free)\n",
                size / (1024 * 1024),
                phys,
                frames,
                free_mib
            ));
            return;
        }
        size /= 2;
    }

    // No contiguous run anywhere in the pool: initialize the static
    // fallback instead of panicking.
    let bottom = &raw mut HEAP_AREA as *mut u8;
    unsafe { ALLOCATOR.lock().init(bottom, HEAP_SIZE) };
    crate::terminal::print_fmt(format_args!(
        "[heap] no contiguous run in the frame pool: static {} KiB fallback at {:#x}\n",
        HEAP_SIZE / 1024,
        bottom as usize
    ));
}

#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    panic!("Allocation error: {:?}", layout)
}
