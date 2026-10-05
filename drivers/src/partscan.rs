//! Partition-table scanning: MBR (primary entries) and GPT.
//!
//! Pure code over a sector-read callback: no device access, no kernel
//! dependencies, nothing but `core` + `alloc`. That is what lets the host
//! unit tests in `drivers/partscan-tests` include this file verbatim with
//! `#[path]` and run it under `cargo test`, while the kernel registry
//! (`drivers/src/blkdev.rs`) calls the very same function against a real disk.
//!
//! # Rules (deliberately close to Linux `block/partitions/{msdos,efi}.c`)
//!
//! * Sector 0 must end in `55 AA`. Every one of the four primary entries must
//!   have a boot-indicator byte of `0x00` or `0x80`; anything else is a FAT/
//!   exFAT boot sector on a "superfloppy" (a stick formatted without a table),
//!   whose BPB bytes happen to overlap the entry area, and is not an MBR.
//! * Any primary entry of type `0xEE` (protective) makes this a GPT disk. The
//!   primary header at LBA 1 is checked (signature, header CRC32, `my_lba`,
//!   entry-array CRC32); on any failure the backup header at the last LBA is
//!   tried. If neither is valid the disk is reported as having no table at
//!   all — a protective entry is never registered as a partition.
//! * MBR: only primary entries are reported. Empty (type 0 / zero length) and
//!   extended-container entries (`0x05`, `0x0F`, `0x85`) are skipped, as are
//!   entries that run past the end of the disk when its size is known.
//! * GPT: entries with an all-zero type GUID are unused. The partition number
//!   is the 1-based slot index in the entry array (Linux numbering, so a
//!   table with a hole at slot 2 yields `sda1`, `sda3`).
//!
//! All LBAs are in units of the device's logical sector size, which is also
//! the unit GPT and MBR use on disk.

extern crate alloc;
use alloc::vec::Vec;

/// GPT type GUID of a Linux filesystem partition,
/// 0FC63DAF-8483-4772-8E79-3D69D8477DE4, in its on-disk (mixed-endian) form.
pub const LINUX_FS_GUID: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47,
    0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];
/// MBR partition type "Linux native".
pub const MBR_LINUX: u8 = 0x83;
/// MBR partition type of a GPT protective entry.
pub const MBR_PROTECTIVE: u8 = 0xEE;

/// Upper bound on GPT entries we are willing to read (the spec minimum is
/// 128; anything past this is a corrupt header, not a real disk).
const GPT_MAX_ENTRIES: u32 = 1024;

/// What kind of table a disk carries, and its disk-level identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    None,
    /// MBR disk signature (the u32 at byte 440).
    Mbr { disk_sig: u32 },
    /// GPT disk GUID (on-disk byte order). `backup` is true when the primary
    /// header was unusable and the backup header supplied the table.
    Gpt { disk_guid: [u8; 16], backup: bool },
}

/// The partition-type field: an MBR type byte or a GPT type GUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartType {
    Mbr(u8),
    Gpt([u8; 16]),
}

impl PartType {
    /// True for MBR type 0x83 or the GPT Linux-filesystem GUID.
    pub fn is_linux_fs(&self) -> bool {
        match self {
            PartType::Mbr(t) => *t == MBR_LINUX,
            PartType::Gpt(g) => *g == LINUX_FS_GUID,
        }
    }
}

/// The identity Linux exposes as `PARTUUID=`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartUuid {
    /// `SSSSSSSS-NN`: MBR disk signature + partition number, both hex.
    Mbr { disk_sig: u32, pno: u32 },
    /// The GPT entry's unique partition GUID (on-disk byte order).
    Gpt([u8; 16]),
}

impl PartUuid {
    /// Canonical lower-case text form, written into `out`; returns its length
    /// (36 for GPT, 11 for MBR).
    pub fn format(&self, out: &mut [u8; 36]) -> usize {
        fn hex(v: u8) -> u8 { if v < 10 { b'0' + v } else { b'a' + v - 10 } }
        let mut n = 0usize;
        let mut put = |b: u8, out: &mut [u8; 36]| { out[n] = b; n += 1; };
        match self {
            PartUuid::Mbr { disk_sig, pno } => {
                for i in (0..8).rev() { put(hex(((disk_sig >> (i * 4)) & 0xF) as u8), out); }
                put(b'-', out);
                let p = (*pno & 0xFF) as u8;
                put(hex(p >> 4), out);
                put(hex(p & 0xF), out);
            }
            PartUuid::Gpt(g) => {
                // Mixed endian: the first three groups are little-endian.
                const ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
                for (i, &bi) in ORDER.iter().enumerate() {
                    if i == 4 || i == 6 || i == 8 || i == 10 { put(b'-', out); }
                    put(hex(g[bi] >> 4), out);
                    put(hex(g[bi] & 0xF), out);
                }
            }
        }
        n
    }

    /// Case-insensitive comparison against a `PARTUUID=` value.
    pub fn matches(&self, s: &str) -> bool {
        let mut buf = [0u8; 36];
        let n = self.format(&mut buf);
        s.len() == n && s.bytes().zip(buf[..n].iter()).all(|(a, &b)| a.to_ascii_lowercase() == b)
    }
}

/// One partition found on a disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// 1-based partition number (`sda<pno>`).
    pub pno: u32,
    /// First sector, in device logical sectors.
    pub start_lba: u64,
    /// Length in device logical sectors.
    pub num_lbas: u64,
    pub ptype: PartType,
    pub uuid: PartUuid,
}

impl Partition {
    /// True when the partition starts on a 4096-byte boundary, which the
    /// 4096-byte block façade (and f2fs) requires.
    pub fn aligned_4k(&self, sector_size: u32) -> bool {
        (self.start_lba * sector_size as u64) % 4096 == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanResult {
    pub table: Table,
    pub parts: Vec<Partition>,
}

impl ScanResult {
    fn none() -> Self { ScanResult { table: Table::None, parts: Vec::new() } }
}

/// IEEE 802.3 CRC-32 (reflected, poly 0xEDB88320) — the one GPT uses.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn le32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn le64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

/// Scan a disk for a partition table.
///
/// * `sector_size` — the device's logical sector size (512 or 4096).
/// * `capacity` — disk size in sectors; 0 when unknown (no backup-GPT
///   fallback and no end-of-disk bounds checks are possible then).
/// * `read(lba, buf)` — read whole sectors starting at `lba` into `buf`
///   (`buf.len()` is a multiple of `sector_size`); false on I/O error.
pub fn scan<F: FnMut(u64, &mut [u8]) -> bool>(sector_size: u32, capacity: u64, mut read: F) -> ScanResult {
    let ss = sector_size as usize;
    if ss < 512 || ss > 4096 || !ss.is_power_of_two() { return ScanResult::none(); }
    let mut s0 = alloc::vec![0u8; ss];
    if !read(0, &mut s0) { return ScanResult::none(); }
    if s0[510] != 0x55 || s0[511] != 0xAA { return ScanResult::none(); }

    let mut protective = false;
    for i in 0..4 {
        let e = &s0[446 + 16 * i..446 + 16 * (i + 1)];
        if e[0] != 0x00 && e[0] != 0x80 { return ScanResult::none(); }
        if e[4] == MBR_PROTECTIVE { protective = true; }
    }

    if protective {
        if let Some(r) = gpt_at(1, ss, capacity, false, &mut read) { return r; }
        if capacity > 1 {
            if let Some(r) = gpt_at(capacity - 1, ss, capacity, true, &mut read) { return r; }
        }
        return ScanResult::none();
    }

    let disk_sig = le32(&s0, 440);
    let mut parts = Vec::new();
    for i in 0..4u32 {
        let o = 446 + 16 * i as usize;
        let ptype = s0[o + 4];
        let start = le32(&s0, o + 8) as u64;
        let num = le32(&s0, o + 12) as u64;
        if ptype == 0 || num == 0 || start == 0 { continue; }
        if matches!(ptype, 0x05 | 0x0F | 0x85) { continue; }
        if capacity != 0 && start + num > capacity { continue; }
        parts.push(Partition {
            pno: i + 1,
            start_lba: start,
            num_lbas: num,
            ptype: PartType::Mbr(ptype),
            uuid: PartUuid::Mbr { disk_sig, pno: i + 1 },
        });
    }
    ScanResult { table: Table::Mbr { disk_sig }, parts }
}

/// Parse and fully validate the GPT header at `lba`, then its entry array.
fn gpt_at<F: FnMut(u64, &mut [u8]) -> bool>(
    lba: u64, ss: usize, capacity: u64, backup: bool, read: &mut F,
) -> Option<ScanResult> {
    let mut h = alloc::vec![0u8; ss];
    if !read(lba, &mut h) { return None; }
    if &h[0..8] != b"EFI PART" { return None; }
    let hsize = le32(&h, 12) as usize;
    if hsize < 92 || hsize > ss { return None; }
    let want = le32(&h, 16);
    let mut hc = h[..hsize].to_vec();
    hc[16..20].fill(0);
    if crc32(&hc) != want { return None; }
    if le64(&h, 24) != lba { return None; }
    let first_usable = le64(&h, 40);
    let last_usable = le64(&h, 48);
    let mut disk_guid = [0u8; 16];
    disk_guid.copy_from_slice(&h[56..72]);
    let entry_lba = le64(&h, 72);
    let nent = le32(&h, 80);
    let esize = le32(&h, 84) as usize;
    let ecrc = le32(&h, 88);
    if nent == 0 || nent > GPT_MAX_ENTRIES { return None; }
    if esize < 128 || esize > 4096 || !esize.is_power_of_two() { return None; }
    if first_usable > last_usable { return None; }
    if capacity != 0 && (last_usable >= capacity || entry_lba >= capacity) { return None; }

    let bytes = nent as usize * esize;
    let nsec = (bytes + ss - 1) / ss;
    let mut arr = alloc::vec![0u8; nsec * ss];
    if !read(entry_lba, &mut arr) { return None; }
    if crc32(&arr[..bytes]) != ecrc { return None; }

    let mut parts = Vec::new();
    for i in 0..nent as usize {
        let e = &arr[i * esize..i * esize + 128];
        let mut tguid = [0u8; 16];
        tguid.copy_from_slice(&e[0..16]);
        if tguid == [0u8; 16] { continue; }
        let mut uguid = [0u8; 16];
        uguid.copy_from_slice(&e[16..32]);
        let first = le64(e, 32);
        let last = le64(e, 40);
        if first > last || first < first_usable || last > last_usable { continue; }
        parts.push(Partition {
            pno: i as u32 + 1,
            start_lba: first,
            num_lbas: last - first + 1,
            ptype: PartType::Gpt(tguid),
            uuid: PartUuid::Gpt(uguid),
        });
    }
    Some(ScanResult { table: Table::Gpt { disk_guid, backup }, parts })
}
