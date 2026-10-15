//! Partition discovery: every whole disk in /dev (sdX, vdX, nvmeXnY,
//! mmcblkN), its MBR or GPT, and the partition nodes the kernel created for
//! it. Partition numbers follow the kernel's scan (MBR slot / GPT entry
//! index, 1-based), so `/dev/sda<n>` is the n-th table entry.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Esp,
    /// The raw console-log partition (kernel/src/klog.rs).
    Log,
    Linux,
    /// The Discoverable Partitions /home type (GPT only; on MBR the home
    /// partition is a second 0x83 and reads as Linux).
    Home,
    BiosBoot,
    Other(String),
}

impl Kind {
    pub fn name(&self) -> String {
        match self {
            Kind::Esp => "esp".into(),
            Kind::Log => "log".into(),
            Kind::Linux => "linux".into(),
            Kind::Home => "home".into(),
            Kind::BiosBoot => "bios-boot".into(),
            Kind::Other(s) => s.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Part {
    pub disk: String,
    pub dev: String,
    pub pno: u32,
    pub start: u64,
    pub size: u64,
    pub kind: Kind,
}

const ESP_GUID: [u8; 16] = guid_le(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
const LOG_GUID: [u8; 16] = guid_le(0x042B7470, 0x1302, 0x4E61, [0x93, 0x47, 0xA0, 0x13, 0x0B, 0x27, 0x67, 0x54]);
const LINUX_GUID: [u8; 16] = guid_le(0x0FC63DAF, 0x8483, 0x4772, [0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);
const HOME_GUID: [u8; 16] = guid_le(0x933AC7E1, 0x2EB4, 0x4F13, [0xB8, 0x44, 0x0E, 0x14, 0xE2, 0xAE, 0xF9, 0x15]);
const BIOS_GUID: [u8; 16] = guid_le(0x21686148, 0x6449, 0x6E6F, [0x74, 0x4E, 0x65, 0x65, 0x64, 0x45, 0x46, 0x49]);

const fn guid_le(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

fn is_whole_disk(name: &str) -> bool {
    let b = name.as_bytes();
    if (name.starts_with("sd") || name.starts_with("vd")) && b.len() > 2 {
        return b[2..].iter().all(|c| c.is_ascii_lowercase());
    }
    if let Some(rest) = name.strip_prefix("nvme") {
        // nvme0n1, not nvme0n1p1
        return rest.contains('n') && !rest.contains('p') && rest.bytes().all(|c| c.is_ascii_digit() || c == b'n');
    }
    if let Some(rest) = name.strip_prefix("mmcblk") {
        return !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit());
    }
    false
}

pub fn part_dev(disk: &str, pno: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("/dev/{}p{}", disk, pno)
    } else {
        format!("/dev/{}{}", disk, pno)
    }
}

pub fn whole_disks() -> Vec<String> {
    let mut v: Vec<String> = match std::fs::read_dir("/dev") {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| is_whole_disk(n)).collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

fn read_at(f: &mut File, off: u64, len: usize) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut buf = vec![0u8; len];
    let mut done = 0;
    while done < len {
        match f.read(&mut buf[done..]) {
            Ok(0) => return None,
            Ok(n) => done += n,
            Err(_) => return None,
        }
    }
    Some(buf)
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// The partitions of one whole disk, from its MBR or GPT (512-byte sectors;
/// a 4 KiB-sector GPT is tried too).
pub fn scan_disk(disk: &str) -> Vec<Part> {
    let mut out = Vec::new();
    let mut f = match File::open(format!("/dev/{}", disk)) {
        Ok(f) => f,
        Err(_) => return out,
    };
    let mbr = match read_at(&mut f, 0, 512) {
        Some(b) => b,
        None => return out,
    };
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return out;
    }
    let protective = (0..4).any(|i| mbr[446 + i * 16 + 4] == 0xEE);
    if !protective {
        for i in 0..4 {
            let e = &mbr[446 + i * 16..446 + i * 16 + 16];
            let ptype = e[4];
            let start = le32(e, 8) as u64;
            let len = le32(e, 12) as u64;
            if ptype == 0 || len == 0 {
                continue;
            }
            let kind = match ptype {
                0xEF => Kind::Esp,
                0xDA => Kind::Log,
                0x83 => Kind::Linux,
                t => Kind::Other(format!("mbr-{:02x}", t)),
            };
            let pno = i as u32 + 1;
            out.push(Part { disk: disk.into(), dev: part_dev(disk, pno), pno, start: start * 512, size: len * 512, kind });
        }
        return out;
    }
    for lbs in [512u64, 4096] {
        let hdr = match read_at(&mut f, lbs, 92) {
            Some(b) => b,
            None => continue,
        };
        if &hdr[0..8] != b"EFI PART" {
            continue;
        }
        let ent_lba = le64(&hdr, 72);
        let n = le32(&hdr, 80).min(256) as usize;
        let esz = le32(&hdr, 84) as usize;
        if esz < 128 || esz > 4096 {
            return out;
        }
        let ents = match read_at(&mut f, ent_lba * lbs, n * esz) {
            Some(b) => b,
            None => return out,
        };
        for i in 0..n {
            let e = &ents[i * esz..i * esz + esz];
            let t: [u8; 16] = e[0..16].try_into().unwrap();
            if t == [0u8; 16] {
                continue;
            }
            let first = le64(e, 32);
            let last = le64(e, 40);
            let kind = if t == ESP_GUID {
                Kind::Esp
            } else if t == LOG_GUID {
                Kind::Log
            } else if t == LINUX_GUID {
                Kind::Linux
            } else if t == HOME_GUID {
                Kind::Home
            } else if t == BIOS_GUID {
                Kind::BiosBoot
            } else {
                Kind::Other("gpt-other".into())
            };
            let pno = i as u32 + 1;
            out.push(Part { disk: disk.into(), dev: part_dev(disk, pno), pno, start: first * lbs, size: (last + 1 - first) * lbs, kind });
        }
        return out;
    }
    out
}

pub fn scan_all() -> Vec<Part> {
    whole_disks().iter().flat_map(|d| scan_disk(d)).collect()
}
