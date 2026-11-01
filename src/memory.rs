// Physical memory management retained from the UEFI memory map.
//
// Two pieces live here:
//   1. `MEMORY_SUMMARY`: a compact, heap-free copy of the firmware memory
//      map taken right after ExitBootServices (fixed-size static array,
//      max 64 regions).
//   2. `FRAME_ALLOCATOR`: a bitmap-based physical frame allocator over the
//      usable (CONVENTIONAL) regions. Frames are 4 KiB physical pages used
//      at their identity-mapped physical addresses — no paging layer yet.
//
// `allocate_contiguous` (via `reserve_frames`) backs the kernel heap at
// boot; `allocate_frame`/`free_frame` are otherwise unused until the
// paging/tasking stages of issue #5/#18 land. They are kept with the same
// `dead_code` allowance kernel.rs uses for forward-looking code.
#![allow(dead_code)]

// LOCKING (single-core kernel): both globals are `spin::Mutex`es. The CPU
// never gets preempted and no SMP cores exist, so a lock is only contended
// by an interrupt handler. Neither lock is ever taken from interrupt
// context — frame allocation/summary access happens in thread context only
// — so no ISR can deadlock against a lock held by the interrupted code.
// Should either ever be needed in an ISR, the taker must disable
// interrupts around the lock (see keyboard.rs / kernel.rs main loop).

use crate::terminal;
use spin::Mutex;
use uefi::table::boot::{MemoryMap, MemoryType};

/// 4 KiB frames, the UEFI page granularity.
pub const FRAME_SIZE: u64 = 4096;

/// Regions starting at or above this physical address are ignored by the
/// frame allocator (its bitmap only covers the low 4 GiB). Reported in the
/// boot log when any are dropped.
pub const MAX_SUPPORTED_PHYS: u64 = 4 * 1024 * 1024 * 1024;

const FRAME_COUNT: usize = (MAX_SUPPORTED_PHYS / FRAME_SIZE) as usize; // 1,048,576
const BITMAP_WORDS: usize = FRAME_COUNT / 64; // 16,384 words = 128 KiB
const MAX_REGIONS: usize = 64;

/// Compact per-region summary copied out of the UEFI memory map.
#[derive(Clone, Copy)]
pub struct RegionInfo {
    pub phys_start: u64,
    pub page_count: u64,
    pub ty: MemoryType,
}

/// Heap-free snapshot of the firmware memory map (logging only — the
/// frame allocator pass iterates the full map).
pub struct MemorySummary {
    regions: [Option<RegionInfo>; MAX_REGIONS],
    /// Regions retained (capped at MAX_REGIONS).
    count: usize,
    /// Regions in the firmware map, including any beyond the cap.
    total: usize,
    /// Regions dropped because they started above 4 GiB.
    ignored_above_4g: usize,
}

pub static MEMORY_SUMMARY: Mutex<MemorySummary> = Mutex::new(MemorySummary::new());

impl MemorySummary {
    const fn new() -> Self {
        MemorySummary {
            regions: [None; MAX_REGIONS],
            count: 0,
            total: 0,
            ignored_above_4g: 0,
        }
    }
}

/// Bitmap frame allocator. One bit per 4 KiB frame over the low 4 GiB;
/// set = used, clear = free. The static starts zero-filled (.bss — no
/// EFI image file cost); `init` marks every bit used and then clears the
/// usable-region frames, so frames the allocator never learned about are
/// unallocatable.
pub struct BitmapFrameAllocator {
    bitmap: [u64; BITMAP_WORDS],
    free_frames: u64,
    usable_frames: u64,
}

pub static FRAME_ALLOCATOR: Mutex<BitmapFrameAllocator> =
    Mutex::new(BitmapFrameAllocator::new());

impl BitmapFrameAllocator {
    /// All-zero state: every frame untracked (treated as used) until
    /// init marks the usable pool. Zero-init keeps the 128 KiB bitmap in
    /// .bss instead of baking it into the EFI image file.
    pub const fn new() -> Self {
        BitmapFrameAllocator {
            bitmap: [0; BITMAP_WORDS],
            free_frames: 0,
            usable_frames: 0,
        }
    }

    /// Mark every frame in the 4 GiB window as used. Call once before
    /// clearing usable regions.
    fn fill_used(&mut self) {
        for word in self.bitmap.iter_mut() {
            *word = u64::MAX;
        }
    }

    /// Mark the frames of one usable region as free.
    fn mark_region_free(&mut self, region: &RegionInfo) {
        let start_frame = (region.phys_start / FRAME_SIZE) as usize;
        for i in 0..region.page_count {
            let frame = start_frame + i as usize;
            if frame >= FRAME_COUNT {
                // Region straddles the 4 GiB boundary; keep the low part.
                break;
            }
            let word = frame / 64;
            let mask = 1u64 << (frame % 64);
            if self.bitmap[word] & mask != 0 {
                self.bitmap[word] &= !mask;
                self.free_frames += 1;
                self.usable_frames += 1;
            }
        }
    }

    /// Allocate one 4 KiB frame. Returns its identity-mapped physical
    /// base address (usable directly as a virtual address — no paging
    /// layer exists yet).
    pub fn allocate_frame(&mut self) -> Option<u64> {
        if self.free_frames == 0 {
            return None;
        }
        for (word_idx, &word) in self.bitmap.iter().enumerate() {
            if word != u64::MAX {
                let bit = word.trailing_ones();
                self.bitmap[word_idx] |= 1u64 << bit;
                self.free_frames -= 1;
                let frame = word_idx * 64 + bit as usize;
                return Some(frame as u64 * FRAME_SIZE);
            }
        }
        None
    }

    /// Reserve `frames` consecutive free 4 KiB frames and mark them used.
    /// Returns the physical base address of the run (identity-mapped — no
    /// paging layer offsets addresses), or `None` when the pool is too
    /// fragmented or too small to satisfy the request.
    ///
    /// First-fit scan over the bitmap for the first run of `frames` clear
    /// bits: O(FRAME_COUNT) at worst, no allocation (safe before and
    /// during heap bring-up). The pre-check on `free_frames` guarantees
    /// the counter decrement cannot underflow.
    pub fn allocate_contiguous(&mut self, frames: usize) -> Option<u64> {
        if frames == 0 || frames as u64 > self.free_frames {
            return None;
        }
        let mut run_start = 0usize;
        let mut run_len = 0usize;
        for (word_idx, &word) in self.bitmap.iter().enumerate() {
            if word == u64::MAX {
                // Fully-used word breaks any open run.
                run_len = 0;
                continue;
            }
            // Runs carry across word boundaries (mixed and fully-free
            // words are scanned bit by bit).
            for bit in 0..64 {
                if word & (1u64 << bit) == 0 {
                    if run_len == 0 {
                        run_start = word_idx * 64 + bit;
                    }
                    run_len += 1;
                    if run_len == frames {
                        self.mark_run_used(run_start, frames);
                        self.free_frames -= frames as u64;
                        return Some(run_start as u64 * FRAME_SIZE);
                    }
                } else {
                    run_len = 0;
                }
            }
        }
        None
    }

    /// Like [`allocate_contiguous`], but the returned run must start at
    /// or above `min_pa`: frames below are skipped without being touched,
    /// so callers can stay clear of the historic low pages without
    /// consuming anything. First-fit scan from `min_pa` upward.
    pub fn allocate_contiguous_above(&mut self, min_pa: u64, frames: usize) -> Option<u64> {
        if frames == 0 || frames as u64 > self.free_frames {
            return None;
        }
        let min_frame = (min_pa / FRAME_SIZE) as usize;
        let mut run_start = 0usize;
        let mut run_len = 0usize;
        for (word_idx, &word) in self.bitmap.iter().enumerate() {
            if word == u64::MAX {
                run_len = 0;
                continue;
            }
            for bit in 0..64 {
                let frame = word_idx * 64 + bit;
                if frame < min_frame {
                    continue;
                }
                if word & (1u64 << bit) == 0 {
                    if run_len == 0 {
                        run_start = frame;
                    }
                    run_len += 1;
                    if run_len == frames {
                        self.mark_run_used(run_start, frames);
                        self.free_frames -= frames as u64;
                        return Some(run_start as u64 * FRAME_SIZE);
                    }
                } else {
                    run_len = 0;
                }
            }
        }
        None
    }

    /// Mark `frames` consecutive bits starting at `start_frame` used.
    /// Callers guarantee the run lies inside the bitmap.
    fn mark_run_used(&mut self, start_frame: usize, frames: usize) {
        for frame in start_frame..start_frame + frames {
            let word = frame / 64;
            self.bitmap[word] |= 1u64 << (frame % 64);
        }
    }

    /// Return a previously allocated frame to the pool. Callers must only
    /// free frames obtained from `allocate_frame`; the bounds/alignment
    /// check here is a guard against corruption, not a region-ownership
    /// proof. Double frees are ignored (the bit is already clear).
    pub fn free_frame(&mut self, addr: u64) {
        if addr >= MAX_SUPPORTED_PHYS || addr % FRAME_SIZE != 0 {
            return;
        }
        let frame = (addr / FRAME_SIZE) as usize;
        let word = frame / 64;
        let mask = 1u64 << (frame % 64);
        if self.bitmap[word] & mask != 0 {
            self.bitmap[word] &= !mask;
            self.free_frames += 1;
        }
    }

    pub fn free_frame_count(&self) -> u64 {
        self.free_frames
    }

    pub fn usable_frame_count(&self) -> u64 {
        self.usable_frames
    }
}

/// Reserve `frames` consecutive free frames for a long-lived allocation
/// (the kernel heap, future page-table pools). Locks `FRAME_ALLOCATOR`;
/// thread context only. The frames stay reserved: bits are set and the
/// free counter decremented, so no later frame allocation can hand them
/// out again. Returns the physical base address of the run.
pub fn reserve_frames(frames: usize) -> Option<u64> {
    FRAME_ALLOCATOR.lock().allocate_contiguous(frames)
}

/// Reserve `frames` consecutive free frames starting at or above
/// `min_pa` (frames below are skipped, not consumed). Locks
/// `FRAME_ALLOCATOR`; thread context only. Used by the usermode stage so
/// the ring-3 code/stack frames never land in the historic low pages a
/// fragmented UEFI map may offer as free CONVENTIONAL RAM (issue #19
/// stage 1).
pub fn reserve_frames_above(min_pa: u64, frames: usize) -> Option<u64> {
    FRAME_ALLOCATOR.lock().allocate_contiguous_above(min_pa, frames)
}

/// Frames currently free in the pool. Locks `FRAME_ALLOCATOR`;
/// thread context only.
pub fn free_frame_count() -> u64 {
    FRAME_ALLOCATOR.lock().free_frame_count()
}

/// Retain the memory map handed back by ExitBootServices and bring up the
/// frame allocator. Runs in thread context before interrupts are enabled;
/// prints the memory summary to the boot log (serial + framebuffer).
pub fn init(memory_map: &MemoryMap<'static>) {
    // Pass 1: compact 64-region summary, for the boot log only. The frame
    // allocator pass below must see every descriptor, so regions beyond
    // this log cap stay allocatable.
    {
        let mut summary = MEMORY_SUMMARY.lock();
        for desc in memory_map.entries() {
            summary.total += 1;
            if summary.count < MAX_REGIONS {
                let idx = summary.count;
                summary.regions[idx] = Some(RegionInfo {
                    phys_start: desc.phys_start,
                    page_count: desc.page_count,
                    ty: desc.ty,
                });
                summary.count = idx + 1;
            }
            if desc.phys_start >= MAX_SUPPORTED_PHYS {
                summary.ignored_above_4g += 1;
            }
        }
    }

    // Pass 2: every descriptor feeds the frame allocator.
    let (free_frames, usable_frames, usable_mib) = {
        let mut allocator = FRAME_ALLOCATOR.lock();
        allocator.fill_used();
        for desc in memory_map.entries() {
            if desc.ty == MemoryType::CONVENTIONAL {
                allocator.mark_region_free(&RegionInfo {
                    phys_start: desc.phys_start,
                    page_count: desc.page_count,
                    ty: desc.ty,
                });
            }
        }
        self_check(&mut allocator);
        (
            allocator.free_frame_count(),
            allocator.usable_frame_count(),
            allocator.usable_frame_count() * FRAME_SIZE / (1024 * 1024),
        )
    };

    let summary = MEMORY_SUMMARY.lock();
    terminal::print_fmt(format_args!(
        "Memory map: {} of {} region(s) retained for logging (cap {}), {} above 4 GiB ignored\n",
        summary.count, summary.total, MAX_REGIONS, summary.ignored_above_4g
    ));
    terminal::print_fmt(format_args!(
        "Usable RAM (CONVENTIONAL, below 4 GiB): {} MiB\n",
        usable_mib
    ));
    terminal::print_fmt(format_args!(
        "Frame allocator: {} frames usable, {} free (4 KiB each)\n",
        usable_frames, free_frames
    ));
}

/// Boot-time self-check of the frame allocator: exercise the API once
/// without permanently consuming a frame (the frame's memory itself is
/// never touched). Panics on invariant mismatch instead of letting the
/// kernel run with a broken allocator.
fn self_check(allocator: &mut BitmapFrameAllocator) {
    let before = allocator.free_frame_count();
    let frame = allocator
        .allocate_frame()
        .expect("frame allocator self-check: no free frame right after init");
    assert!(
        frame % FRAME_SIZE == 0,
        "frame allocator self-check: frame {:#x} not 4 KiB-aligned",
        frame
    );
    assert!(
        frame < MAX_SUPPORTED_PHYS,
        "frame allocator self-check: frame {:#x} outside the 4 GiB window",
        frame
    );
    allocator.free_frame(frame);
    let after = allocator.free_frame_count();
    assert!(
        after == before,
        "frame allocator self-check: free count {} did not return to {}",
        after,
        before
    );

    // Contiguous reservation: the same invariants for a multi-frame run,
    // exercised the way the heap bring-up reserves its backing frames.
    const RUN: usize = 4;
    let run = allocator
        .allocate_contiguous(RUN)
        .expect("frame allocator self-check: no contiguous 4-frame run right after init");
    assert!(
        run % FRAME_SIZE == 0,
        "frame allocator self-check: run {:#x} not 4 KiB-aligned",
        run
    );
    assert!(
        run + RUN as u64 * FRAME_SIZE <= MAX_SUPPORTED_PHYS,
        "frame allocator self-check: run {:#x}..{:#x} outside the 4 GiB window",
        run,
        run + RUN as u64 * FRAME_SIZE
    );
    for i in 0..RUN {
        allocator.free_frame(run + i as u64 * FRAME_SIZE);
    }
    let after_run = allocator.free_frame_count();
    assert!(
        after_run == before,
        "frame allocator self-check: free count {} did not return to {} after the contiguous run",
        after_run,
        before
    );
    terminal::print("Frame allocator self-check OK\n");
}
