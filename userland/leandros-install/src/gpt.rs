//! GPT writer: protective MBR, primary and backup header + entries (512-byte
//! sectors, 128 entries of 128 bytes), and GUID helpers.

pub const ESP_TYPE: [u8; 16] = guid_le(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
pub const LINUX_TYPE: [u8; 16] = guid_le(0x0FC63DAF, 0x8483, 0x4772, [0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);
pub const HOME_TYPE: [u8; 16] = guid_le(0x933AC7E1, 0x2EB4, 0x4F13, [0xB8, 0x44, 0x0E, 0x14, 0xE2, 0xAE, 0xF9, 0x15]);
pub const LOG_TYPE: [u8; 16] = guid_le(0x042B7470, 0x1302, 0x4E61, [0x93, 0x47, 0xA0, 0x13, 0x0B, 0x27, 0x67, 0x54]);

/// LeandrOS-specific partition type GUIDs (generated once, fixed). Every
/// LeandrOS partition also carries `ATTR_NO_AUTO` so gpt-auto never mounts it.
/// The legacy 0FC63DAF/933AC7E1 types are still accepted on old sticks.
pub const LROOT_TYPE: [u8; 16] = guid_le(0x3F2D991B, 0xD385, 0x4FFF, [0x9C, 0xF1, 0xA0, 0xDB, 0x03, 0x06, 0x2D, 0xE5]);
pub const LHOME_TYPE: [u8; 16] = guid_le(0xF936DEE1, 0x99D9, 0x4BBC, [0x8D, 0x2A, 0xB7, 0x44, 0x7D, 0xB9, 0xF6, 0x50]);
pub const LLOG_TYPE: [u8; 16] = guid_le(0x3EC8FA02, 0x816A, 0x4D04, [0x87, 0x18, 0x64, 0xC3, 0x77, 0x1C, 0x51, 0x87]);
pub const LBOOT_TYPE: [u8; 16] = guid_le(0x0E02ED9A, 0xFEEE, 0x4CAB, [0xA2, 0x2F, 0xC8, 0x61, 0xAD, 0x7D, 0xE3, 0x07]);
/// GPT attribute bit 63: "do not automount" (systemd-gpt-auto honors it).
pub const ATTR_NO_AUTO: u64 = 1 << 63;

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
    pub attrs: u64,
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
        e[48..56].copy_from_slice(&p.attrs.to_le_bytes());
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


// ---------------------------------------------------------------------------
// GPT editor (F5): reads an existing table (any sector size) and changes only
// what it is told to; every other entry byte and header field is preserved.

use std::io::{Read, Seek, SeekFrom, Write};

/// 1 MiB alignment for new partitions and reported gaps.
pub const ALIGN_BYTES: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub ptype: [u8; 16],
    pub guid: [u8; 16],
    pub first: u64,
    /// Inclusive.
    pub last: u64,
    pub attrs: u64,
    pub name: String,
}

#[derive(Clone)]
pub struct Gpt {
    /// Logical sector size (512 or 4096).
    pub ss: u64,
    /// Disk size in sectors.
    pub total: u64,
    /// Raw header sectors, as read (first `hsize` bytes meaningful).
    pri: Vec<u8>,
    bak: Vec<u8>,
    /// Entry array, byte for byte (nent * esize bytes).
    entries: Vec<u8>,
    pub nent: usize,
    pub esize: usize,
    pub first_usable: u64,
    pub last_usable: u64,
    pub disk_guid: [u8; 16],
    /// True when the primary header was unusable and the backup was used.
    pub from_backup: bool,
}

fn le64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }
fn le32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }

fn read_at<D: Read + Seek>(d: &mut D, off: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut v = vec![0u8; len];
    d.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    d.read_exact(&mut v).map_err(|e| format!("read at {off}: {e}"))?;
    Ok(v)
}

fn write_at<D: Write + Seek>(d: &mut D, off: u64, data: &[u8]) -> Result<(), String> {
    d.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    d.write_all(data).map_err(|e| format!("write at {off}: {e}"))
}

/// Validate a header sector: signature, header CRC, my-LBA. Returns the entry
/// array location and expected CRC.
fn check_header(h: &[u8], my_lba: u64) -> Result<(u64, usize, usize, u32), String> {
    if &h[0..8] != b"EFI PART" { return Err("bad signature".into()); }
    let hs = le32(h, 12) as usize;
    if hs < 92 || hs > h.len() { return Err("bad header size".into()); }
    let mut t = h[0..hs].to_vec();
    t[16..20].fill(0);
    if crc32(&t) != le32(h, 16) { return Err("header CRC mismatch".into()); }
    if le64(h, 24) != my_lba { return Err("my_lba mismatch".into()); }
    let n = le32(h, 80) as usize;
    let es = le32(h, 84) as usize;
    if es < 128 || es % 8 != 0 || n == 0 || n > 4096 { return Err("bad entry geometry".into()); }
    Ok((le64(h, 72), n, es, le32(h, 88)))
}

fn set_header_crcs(h: &mut [u8], ecrc: u32) {
    let hs = le32(h, 12) as usize;
    h[88..92].copy_from_slice(&ecrc.to_le_bytes());
    h[16..20].fill(0);
    let c = crc32(&h[0..hs]);
    h[16..20].copy_from_slice(&c.to_le_bytes());
}

impl Gpt {
    /// Read the table of a disk of `total` sectors of `ss` bytes. Uses the
    /// primary copy, or the backup when the primary does not validate.
    pub fn read<D: Read + Seek>(d: &mut D, ss: u64, total: u64) -> Result<Gpt, String> {
        if ss != 512 && ss != 4096 { return Err(format!("unsupported sector size {ss}")); }
        let sz = ss as usize;
        let pri = read_at(d, ss, sz)?;
        let bak = read_at(d, (total - 1) * ss, sz)?;
        let mut last_err = String::new();
        for (from_backup, hdr, my) in [(false, &pri, 1u64), (true, &bak, total - 1)] {
            match check_header(hdr, my) {
                Ok((elba, n, es, ecrc)) => {
                    let bytes = n * es;
                    let ents = match read_at(d, elba * ss, bytes) { Ok(e) => e, Err(e) => { last_err = e; continue; } };
                    if crc32(&ents) != ecrc { last_err = "entry array CRC mismatch".into(); continue; }
                    let mut disk_guid = [0u8; 16];
                    disk_guid.copy_from_slice(&hdr[56..72]);
                    // When only one copy is good, rebuild the other from it on write.
                    let (p, b) = if from_backup {
                        let mut p = hdr.clone();
                        p[24..32].copy_from_slice(&1u64.to_le_bytes());
                        p[32..40].copy_from_slice(&(total - 1).to_le_bytes());
                        p[72..80].copy_from_slice(&2u64.to_le_bytes());
                        (p, hdr.clone())
                    } else {
                        (hdr.clone(), if check_header(&bak, total - 1).is_ok() { bak.clone() } else {
                            let mut b = hdr.clone();
                            let esec = ((bytes as u64) + ss - 1) / ss;
                            b[24..32].copy_from_slice(&(total - 1).to_le_bytes());
                            b[32..40].copy_from_slice(&1u64.to_le_bytes());
                            b[72..80].copy_from_slice(&(total - 1 - esec).to_le_bytes());
                            b
                        })
                    };
                    return Ok(Gpt {
                        ss, total, pri: p, bak: b, entries: ents, nent: n, esize: es,
                        first_usable: le64(hdr, 40), last_usable: le64(hdr, 48), disk_guid, from_backup,
                    });
                }
                Err(e) => last_err = e,
            }
        }
        Err(format!("no valid GPT: {last_err}"))
    }

    fn raw(&self, i: usize) -> &[u8] { &self.entries[i * self.esize..(i + 1) * self.esize] }
    fn raw_mut(&mut self, i: usize) -> &mut [u8] { let es = self.esize; &mut self.entries[i * es..(i + 1) * es] }

    /// Parsed entry `i` (0-based slot), or None when the slot is empty.
    pub fn entry(&self, i: usize) -> Option<Entry> {
        let e = self.raw(i);
        if e[0..16] == [0u8; 16] { return None; }
        let units: Vec<u16> = e[56..128].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        Some(Entry {
            ptype: e[0..16].try_into().unwrap(),
            guid: e[16..32].try_into().unwrap(),
            first: le64(e, 32),
            last: le64(e, 40),
            attrs: le64(e, 48),
            name: String::from_utf16_lossy(&units),
        })
    }

    /// All used entries with their 0-based slot index.
    pub fn used(&self) -> Vec<(usize, Entry)> {
        (0..self.nent).filter_map(|i| self.entry(i).map(|e| (i, e))).collect()
    }

    fn align_sectors(&self) -> u64 { ALIGN_BYTES / self.ss }

    /// Unallocated ranges (first, last inclusive) of at least 1 MiB, with
    /// `first` rounded up and the end rounded down to 1 MiB boundaries.
    pub fn gaps(&self) -> Vec<(u64, u64)> {
        let al = self.align_sectors();
        let mut used: Vec<(u64, u64)> = self.used().iter().map(|(_, e)| (e.first, e.last)).collect();
        used.sort();
        let mut out = Vec::new();
        let mut cur = self.first_usable;
        let mut push = |from: u64, to_excl: u64, out: &mut Vec<(u64, u64)>| {
            let f = (from + al - 1) / al * al;
            let e = to_excl / al * al; // exclusive, aligned down
            if e > f && (e - f) * self.ss >= ALIGN_BYTES { out.push((f, e - 1)); }
        };
        for (f, l) in used {
            if f > cur { push(cur, f, &mut out); }
            cur = cur.max(l + 1);
        }
        if self.last_usable + 1 > cur { push(cur, self.last_usable + 1, &mut out); }
        out
    }

    /// Shrink entry `i` so that it ends at `new_last` (inclusive). Growing is refused.
    pub fn shrink(&mut self, i: usize, new_last: u64) -> Result<(), String> {
        let e = self.entry(i).ok_or("empty slot")?;
        if new_last >= e.last { return Err(format!("shrink only: new end {new_last} >= current {}", e.last)); }
        if new_last < e.first { return Err("new end before partition start".into()); }
        self.raw_mut(i)[40..48].copy_from_slice(&new_last.to_le_bytes());
        Ok(())
    }

    /// Replace the attribute bits of entry `i`.
    pub fn set_attrs(&mut self, i: usize, attrs: u64) -> Result<(), String> {
        self.entry(i).ok_or("empty slot")?;
        self.raw_mut(i)[48..56].copy_from_slice(&attrs.to_le_bytes());
        Ok(())
    }

    /// Add a partition in the first empty slot. `first` must be 1 MiB aligned
    /// and the range must lie in usable space and overlap nothing. Returns the slot.
    pub fn add(&mut self, ptype: [u8; 16], guid: [u8; 16], first: u64, last: u64, name: &str, attrs: u64) -> Result<usize, String> {
        if first % self.align_sectors() != 0 { return Err("start not 1 MiB aligned".into()); }
        if first < self.first_usable || last > self.last_usable || last < first { return Err("range outside usable area".into()); }
        for (_, e) in self.used() {
            if first <= e.last && last >= e.first { return Err(format!("overlaps existing partition {}..{}", e.first, e.last)); }
        }
        let slot = (0..self.nent).find(|&i| self.raw(i)[0..16] == [0u8; 16]).ok_or("no free GPT slot")?;
        let r = self.raw_mut(slot);
        r.fill(0);
        r[0..16].copy_from_slice(&ptype);
        r[16..32].copy_from_slice(&guid);
        r[32..40].copy_from_slice(&first.to_le_bytes());
        r[40..48].copy_from_slice(&last.to_le_bytes());
        r[48..56].copy_from_slice(&attrs.to_le_bytes());
        for (j, u) in name.encode_utf16().take(36).enumerate() {
            r[56 + j * 2..58 + j * 2].copy_from_slice(&u.to_le_bytes());
        }
        Ok(slot)
    }

    /// Write the backup (entries, then header) before the primary, then re-read
    /// and verify both copies. LBA 0 is never touched.
    pub fn write<D: Read + Write + Seek>(&self, d: &mut D) -> Result<(), String> {
        let ss = self.ss;
        let ecrc = crc32(&self.entries);
        let mut pri = self.pri.clone();
        let mut bak = self.bak.clone();
        set_header_crcs(&mut pri, ecrc);
        set_header_crcs(&mut bak, ecrc);
        let pad = |e: &[u8]| { let mut v = e.to_vec(); v.resize((e.len() as u64 + ss - 1) as usize / ss as usize * ss as usize, 0); v };
        let ents = pad(&self.entries);
        write_at(d, le64(&bak, 72) * ss, &ents)?;
        write_at(d, (self.total - 1) * ss, &bak)?;
        d.flush().map_err(|e| e.to_string())?;
        write_at(d, le64(&pri, 72) * ss, &ents)?;
        write_at(d, ss, &pri)?;
        d.flush().map_err(|e| e.to_string())?;
        // Verify: both copies parse, CRCs hold, and the arrays equal ours.
        for (hdr_lba, name) in [(1u64, "primary"), (self.total - 1, "backup")] {
            let h = read_at(d, hdr_lba * ss, ss as usize)?;
            let (elba, n, es, c) = check_header(&h, hdr_lba).map_err(|e| format!("verify {name}: {e}"))?;
            let e = read_at(d, elba * ss, n * es)?;
            if crc32(&e) != c || e != self.entries { return Err(format!("verify {name}: entry array mismatch")); }
        }
        Ok(())
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
        let p = PartSpec { ptype: LINUX_TYPE, guid: g, first: 2048, last: 4095, name: "x", attrs: 0 };
        let t = build(1 << 20, &g, &[p]);
        assert_eq!(&t.primary_hdr[0..8], b"EFI PART");
        assert_eq!(&t.mbr[510..512], &[0x55, 0xAA]);
        let mut h = t.primary_hdr;
        let c = u32::from_le_bytes(h[16..20].try_into().unwrap());
        h[16..20].fill(0);
        assert_eq!(crc32(&h[0..92]), c);
    }

    #[derive(Clone)]
    struct Sparse { len: u64, pos: u64, ch: std::collections::HashMap<u64, Vec<u8>> }
    const CH: u64 = 65536;
    impl Sparse {
        fn new(len: u64) -> Self { Sparse { len, pos: 0, ch: Default::default() } }
        fn get(&self, off: u64, n: usize) -> Vec<u8> {
            let mut v = vec![0u8; n];
            for (i, b) in v.iter_mut().enumerate() { let o = off + i as u64; if let Some(c) = self.ch.get(&(o / CH)) { *b = c[(o % CH) as usize]; } }
            v
        }
        fn put(&mut self, off: u64, d: &[u8]) {
            for (i, b) in d.iter().enumerate() { let o = off + i as u64; self.ch.entry(o / CH).or_insert_with(|| vec![0; CH as usize])[(o % CH) as usize] = *b; }
        }
    }
    impl Read for Sparse {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let n = b.len().min((self.len - self.pos) as usize);
            b[..n].copy_from_slice(&self.get(self.pos, n)); self.pos += n as u64; Ok(n)
        }
    }
    impl Write for Sparse {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.put(self.pos, b); self.pos += b.len() as u64; Ok(b.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    impl Seek for Sparse {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> { if let SeekFrom::Start(o) = p { self.pos = o; } Ok(self.pos) }
    }

    fn laptop_image(ss: u64, total: u64, parts: &[(Vec<u8>, u64, u64)]) -> Sparse {
        // Build a GPT via the editor's own wire format with ss-sized sectors.
        let mut img = Sparse::new(total * ss);
        let mut ents = vec![0u8; NENTRIES * ESIZE];
        for (i, (name, f, l)) in parts.iter().enumerate() {
            let e = &mut ents[i * ESIZE..(i + 1) * ESIZE];
            e[0..16].copy_from_slice(&[(i as u8) + 1; 16]);
            e[16..32].copy_from_slice(&[0xA0 + i as u8; 16]);
            e[32..40].copy_from_slice(&f.to_le_bytes());
            e[40..48].copy_from_slice(&l.to_le_bytes());
            e[48..56].copy_from_slice(&(0x1234_5678_9ABC_0000u64 + i as u64).to_le_bytes());
            for (j, u) in std::str::from_utf8(name).unwrap().encode_utf16().enumerate() {
                e[56 + j * 2..58 + j * 2].copy_from_slice(&u.to_le_bytes());
            }
        }
        let ecrc = crc32(&ents);
        let esec = (ents.len() as u64 + ss - 1) / ss;
        let hdr = |my: u64, other: u64, el: u64| {
            let mut h = vec![0u8; ss as usize];
            h[0..8].copy_from_slice(b"EFI PART");
            h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            h[12..16].copy_from_slice(&92u32.to_le_bytes());
            h[24..32].copy_from_slice(&my.to_le_bytes());
            h[32..40].copy_from_slice(&other.to_le_bytes());
            h[40..48].copy_from_slice(&(2 + esec).to_le_bytes());
            h[48..56].copy_from_slice(&(total - 2 - esec).to_le_bytes());
            h[56..72].copy_from_slice(&[0x77; 16]);
            h[72..80].copy_from_slice(&el.to_le_bytes());
            h[80..84].copy_from_slice(&(NENTRIES as u32).to_le_bytes());
            h[84..88].copy_from_slice(&(ESIZE as u32).to_le_bytes());
            h[88..92].copy_from_slice(&ecrc.to_le_bytes());
            let c = crc32(&h[0..92]);
            h[16..20].copy_from_slice(&c.to_le_bytes());
            h
        };
        img.put(510, &[0x55, 0xAA]);
        img.put(ss, &hdr(1, total - 1, 2));
        img.put(2 * ss, &ents);
        let bl = total - 1 - esec;
        img.put(bl * ss, &ents);
        img.put((total - 1) * ss, &hdr(total - 1, 1, bl));
        img
    }

    fn edit_roundtrip(ss: u64) {
        // Laptop geometry in 512-byte sectors; scale LBAs for 4Kn (same byte layout).
        let k = ss / 512;
        let sc = |v: u64| if k == 1 { v } else { (v + k - 1) / k };
        let sl = |v: u64| if k == 1 { v } else { (v + 1) / k - 1 };
        let total = if k == 1 { 500_118_192 } else { 500_118_192 / 8 };
        let parts = vec![
            (b"ESP".to_vec(), sc(4096), sl(2_101_247)),
            (b"ext4".to_vec(), sc(2_101_248), sl(464_420_724)),
            (b"swap".to_vec(), sc(464_420_725), sl(500_103_449)),
        ];
        let parts: Vec<_> = parts.into_iter().map(|(n, f, l)| (n, f, l.min(total - 40))).collect();
        let orig = laptop_image(ss, total, &parts);
        let mut cur = orig.clone();
        let mut g = Gpt::read(&mut cur, ss, total).unwrap();
        assert!(!g.from_backup);
        let al = ALIGN_BYTES / ss;
        // Shrink p2 so that the rest of the disk is free except p3 at the end.
        let new_end = (parts[1].1 / al + 40) * al * 1 + 100 * al - 1; // 100 MiB past an aligned start
        let p2_last = parts[1].2;
        let new_end = new_end.min(p2_last - 1);
        g.shrink(1, new_end).unwrap();
        assert!(g.shrink(1, p2_last).is_err());
        let gaps = g.gaps();
        let (gf, gl) = *gaps.iter().find(|(f, l)| *f > new_end && *l < parts[2].1).expect("gap");
        assert_eq!(gf % al, 0);
        let mut at = gf;
        let sizes = [512u64, 4096, 4096, 64];
        let types = [LBOOT_TYPE, LROOT_TYPE, LHOME_TYPE, LLOG_TYPE];
        for (n, (sz, t)) in sizes.iter().zip(types).enumerate() {
            let len = sz * al;
            assert!(at + len - 1 <= gl);
            let slot = g.add(t, [0x50 + n as u8; 16], at, at + len - 1, &format!("new{n}"), ATTR_NO_AUTO).unwrap();
            assert_eq!(slot, 3 + n);
            at += len;
        }
        assert!(g.add(LROOT_TYPE, [1; 16], gf, gf + al - 1, "dup", 0).is_err());
        assert!(g.add(LROOT_TYPE, [1; 16], gf + 1, gf + al, "unaligned", 0).is_err());
        g.write(&mut cur).unwrap();
        let out = cur;
        // LBA 0 untouched.
        assert_eq!(out.get(0, ss as usize), orig.get(0, ss as usize));
        let g2 = Gpt::read(&mut out.clone(), ss, total).unwrap();
        let (a, b) = (Gpt::read(&mut orig.clone(), ss, total).unwrap(), &g2);
        for i in [0usize, 2] {
            assert_eq!(a.raw(i), b.raw(i), "entry {i} bytes changed");
        }
        // Entry 2: only the end LBA differs.
        let (ra, rb) = (a.raw(1), b.raw(1));
        assert_eq!(ra[..40], rb[..40]);
        assert_eq!(ra[48..], rb[48..]);
        assert_eq!(le64(rb, 40), new_end);
        for i in 3..7 {
            let e = g2.entry(i).unwrap();
            assert_eq!(e.attrs, ATTR_NO_AUTO);
        }
        assert_eq!(g2.entry(3).unwrap().ptype, LBOOT_TYPE);
        assert_eq!(a.disk_guid, g2.disk_guid);
        // Backup copy valid too: corrupt primary header, must fall back.
        let mut broken = out.clone();
        broken.put(ss + 20, &[0xFF]);
        let g3 = Gpt::read(&mut broken, ss, total).unwrap();
        assert!(g3.from_backup);
        assert_eq!(g3.raw(4), g2.raw(4));
        if let Ok(p) = std::env::var("GPT_DUMP_DIR") {
            let mut f = std::fs::File::create(format!("{p}/gpt-{ss}.img")).unwrap();
            f.set_len(total * ss).unwrap();
            for (k, c) in &out.ch { f.seek(SeekFrom::Start(k * CH)).unwrap(); let n = (CH.min(total * ss - k * CH)) as usize; f.write_all(&c[..n]).unwrap(); }
        }
    }

    #[test]
    fn edit_512() { edit_roundtrip(512); }

    #[test]
    fn edit_4kn() { edit_roundtrip(4096); }

    #[test]
    fn gaps_basic() {
        let total = 1 << 20; // 512 MiB at 512
        let img = laptop_image(512, total, &[(b"a".to_vec(), 2048, 4095)]);
        let g = Gpt::read(&mut img.clone(), 512, total).unwrap();
        let gp = g.gaps();
        assert_eq!(gp.len(), 1);
        assert_eq!(gp[0].0, 4096);
        assert_eq!((gp[0].1 + 1) % 2048, 0);
    }

    #[test]
    fn new_types_distinct() {
        let all = [LROOT_TYPE, LHOME_TYPE, LLOG_TYPE, LBOOT_TYPE, LINUX_TYPE, HOME_TYPE, LOG_TYPE];
        for i in 0..all.len() { for j in i + 1..all.len() { assert_ne!(all[i], all[j]); } }
        assert_eq!(guid_text(&LBOOT_TYPE), "0e02ed9a-feee-4cab-a22f-c861ad7de307");
        assert_eq!(guid_text(&LROOT_TYPE), "3f2d991b-d385-4fff-9cf1-a0db03062de5");
        assert_eq!(guid_text(&LHOME_TYPE), "f936dee1-99d9-4bbc-8d2a-b7447db9f650");
        assert_eq!(guid_text(&LLOG_TYPE), "3ec8fa02-816a-4d04-8718-64c3771c5187");
    }

    #[test]
    fn build_sets_no_auto() {
        let p = PartSpec { ptype: LROOT_TYPE, guid: [7; 16], first: 2048, last: 4095, name: "r", attrs: ATTR_NO_AUTO };
        let t = build(1 << 20, &[1; 16], &[p]);
        assert_eq!(le64(&t.entries, 48), ATTR_NO_AUTO);
    }
}
