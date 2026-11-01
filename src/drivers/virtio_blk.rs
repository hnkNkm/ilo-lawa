//! virtio-blk block device driver over QEMU's *transitional legacy* PCI
//! transport (vendor 0x1AF4, device 0x1001), legacy virtio 0.9.5 register
//! layout (issue #14).
//!
//! Why legacy: the device exposes its control registers through the I/O
//! port BAR0, so no page-table work is needed — everything the driver
//! touches (I/O ports, DMA rings, heap RAM) is already inside the
//! identity-mapped 0..6 GiB window built by `paging::init`. A modern-only
//! device (1AF4:1042) would need its MMIO BAR mapped explicitly; we
//! deliberately do not support that yet and report it clearly instead.
//!
//! DMA: the split virtqueue rings (descriptor table, avail ring, used
//! ring) and the single in-flight request's header/data/status buffers
//! live in ONE physically contiguous page-aligned heap allocation. The
//! heap is carved from identity-mapped conventional RAM below 4 GiB
//! (see `allocator.rs`), so virtual == physical and the device can DMA
//! into it directly — no bounce buffers, no cache maintenance (x86 PCI
//! DMA is cache-coherent). The allocation is intentionally never freed:
//! the device object lives for the kernel's lifetime.
//!
//! Completion is POLLED (the used-ring index is spun on); no virtio IRQ
//! is routed and the driver never runs from an interrupt handler — it is
//! thread-context only, behind `BLOCK_DEVICE`.
//!
//! Register map (legacy, inside BAR0 I/O space):
//!   0x00 u32 host_features (RO)      0x04 u32 guest_features (W)
//!   0x08 u32 queue_pfn (RW, 4 KiB page frame)
//!   0x0C u16 queue_num (RO)          0x0E u16 queue_sel (W)
//!   0x10 u16 queue_notify (W)        0x12 u8  status (RW)
//!   0x13 u8  isr (RO)                0x14+ device config (blk capacity u64)

use alloc::alloc::{alloc, dealloc, Layout};
use alloc::vec::Vec;
use core::ptr;
use spin::Mutex;
use x86_64::instructions::port::Port;

use crate::drivers::block::{BlockDevice, BlockError, SECTOR_SIZE};
use crate::drivers::pci;

// --- legacy PCI register offsets (all within the BAR0 I/O region) ---
const REG_HOST_FEATURES: u16 = 0x00;
const REG_GUEST_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_NUM: u16 = 0x0C;
const REG_QUEUE_SEL: u16 = 0x0E;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
// Virtio-blk device-specific configuration starts at 0x14: the u64
// capacity (in sectors) occupies the first eight bytes.
const CONFIG_CAPACITY: u16 = 0x14;

// --- device status bits (write sequence per virtio 0.9.5 §2.2.1) ---
const STATUS_ACKNOWLEDGE: u8 = 1 << 0;
const STATUS_DRIVER: u8 = 1 << 1;
const STATUS_DRIVER_OK: u8 = 1 << 2;
const STATUS_FAILED: u8 = 1 << 7;

// --- feature bits we inspect/negotiate ---
/// Must never be set by the device (legacy sanity check).
const VIRTIO_F_BAD_FEATURE: u32 = 1 << 31;
/// The disk is read-only (informational; accepting it changes nothing).
const VIRTIO_BLK_F_RO: u32 = 1 << 5;

// --- virtio-blk request types ---
const VIRTIO_BLK_T_IN: u32 = 0; // read: device -> driver

// --- split virtqueue descriptor flags ---
const DESC_F_NEXT: u16 = 1; // chained via .next
const DESC_F_WRITE: u16 = 2; // device-writable (driver-read)

/// Device-reported queue size is trusted only within these bounds; QEMU's
/// virtio-blk legacy reports 128.
const MIN_QUEUE_SIZE: u32 = 8;
const MAX_QUEUE_SIZE: u32 = 1024;

/// Data sectors one request can carry. The FAT32 driver reads whole
/// clusters (mkfs.vfat defaults to 4 KiB = 8 sectors for a 64 MiB image);
/// 16 sectors of data headroom keeps every current caller in one request.
const MAX_DATA_SECTORS: usize = 16;

/// Bounded spin for polled completion: ~100 M port reads would take
/// seconds; QEMU answers in microseconds, so exhausting this means the
/// device is wedged and we fail the request instead of hanging forever.
const POLL_LIMIT: usize = 50_000_000;

/// Legacy queue geometry: the device splits the page-aligned region the
/// driver submitted via QUEUE_PFN as
///   desc @ 0, avail @ 16*Q, used @ align4096(16*Q + 6 + 2*Q)
/// (QEMU `VIRTIO_PCI_VRING_ALIGN == 4096`). We append our own request
/// buffers behind the used ring inside the same allocation.
#[inline]
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// # Safety
/// `base..base + len` must be a valid, exclusively owned, DMA-accessible
/// region laid out per the legacy queue geometry (desc @ base, avail @
/// 16*Q, used @ align4096(16*Q + 6 + 2*Q); only violated by a driver bug —
/// every pointer below is derived from the same checked allocation).
struct QueueView {
    base: usize,
    queue_size: u16,
    len: usize,
}

impl QueueView {
    #[inline]
    fn avail_at(&self) -> usize {
        self.base + 16 * self.queue_size as usize
    }

    #[inline]
    fn used_at(&self) -> usize {
        align_up(self.avail_at() + 6 + 2 * self.queue_size as usize, 4096)
    }

    #[inline]
    fn req_at(&self) -> usize {
        align_up(self.used_at() + 6 + 8 * self.queue_size as usize + 2, 16)
    }

    #[inline]
    fn data_at(&self) -> usize {
        self.req_at() + 16 // after the 16-byte virtio-blk request header
    }

    #[inline]
    fn status_at(&self) -> usize {
        self.data_at() + MAX_DATA_SECTORS * SECTOR_SIZE
    }

    fn dma_len(&self) -> usize {
        self.status_at() + 1 - self.base
    }
}

// Request header: { type: u32, reserved: u32, sector: u64 } — 16 bytes,
// written into the DMA region with plain stores below.

/// One initialized virtio-blk device.
pub struct VirtioBlkDevice {
    io_base: u16,
    queue: QueueView,
    read_only: bool,
    capacity_sectors: u64,
    /// Driver-side avail ring index (monotonically increasing).
    avail_idx: u16,
    /// Last used-ring index consumed by the driver.
    last_used_idx: u16,
    /// PCI location, for `disks`/`lspci` output.
    pub pci: (u8, u8, u8),
}

impl VirtioBlkDevice {
    /// Probe the PCI table for a virtio-blk device and bring it up.
    /// Prefers the legacy/transitional device (0x1001, I/O-port BAR0); a
    /// modern-only device (0x1042) is reported as unsupported (its MMIO
    /// BAR would need a page-table mapping, which this driver avoids).
    pub fn probe_and_init() -> Result<VirtioBlkDevice, &'static str> {
        let mut legacy = None;
        let mut modern = false;
        pci::for_each_device(|dev| {
            if dev.vendor_id != 0x1AF4 {
                return;
            }
            match dev.device_id {
                0x1001 => legacy = Some(*dev),
                0x1042 => modern = true,
                _ => {}
            }
        });

        let dev = match legacy {
            Some(dev) => dev,
            None => {
                return Err(if modern {
                    "virtio-blk: found modern-only device 1af4:1042 (MMIO BAR mapping not implemented); attach a transitional device (QEMU default virtio-blk-pci on the pc machine)"
                } else {
                    "no virtio-blk PCI device present"
                });
            }
        };

        if !dev.bar_is_io(0) {
            return Err("virtio-blk: BAR0 is not an I/O BAR (unexpected legacy layout)");
        }
        let io_base = dev.bar_base(0) as u16;
        if io_base == 0 {
            return Err("virtio-blk: BAR0 unmapped");
        }

        // DMA permission for the queue rings and data buffers.
        pci::enable_device(&dev);

        // SAFETY: I/O BAR base from PCI enumeration; QEMU's legacy
        // virtio-blk decode region is 32 bytes of registers + config.
        let mut device = VirtioBlkDevice {
            io_base,
            queue: QueueView {
                base: 0,
                queue_size: 0,
                len: 0,
            },
            read_only: false,
            capacity_sectors: 0,
            avail_idx: 0,
            last_used_idx: 0,
            pci: (dev.bus, dev.device, dev.function),
        };
        device.reset_and_ack()?;
        device.negotiate_features()?;
        device.setup_queue()?;
        device.verify_sector_read()?;

        Ok(device)
    }

    fn out32(&mut self, offset: u16, value: u32) {
        unsafe { Port::<u32>::new(self.io_base + offset).write(value) };
    }
    fn out16(&mut self, offset: u16, value: u16) {
        unsafe { Port::<u16>::new(self.io_base + offset).write(value) };
    }
    fn out8(&mut self, offset: u16, value: u8) {
        unsafe { Port::<u8>::new(self.io_base + offset).write(value) };
    }
    fn in32(&mut self, offset: u16) -> u32 {
        unsafe { Port::<u32>::new(self.io_base + offset).read() }
    }
    fn in16(&mut self, offset: u16) -> u16 {
        unsafe { Port::<u16>::new(self.io_base + offset).read() }
    }
    fn in8(&mut self, offset: u16) -> u8 {
        unsafe { Port::<u8>::new(self.io_base + offset).read() }
    }

    /// Reset, then acknowledge the device. Verifies the reset took effect
    /// by reading the status register back.
    fn reset_and_ack(&mut self) -> Result<(), &'static str> {
        self.out8(REG_STATUS, 0);
        let status = self.in8(REG_STATUS);
        if status != 0 {
            self.out8(REG_STATUS, STATUS_FAILED);
            return Err("virtio-blk: device did not reset");
        }
        self.out8(REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        Ok(())
    }

    /// Feature negotiation (legacy style): read the device's offered bits,
    /// reject a broken device, write back the accepted subset (read-only
    /// flag only — every other virtio-blk feature is optional and the
    /// driver is happy with the defaults).
    fn negotiate_features(&mut self) -> Result<(), &'static str> {
        let host = self.in32(REG_HOST_FEATURES);
        // Bit 31 (BAD_FEATURE) must never be offered; a set high bit also
        // means modern-only features, which the legacy register cannot
        // express. QEMU masks both out for legacy guests anyway.
        if host & VIRTIO_F_BAD_FEATURE != 0 {
            self.out8(REG_STATUS, STATUS_FAILED);
            return Err("virtio-blk: device offered the forbidden BAD_FEATURE bit");
        }
        let guest = host & VIRTIO_BLK_F_RO;
        self.out32(REG_GUEST_FEATURES, guest);
        self.read_only = guest & VIRTIO_BLK_F_RO != 0;
        crate::terminal::print_fmt(format_args!(
            "\n[virtio-blk] features: host={:#010x} accepted={:#010x}{}",
            host,
            guest,
            if self.read_only { " (read-only disk)" } else { "" },
        ));
        Ok(())
    }

    /// Queue 0 setup: select it, trust the reported size only within
    /// bounds, allocate the physically contiguous page-aligned DMA block
    /// from the identity-mapped heap, hand its 4 KiB page frame number to
    /// the device.
    fn setup_queue(&mut self) -> Result<(), &'static str> {
        self.out16(REG_QUEUE_SEL, 0);
        let queue_size = self.in16(REG_QUEUE_NUM) as u32;
        if queue_size < MIN_QUEUE_SIZE
            || queue_size > MAX_QUEUE_SIZE
            || !queue_size.is_power_of_two()
        {
            self.out8(REG_STATUS, STATUS_FAILED);
            return Err("virtio-blk: device reported an unusable queue size");
        }

        // Reserve the queue view layout and size the DMA allocation.
        let view = QueueView {
            base: 0,
            queue_size: queue_size as u16,
            len: 0,
        };
        let dma_len = align_up(view.dma_len(), 4096);
        let layout = Layout::from_size_align(dma_len, 4096)
            .map_err(|_| "virtio-blk: bad DMA layout")?;

        // Heap RAM is identity-mapped conventional memory below 4 GiB
        // (allocator.rs / paging.rs) — virtual address == physical, which
        // is exactly what the device needs for DMA and QUEUE_PFN.
        // SAFETY: layout has non-zero size; allocation checked below.
        let base_ptr = unsafe { alloc(layout) };
        if base_ptr.is_null() {
            return Err("virtio-blk: out of memory for the virtqueue");
        }
        let base = base_ptr as usize;
        if base >= 0xFFFF_F000 {
            // Cannot be expressed in QUEUE_PFN (32-bit page frame) — and
            // cannot happen: the heap comes from conventional RAM < 4 GiB.
            return Err("virtio-blk: DMA region above 4 GiB");
        }

        // Rings start zeroed (fresh heap pages are zero, but the heap may
        // be reused, so clear explicitly): empty avail/used rings, free
        // descriptor 0. We set avail.flags = NO_INTERRUPT (1): completion
        // is polled off the used ring, no IRQ is routed to this device.
        // SAFETY: base..base+dma_len is ours exclusively.
        unsafe {
            ptr::write_bytes(base_ptr, 0, dma_len);
            ptr::write_volatile(base as *mut u16, 1);
        }

        self.out32(REG_QUEUE_PFN, (base >> 12) as u32);
        let queued_pfn = self.in32(REG_QUEUE_PFN);
        if queued_pfn != (base >> 12) as u32 {
            self.out8(REG_STATUS, STATUS_FAILED);
            return Err("virtio-blk: device did not accept the queue address");
        }

        self.queue = QueueView {
            base,
            queue_size: queue_size as u16,
            len: dma_len,
        };

        // Capacity lives in the device config area (u64 at 0x14): two
        // 32-bit port reads, little-endian order.
        let cap_lo = self.in32(CONFIG_CAPACITY) as u64;
        let cap_hi = self.in32(CONFIG_CAPACITY + 4) as u64;
        self.capacity_sectors = cap_lo | (cap_hi << 32);
        if self.capacity_sectors == 0 {
            self.out8(REG_STATUS, STATUS_FAILED);
            return Err("virtio-blk: device reports zero capacity");
        }

        // Everything is in place: the device may now execute requests.
        self.out8(REG_STATUS, STATUS_DRIVER_OK);
        let status = self.in8(REG_STATUS);
        if status & STATUS_DRIVER_OK == 0 || status & STATUS_FAILED != 0 {
            return Err("virtio-blk: device refused DRIVER_OK");
        }

        crate::terminal::print_fmt(format_args!(
            "\n[virtio-blk] queue 0: {} entries, rings at {:#x} ({} KiB), pfn {:#x}",
            queue_size,
            base,
            dma_len / 1024,
            base >> 12,
        ));
        Ok(())
    }

    /// Post-init sanity check required by issue #14: one real polled
    /// sector-0 read through the full descriptor/avail/notify/used path.
    fn verify_sector_read(&mut self) -> Result<(), &'static str> {
        let sector0 = self
            .read_sectors(0, 1)
            .map_err(|_| "virtio-blk: sector 0 read failed")?;
        let sig = (sector0[510], sector0[511]);
        crate::terminal::print_fmt(format_args!(
            "\n[virtio-blk] sector 0 read: {} bytes, bytes[510..512]={:02x} {:02x}{}",
            sector0.len(),
            sig.0,
            sig.1,
            if sig == (0x55, 0xAA) {
                " (boot signature)"
            } else {
                ""
            },
        ));
        Ok(())
    }

    /// Write one descriptor. All ring accesses are volatile: the device
    /// reads/writes this memory concurrently via DMA.
    ///
    /// # Safety
    /// `index` must be < queue_size and the DMA region must be mapped.
    unsafe fn write_desc(&self, index: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let desc = (self.queue.base + 16 * index as usize) as *mut u64;
        // Layout: addr(8) len(4) flags(2) next(2) — two u64 stores cover
        // it; the device reads only after we publish the avail entry.
        ptr::write_volatile(desc, addr);
        ptr::write_volatile(
            desc.add(1),
            (len as u64) | ((flags as u64) << 32) | ((next as u64) << 48),
        );
    }

    fn read_used_idx(&self) -> u16 {
        // SAFETY: used.idx lives at used_at()+2 of our own region.
        unsafe { ptr::read_volatile((self.queue.used_at() + 2) as *const u16) }
    }
}

impl BlockDevice for VirtioBlkDevice {
    fn sector_count(&self) -> u64 {
        self.capacity_sectors
    }

    fn read_sectors(&mut self, lba: u64, count: usize) -> Result<Vec<u8>, BlockError> {
        if count == 0 || count > MAX_DATA_SECTORS {
            return Err(BlockError::OutOfBounds);
        }
        if lba >= self.capacity_sectors
            || count as u64 > self.capacity_sectors - lba
        {
            return Err(BlockError::OutOfBounds);
        }

        let q = self.queue_size() as usize;
        // Single in-flight request: descriptors 0 (header), 1 (data),
        // 2 (status); avail head 0. Freed implicitly by overwriting on the
        // next request.
        // SAFETY: queue_size >= MIN_QUEUE_SIZE = 8 > 2; DMA region is ours.
        unsafe {
            self.write_desc(
                0,
                self.queue.req_at() as u64,
                16,
                DESC_F_NEXT,
                1,
            );
            self.write_desc(
                1,
                self.queue.data_at() as u64,
                (count * SECTOR_SIZE) as u32,
                DESC_F_NEXT | DESC_F_WRITE,
                2,
            );
            self.write_desc(2, self.queue.status_at() as u64, 1, DESC_F_WRITE, 0);
        }

        // Request header: read request for `lba`.
        // SAFETY: 16 bytes at req_at inside our region.
        unsafe {
            let hdr = self.queue.req_at() as *mut u64;
            ptr::write_volatile(hdr, VIRTIO_BLK_T_IN as u64);
            ptr::write_volatile(hdr.add(1), lba);
        }

        // Publish: fill the avail slot, then bump avail.idx. Release
        // fence orders the descriptor/header stores (x86 is already TSO;
        // this is for the compiler).
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        let head = self.avail_idx;
        // SAFETY: avail.ring[head % q] inside our region.
        unsafe {
            ptr::write_volatile(
                (self.queue.avail_at() + 4 + 2 * (head as usize % q)) as *mut u16,
                0, // head descriptor index
            );
        }
        self.avail_idx = head.wrapping_add(1);
        // SAFETY: avail.idx inside our region.
        unsafe {
            ptr::write_volatile((self.queue.avail_at() + 2) as *mut u16, self.avail_idx);
        }

        // Kick the device.
        self.out16(REG_QUEUE_NOTIFY, 0);

        // Poll for completion: used.idx advances past last_used_idx.
        let mut waited = 0usize;
        while self.read_used_idx() == self.last_used_idx {
            waited += 1;
            if waited > POLL_LIMIT {
                return Err(BlockError::Timeout);
            }
            core::hint::spin_loop();
        }
        // The device wrote the descriptor buffers (status, data) via DMA
        // before publishing used.idx: acquire fence between observing
        // used.idx and reading status/data (pairs with the submit-side
        // release fence; on x86 this is a compiler barrier).
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

        // Consume THE used entry for this request. Entries are 8 bytes
        // (id u32 + len u32) starting at used-ring offset 4, indexed by
        // last_used_idx % queue_size; then advance our index. Slot-based
        // (not entry-0) access keeps wrap-around and future batching of
        // several in-flight requests correct.
        let slot = self.last_used_idx as usize % self.queue_size() as usize;
        // SAFETY: used ring entry inside our region.
        let (used_id, used_len) = unsafe {
            let entry = (self.queue.used_at() + 4 + 8 * slot) as *const u32;
            (ptr::read_volatile(entry), ptr::read_volatile(entry.add(1)))
        };
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        if used_id != 0 {
            return Err(BlockError::ProtocolError);
        }
        let _ = used_len; // informational; the status byte governs success
        // Status byte: 0 = OK, 1 = IOERR, 2 = UNSUPPORTED.
        // SAFETY: status byte inside our region.
        let status =
            unsafe { ptr::read_volatile(self.queue.status_at() as *const u8) };
        if status != 0 {
            return Err(BlockError::IoError);
        }

        // Copy the data out to a heap Vec. Device writes were volatile
        // reads/writes from our side's perspective... the device wrote via
        // DMA; reading through non-volatile slice is fine on x86 (TSO,
        // coherent caches) after the used-ring read above.
        let mut out = Vec::with_capacity(count * SECTOR_SIZE);
        // SAFETY: data region holds exactly count*512 written bytes.
        unsafe {
            let src = core::slice::from_raw_parts(
                self.queue.data_at() as *const u8,
                count * SECTOR_SIZE,
            );
            out.extend_from_slice(src);
        }
        Ok(out)
    }
}

impl VirtioBlkDevice {
    fn queue_size(&self) -> u16 {
        self.queue.queue_size
    }
}

impl Drop for VirtioBlkDevice {
    fn drop(&mut self) {
        // The device object lives for the kernel's lifetime (parked in
        // BLOCK_DEVICE and never replaced), so this never runs in
        // practice; implemented for correctness so a future re-init would
        // return the DMA region instead of leaking it.
        if self.queue.len != 0 {
            // SAFETY: matches the allocation in setup_queue.
            unsafe {
                dealloc(
                    self.queue.base as *mut u8,
                    Layout::from_size_align(self.queue.len, 4096).unwrap(),
                );
            }
            self.queue.len = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Global registration + free-function access layer
// ---------------------------------------------------------------------------

/// Summary of a successful init, for boot-log and `disks` output.
#[derive(Clone, Copy)]
pub struct DeviceInfo {
    pub pci: (u8, u8, u8),
    pub io_base: u16,
    pub queue_size: u16,
    pub capacity_sectors: u64,
    pub read_only: bool,
}

static BLOCK_DEVICE: Mutex<Option<VirtioBlkDevice>> = Mutex::new(None);

/// Probe, initialize and register the virtio-blk device. Safe to call
/// once, in thread context, after PCI enumeration and the heap.
pub fn init() -> Result<DeviceInfo, &'static str> {
    let device = VirtioBlkDevice::probe_and_init()?;
    let info = DeviceInfo {
        pci: device.pci,
        io_base: device.io_base,
        queue_size: device.queue_size(),
        capacity_sectors: device.sector_count(),
        read_only: device.read_only,
    };
    *BLOCK_DEVICE.lock() = Some(device);
    Ok(info)
}

/// Info block for `disks`, or `None` when init failed / never ran.
pub fn device_info() -> Option<DeviceInfo> {
    let guard = BLOCK_DEVICE.lock();
    guard.as_ref().map(|dev| DeviceInfo {
        pci: dev.pci,
        io_base: dev.io_base,
        queue_size: dev.queue_size(),
        capacity_sectors: dev.capacity_sectors,
        read_only: dev.read_only,
    })
}

/// Run `f` with exclusive access to the registered device.
pub fn with_device<R>(
    f: impl FnOnce(&mut VirtioBlkDevice) -> R,
) -> Result<R, &'static str> {
    let mut guard = BLOCK_DEVICE.lock();
    let dev = guard.as_mut().ok_or("no block device initialized")?;
    Ok(f(dev))
}
