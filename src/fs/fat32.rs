//! Read-only FAT32 driver over the registered block device (issue #14).
//!
//! The disk is a raw image attached via virtio-blk (`make disk`); sector 0
//! is either the FAT32 boot sector itself (superfloppy, what `mkfs.vfat`
//! produces) or an MBR whose FAT32-typed partition (types 0x0B/0x0C) holds
//! the boot sector — both layouts are detected at mount time.
//!
//! Read-only: boot sector validation, on-demand FAT sector reads (one
//! sector cached), FAT chain walking, directory parsing with 8.3 short
//! names (honoring the NT lowercase flags) and LFN (long file name)
//! assembly with checksum verification, path resolution and file reads.
//! Everything runs in thread context; the instance sits behind `FAT32`.
//!
//! This deliberately does NOT implement the `FileSystem` trait of
//! `fs::mod`: that trait is the in-RAM root filesystem behind `ls`/`cd`/
//! `cat`, and the data disk is a separate, read-only surface exposed via
//! `fatls`/`fatcat`. Replacing the RAM fs wholesale is future work.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

use crate::drivers::block;

const SECTOR: usize = 512; // validated against the BPB
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LONG_NAME: u8 = 0x0F; // RO|HIDDEN|SYSTEM|VOLUME_ID
const DIR_ENTRY_SIZE: usize = 32;

/// Parsed FAT32 BIOS Parameter Block (the fields the driver needs).
#[derive(Clone, Copy)]
struct Bpb {
    sectors_per_cluster: u8,
    reserved_sectors: u16,
    num_fats: u8,
    fat_size_sectors: u32,
    root_cluster: u32,
    total_sectors: u64,
}

impl Bpb {
    /// Data region start, relative to the volume base (LBA of cluster 2).
    fn data_start(&self) -> u64 {
        u64::from(self.reserved_sectors)
            + u64::from(self.num_fats) * u64::from(self.fat_size_sectors)
    }

    fn cluster_lba(&self, base: u64, cluster: u32) -> u64 {
        base + self.data_start() + (u64::from(cluster) - 2) * u64::from(self.sectors_per_cluster)
    }

    fn total_clusters(&self) -> u64 {
        (self.total_sectors - self.data_start()) / u64::from(self.sectors_per_cluster)
    }
}

/// Validate a sector as a FAT32 boot sector and extract the BPB. Field
/// reads are explicit little-endian — no unaligned pointer casts.
fn parse_boot_sector(sec: &[u8]) -> Option<Bpb> {
    if sec.len() < SECTOR {
        return None;
    }
    let u16_at = |off: usize| u16::from_le_bytes([sec[off], sec[off + 1]]);
    let u32_at =
        |off: usize| u32::from_le_bytes([sec[off], sec[off + 1], sec[off + 2], sec[off + 3]]);

    // Boot signature + the FAT32 markers: 512-byte sectors, no FAT12/16
    // root-dir area, FAT size only in the 32-bit field.
    if sec[510] != 0x55 || sec[511] != 0xAA {
        return None;
    }
    if u16_at(11) != 512 || u16_at(17) != 0 || u16_at(19) != 0 || u16_at(22) != 0 {
        return None;
    }
    let sectors_per_cluster = sec[13];
    let num_fats = sec[16];
    let fat_size_sectors = u32_at(36);
    let root_cluster = u32_at(44);
    let total_sectors = {
        let t32 = u32_at(32);
        if t32 != 0 {
            u64::from(t32)
        } else {
            u64::from(u16_at(19))
        }
    };
    // Geometry sanity: nonzero power-of-two cluster, at least one FAT, a
    // data region that actually exists, root dir in the data region.
    if !sectors_per_cluster.is_power_of_two()
        || num_fats == 0
        || fat_size_sectors == 0
        || root_cluster < 2
    {
        return None;
    }
    let data_start = u64::from(u16_at(14))
        + u64::from(num_fats) * u64::from(fat_size_sectors);
    if total_sectors <= data_start {
        return None;
    }
    Some(Bpb {
        sectors_per_cluster,
        reserved_sectors: u16_at(14),
        num_fats,
        fat_size_sectors,
        root_cluster,
        total_sectors,
    })
}

/// Locate the volume: superfloppy (sector 0 is the boot sector) or MBR
/// with a FAT32 partition. Returns (BPB, volume base LBA, boot sector).
fn detect_volume(sector0: &[u8]) -> Result<(Bpb, u64, [u8; SECTOR]), String> {
    if let Some(bpb) = parse_boot_sector(sector0) {
        let mut boot = [0u8; SECTOR];
        boot.copy_from_slice(&sector0[..SECTOR]);
        return Ok((bpb, 0, boot));
    }

    // MBR: signature + one FAT32-typed partition entry (types 0x0B/0x0C).
    if sector0.len() >= SECTOR && sector0[510] == 0x55 && sector0[511] == 0xAA {
        for i in 0..4 {
            let entry = &sector0[446 + 16 * i..446 + 16 * i + 16];
            let part_type = entry[4];
            let lba = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
            if part_type == 0x0B || part_type == 0x0C {
                let boot = block::read_sectors(u64::from(lba), 1)?;
                if let Some(bpb) = parse_boot_sector(&boot) {
                    let mut sector = [0u8; SECTOR];
                    sector.copy_from_slice(&boot[..SECTOR]);
                    return Ok((bpb, u64::from(lba), sector));
                }
            }
        }
        return Err("no FAT32 partition in MBR (need type 0x0B/0x0C)".into());
    }
    Err("sector 0 is neither a FAT32 boot sector nor an MBR".into())
}

/// Volume label from the boot sector (bytes 71..=81, space-padded).
fn volume_label(boot: &[u8; SECTOR]) -> String {
    let mut label = String::new();
    for &b in &boot[71..82] {
        if b != 0x20 && b != 0x00 {
            label.push(b as char);
        }
    }
    if label.is_empty() {
        label.push_str("NO NAME");
    }
    label
}

/// One directory entry as surfaced to callers (LFN applied when valid).
#[derive(Clone)]
pub struct FatDirEntry {
    pub name: String,
    pub is_directory: bool,
    pub size: u32,
    pub first_cluster: u32,
}

/// Mounted read-only FAT32 volume.
pub struct Fat32FileSystem {
    bpb: Bpb,
    /// LBA where the volume (boot sector) starts: 0 for a superfloppy,
    /// the MBR partition base otherwise.
    base_lba: u64,
    root_cluster: u32,
    volume_label: String,
    /// Recently used FAT sector cache: `fat_cache_sector` is volume-
    /// relative (FAT start excluded); `u32::MAX` = empty.
    fat_cache_sector: u32,
    fat_cache: [u8; SECTOR],
}

impl Fat32FileSystem {
    /// Detect and mount the volume on the registered block device.
    pub fn mount() -> Result<Fat32FileSystem, String> {
        let sector0 = block::read_sectors(0, 1)?;
        let (bpb, base_lba, boot) = detect_volume(&sector0)?;
        let mut fs = Fat32FileSystem {
            bpb,
            base_lba,
            root_cluster: bpb.root_cluster,
            volume_label: volume_label(&boot),
            fat_cache_sector: u32::MAX,
            fat_cache: [0; SECTOR],
        };
        fs.fat_entry(2)?; // probe: the FAT must be readable to be usable
        Ok(fs)
    }

    /// FAT entry for `cluster` (masked to 28 bits), via the cached sector.
    fn fat_entry(&mut self, cluster: u32) -> Result<u32, String> {
        let fat_offset = cluster * 4;
        let sector = fat_offset / SECTOR as u32;
        let offset = (fat_offset % SECTOR as u32) as usize;
        if self.fat_cache_sector != sector {
            let lba = self.base_lba + u64::from(self.bpb.reserved_sectors) + u64::from(sector);
            let data = block::read_sectors(lba, 1)?;
            self.fat_cache.copy_from_slice(&data[..SECTOR]);
            self.fat_cache_sector = sector;
        }
        Ok(u32::from_le_bytes([
            self.fat_cache[offset],
            self.fat_cache[offset + 1],
            self.fat_cache[offset + 2],
            self.fat_cache[offset + 3],
        ]) & 0x0FFF_FFFF)
    }

    /// Read the whole cluster chain of `start` (capped at the volume's
    /// cluster count so a corrupt FAT cannot loop forever).
    fn read_chain(&mut self, start: u32) -> Result<Vec<u8>, String> {
        let spc = self.bpb.sectors_per_cluster as usize;
        let mut data = Vec::new();
        let mut cluster = start;
        let max_hops = self.bpb.total_clusters() + 1;
        let mut hops = 0u64;
        loop {
            if cluster < 2 {
                return Err("FAT chain: invalid cluster".into());
            }
            let lba = self.bpb.cluster_lba(self.base_lba, cluster);
            data.extend_from_slice(&block::read_sectors(lba, spc)?);
            hops += 1;
            if hops > max_hops {
                return Err("FAT chain: loop detected".into());
            }
            let next = self.fat_entry(cluster)?;
            // >= 0x0FFFFFF8 = end of chain; 0x0FFFFFF7 = bad cluster.
            if next >= 0x0FFF_FFF8 {
                break;
            }
            if next == 0x0FFF_FFF7 {
                return Err("FAT chain: bad cluster".into());
            }
            cluster = next;
        }
        Ok(data)
    }

    /// Parse one directory cluster chain into entries. LFN fragments are
    /// assembled when their sequence numbers and checksum line up with
    /// the following short entry.
    fn read_dir(&mut self, dir_cluster: u32) -> Result<Vec<FatDirEntry>, String> {
        let raw = self.read_chain(dir_cluster)?;
        let mut entries = Vec::new();
        // Collected LFN fragments in disk order (sequence n..1, the short
        // entry last): (seq, checksum, 13 UCS-2 chars).
        let mut lfn_parts: Vec<(u8, u8, [u16; 13])> = Vec::new();

        for chunk in raw.chunks_exact(DIR_ENTRY_SIZE) {
            match chunk[0] {
                0x00 => break, // end-of-directory marker
                0xE5 => {
                    lfn_parts.clear(); // deleted entry: drop stale fragments
                    continue;
                }
                _ => {}
            }
            let attr = chunk[11];

            if attr == ATTR_LONG_NAME {
                let seq = chunk[0] & 0x1F;
                let mut chars = [0u16; 13];
                let offsets = [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                for (n, &off) in offsets.iter().enumerate() {
                    chars[n] = u16::from_le_bytes([chunk[off], chunk[off + 1]]);
                }
                lfn_parts.push((seq, chunk[13], chars));
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                lfn_parts.clear();
                continue; // volume label pseudo-entry
            }

            let short = Self::short_name(chunk);
            let name =
                Self::assemble_lfn(&lfn_parts, &chunk[..11]).unwrap_or_else(|| short.clone());
            lfn_parts.clear();

            let first_cluster = (u32::from(u16::from_le_bytes([chunk[20], chunk[21]])) << 16)
                | u32::from(u16::from_le_bytes([chunk[26], chunk[27]]));
            entries.push(FatDirEntry {
                name,
                is_directory: attr & ATTR_DIRECTORY != 0,
                size: u32::from_le_bytes([chunk[28], chunk[29], chunk[30], chunk[31]]),
                first_cluster,
            });
        }
        Ok(entries)
    }

    /// 8.3 short name, honoring the NT lowercase flags in the ntres byte
    /// (bit 3 = base lowercase, bit 4 = extension lowercase).
    fn short_name(chunk: &[u8]) -> String {
        let mut name = String::new();
        let lower_base = chunk[12] & 0x08 != 0;
        let lower_ext = chunk[12] & 0x10 != 0;
        for &b in &chunk[..8] {
            if b == 0x20 {
                break;
            }
            name.push(if lower_base {
                (b as char).to_ascii_lowercase()
            } else {
                b as char
            });
        }
        if chunk[8] != 0x20 {
            name.push('.');
            for &b in &chunk[8..11] {
                if b == 0x20 {
                    break;
                }
                name.push(if lower_ext {
                    (b as char).to_ascii_lowercase()
                } else {
                    b as char
                });
            }
        }
        name
    }

    /// LFN checksum over the 11 raw name bytes of a short entry:
    /// rotate the running sum right by one, then add the byte.
    fn lfn_checksum(short_name_bytes: &[u8]) -> u8 {
        let mut sum = 0u8;
        for &b in &short_name_bytes[..11] {
            sum = sum.rotate_right(1).wrapping_add(b);
        }
        sum
    }

    /// Reassemble an LFN from the collected fragments when valid: sequence
    /// numbers must run 1..=n ascending, every fragment's stored checksum
    /// (its byte 13) must equal the checksum of the short entry's 11 name
    /// bytes, and characters are mapped as ASCII (anything else becomes
    /// '?'). 13 chars per fragment, NUL-terminated, 0xFFFF-padded.
    fn assemble_lfn(
        parts: &[(u8, u8, [u16; 13])],
        short_name_bytes: &[u8],
    ) -> Option<String> {
        if parts.is_empty() {
            return None;
        }
        let expected = Self::lfn_checksum(short_name_bytes);
        let mut ordered = parts.to_vec();
        ordered.sort_by_key(|(seq, _, _)| *seq);
        for (i, (seq, checksum, _)) in ordered.iter().enumerate() {
            if *seq != (i + 1) as u8 || *checksum != expected {
                return None;
            }
        }
        let mut name = String::new();
        for (_, _, chars) in &ordered {
            for &c in chars {
                match c {
                    0x0000 | 0xFFFF => return Some(name),
                    0x20..=0x7E => name.push(c as u8 as char),
                    _ => name.push('?'), // non-ASCII: keep position stable
                }
            }
        }
        Some(name)
    }

    /// Case-insensitive name match (8.3 and LFN are compared uppercase).
    fn name_eq(a: &str, b: &str) -> bool {
        a.eq_ignore_ascii_case(b)
    }

    /// Resolve a slash-separated path (always from the volume root — the
    /// read-only surface has no persistent FAT cwd) to its entry.
    fn resolve(&mut self, path: &str) -> Result<FatDirEntry, String> {
        let mut current = FatDirEntry {
            name: String::from("/"),
            is_directory: true,
            size: 0,
            first_cluster: self.root_cluster,
        };
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            if !current.is_directory {
                return Err(format!("not a directory: {}", current.name));
            }
            let entries = self.read_dir(current.first_cluster)?;
            let entry = entries
                .iter()
                .find(|e| Self::name_eq(&e.name, comp))
                .ok_or_else(|| format!("no such entry: {}", comp))?;
            let mut entry = entry.clone();
            // "." stays; ".." may encode the root as cluster 0.
            if Self::name_eq(&entry.name, "..") && entry.first_cluster == 0 {
                entry.first_cluster = self.root_cluster;
            }
            current = entry;
        }
        Ok(current)
    }

    /// List a directory path; "" and "/" mean the root.
    pub fn list(&mut self, path: &str) -> Result<Vec<FatDirEntry>, String> {
        let dir = self.resolve(path)?;
        if !dir.is_directory {
            return Err(format!("not a directory: {}", path));
        }
        self.read_dir(dir.first_cluster)
    }

    /// Read a file's contents (truncated to the directory-entry size).
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        let file = self.resolve(path)?;
        if file.is_directory {
            return Err(format!("is a directory: {}", path));
        }
        if file.size == 0 || file.first_cluster < 2 {
            return Ok(Vec::new());
        }
        let mut data = self.read_chain(file.first_cluster)?;
        data.truncate(file.size as usize);
        Ok(data)
    }

    /// Volume label + geometry summary for `disks`/boot log.
    pub fn summary(&self) -> String {
        let size_kib = self.bpb.total_clusters()
            * u64::from(self.bpb.sectors_per_cluster)
            * 512
            / 1024;
        format!(
            "FAT32 \"{}\": {} KiB, {} sector(s)/cluster, volume at LBA {}",
            self.volume_label,
            size_kib,
            self.bpb.sectors_per_cluster,
            self.base_lba
        )
    }
}

static FAT32: Mutex<Option<Fat32FileSystem>> = Mutex::new(None);

/// Mount the data disk. Returns the summary string on success. Thread
/// context only, after `virtio_blk::init`.
pub fn mount() -> Result<String, String> {
    let fs = Fat32FileSystem::mount()?;
    let summary = fs.summary();
    *FAT32.lock() = Some(fs);
    Ok(summary)
}

/// Run `f` with the mounted volume.
pub fn with_fs<R>(f: impl FnOnce(&mut Fat32FileSystem) -> Result<R, String>) -> Result<R, String> {
    let mut guard = FAT32.lock();
    let fs = guard.as_mut().ok_or("no FAT32 volume mounted")?;
    f(fs)
}
