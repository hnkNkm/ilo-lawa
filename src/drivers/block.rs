//! Block device abstraction (issue #14).
//!
//! One trait ([`BlockDevice`]) for 512-byte-sector devices, plus the
//! free-function access layer ([`read_sectors`], [`sector_count`]) the
//! FAT32 driver and shell commands use. The kernel has a single data disk
//! (the virtio-blk device, see `virtio_blk.rs`), so the access layer
//! forwards to that registered device instead of a registry of many;
//! future devices (AHCI, NVMe) plug in by implementing the trait.
//!
//! Read-only for now: write support is tracked separately in issue #14's
//! remaining scope, so the trait deliberately has no write method yet.
//!
//! Locking: the concrete device sits behind a `spin::Mutex` in
//! `virtio_blk` and is thread-context only (polled I/O, never from an
//! interrupt handler).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Size of one logical block in bytes.
pub const SECTOR_SIZE: usize = 512;

/// Errors returned by block device operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// Sector range does not exist on the device.
    OutOfBounds,
    /// Device reported an I/O error for the request.
    IoError,
    /// Polled completion did not arrive within the wait bound.
    Timeout,
    /// Device state/registers inconsistent with what the driver sent.
    ProtocolError,
}

impl core::fmt::Display for BlockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let msg = match self {
            BlockError::OutOfBounds => "sector out of bounds",
            BlockError::IoError => "device I/O error",
            BlockError::Timeout => "completion timed out",
            BlockError::ProtocolError => "device protocol error",
        };
        f.write_str(msg)
    }
}

/// A 512-byte-sector block device.
pub trait BlockDevice: Send {
    /// Read `count` sectors starting at logical block `lba`. Returns
    /// exactly `count * SECTOR_SIZE` bytes on success.
    fn read_sectors(&mut self, lba: u64, count: usize) -> Result<Vec<u8>, BlockError>;

    /// Device size in 512-byte sectors.
    fn sector_count(&self) -> u64;
}

/// Read sectors from the (single) registered block device. The string
/// error matches the `Result<_, String>` convention of the fs/shell layer.
pub fn read_sectors(lba: u64, count: usize) -> Result<Vec<u8>, String> {
    match crate::drivers::virtio_blk::with_device(|dev| dev.read_sectors(lba, count)) {
        Ok(Ok(data)) => Ok(data),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}
