//! GPT writer: protective MBR, primary and backup header + entries (512-byte
//! sectors, 128 entries of 128 bytes), and GUID helpers.

pub const ESP_TYPE: [u8; 16] = guid_le(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
pub const LINUX_TYPE: [u8; 16] = guid_le(0x0FC63DAF, 0x8483, 0x4772, [0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);
pub const HOME_TYPE: [u8; 16] = guid_le(0x933AC7E1, 0x2EB4, 0x4F13, [0xB8, 0x44, 0x0E, 0x14, 0xE2, 0xAE, 0xF9, 0x15]);
pub const LOG_TYPE: [u8; 16] = guid_le(0x042B7470, 0x1302, 0x4E61, [0x93, 0x47, 0xA0, 0x13, 0x0B, 0x27, 0x67, 0x54]);

pub const SECTOR: u64 = 512;
pub const NENTRIES: usize = 128;
pub const ESIZE: usize = 128;

pub const fn guid_le(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

/// Lower-case text form of an on-disk (mixed-endian) GUID; this is the
/// kernel's PARTUUID text.
pub fn guid_text(g: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes(g[0..4].try_into().unwrap());
    let d2 = u16::from_le_bytes(g[4..6].try_into().unwrap());
    let d3 = u16::from_le_bytes(g[6..8].try_into().unwrap());
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        d1, d2, d3, g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    )
}

/// A random version-4 GUID in on-disk byte order.
pub fn random_guid() -> std::io::Result<[u8; 16]> {
    use std::io::Read;
    let mut g = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut g)?;
    g[7] = (g[7] & 0x0F) | 0x40;
    g[8] = (g[8] & 0x3F) | 0x80;
    Ok(g)
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

#[derive(Clone, Debug)]
pub struct PartSpec {
    pub ptype: [u8; 16],
    pub guid: [u8; 16],
    pub first: u64,
    /// Inclusive.
    pub last: u64,
    pub name: &'static str,
}

pub struct Tables {
    pub mbr: [u8; 512],
    /// LBA 1 (padded to a sector).
    pub primary_hdr: [u8; 512],
    /// LBA 2.., 128 * 128 bytes.
    pub entries: Vec<u8>,
    /// LBA total-1 (padded to a sector).
    pub backup_hdr: [u8; 512],
}

/// Build the tables for a disk of `total` sectors. The i-th spec becomes
/// partition i+1.
pub fn build(total: u64, disk_guid: &[u8; 16], parts: &[PartSpec]) -> Tables {
    assert!(parts.len() <= NENTRIES);
    let mut mbr = [0u8; 512];
    // Protective entry: type 0xEE, LBA 1 .. end (capped at 32 bits).
    let e = &mut mbr[446..462];
    e[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    e[4] = 0xEE;
    e[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    e[8..12].copy_from_slice(&1u32.to_le_bytes());
    e[12..16].copy_from_slice(&(((total - 1).min(0xFFFF_FFFF)) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    let mut entries = vec![0u8; NENTRIES * ESIZE];
    for (i, p) in parts.iter().enumerate() {
        let e = &mut entries[i * ESIZE..(i + 1) * ESIZE];
        e[0..16].copy_from_slice(&p.ptype);
        e[16..32].copy_from_slice(&p.guid);
        e[32..40].copy_from_slice(&p.first.to_le_bytes());
        e[40..48].copy_from_slice(&p.last.to_le_bytes());
        for (j, u) in p.name.encode_utf16().take(36).enumerate() {
            e[56 + j * 2..58 + j * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    let ecrc = crc32(&entries);
    let first_usable = 34u64;
    let last_usable = total - 34;
    let header = |my: u64, other: u64, ent_lba: u64| -> [u8; 512] {
        let mut h = [0u8; 512];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my.to_le_bytes());
        h[32..40].copy_from_slice(&other.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(disk_guid);
        h[72..80].copy_from_slice(&ent_lba.to_le_bytes());
        h[80..84].copy_from_slice(&(NENTRIES as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(ESIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&ecrc.to_le_bytes());
        let c = crc32(&h[0..92]);
        h[16..20].copy_from_slice(&c.to_le_bytes());
        h
    };
    Tables {
        mbr,
        primary_hdr: header(1, total - 1, 2),
        entries,
        backup_hdr: header(total - 1, 1, total - 33),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn guid_roundtrip() {
        assert_eq!(guid_text(&ESP_TYPE), "c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
    }

    #[test]
    fn tables_valid() {
        let g = [7u8; 16];
        let p = PartSpec { ptype: LINUX_TYPE, guid: g, first: 2048, last: 4095, name: "x" };
        let t = build(1 << 20, &g, &[p]);
        assert_eq!(&t.primary_hdr[0..8], b"EFI PART");
        assert_eq!(&t.mbr[510..512], &[0x55, 0xAA]);
        let mut h = t.primary_hdr;
        let c = u32::from_le_bytes(h[16..20].try_into().unwrap());
        h[16..20].fill(0);
        assert_eq!(crc32(&h[0..92]), c);
    }
}
