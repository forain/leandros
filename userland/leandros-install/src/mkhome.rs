//! Writer for a fresh, seeded `leandros-home` f2fs volume. A port of the
//! `head` mode of scripts/mkfs-f2fs-home.py plus the "lx" node bits of
//! scripts/f2fslx.py; the two must stay byte-identical (same uuid and time).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const LABEL: &str = "leandros-home";
const BLOCK: usize = 4096;
const BPS: u64 = 512;
const F2FS_MAGIC: u32 = 0xF2F5_2010;
const CP_UMOUNT_FLAG: u32 = 1;
const NAT_ENTRY_SIZE: usize = 9;
const SIT_ENTRY_SIZE: usize = 74;
const SIT_PER_BLK: u64 = (BLOCK / SIT_ENTRY_SIZE) as u64;
const MAX_ACTIVE_LOGS: usize = 8;
const DT_DIR: u8 = 4;
const SB_LABEL_OFF: usize = 124;
const MARKER: &[u8] = b"LeandrOS lxnode2";
const SB_VERSION_OFF: usize = 1668;
const SB_INIT_VERSION_OFF: usize = 1924;

const I_BLOCKS: usize = 24;
const I_ATIME: usize = 32;
const I_CTIME: usize = 40;
const I_MTIME: usize = 48;
const I_CURRENT_DEPTH: usize = 72;
const I_PINO: usize = 84;
const I_NAMELEN: usize = 88;
const I_NAME: usize = 92;
const I_ADDR: usize = 360;
const FOOTER: usize = 4072;
const OFFSET_BIT_SHIFT: u32 = 3;

type Blk = Vec<u8>;

/// (path relative to the volume root, mode, uid, gid). Parents first.
const SKELETON: [(&str, u16, u32, u32); 5] = [
    ("leandro", 0o040700, 1000, 1000),
    ("cosmic-greeter", 0o040700, 990, 990),
    ("cosmic-greeter/.config", 0o040700, 990, 990),
    ("cosmic-greeter/.cache", 0o040700, 990, 990),
    ("cosmic-greeter/.local", 0o040700, 990, 990),
];

fn w16(b: &mut [u8], o: usize, v: u32) { b[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes()); }
fn w32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
fn w64(b: &mut [u8], o: usize, v: u64) { b[o..o + 8].copy_from_slice(&v.to_le_bytes()); }

/// Parse "64M", "20G", "1T", "4096" (powers of 1024, optional trailing B/iB).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let mut s = s.trim().to_ascii_uppercase();
    if s.ends_with('B') { s.pop(); }
    if s.ends_with('I') { s.pop(); }
    let (num, mult) = match s.chars().last() {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        Some('T') => (&s[..s.len() - 1], 1 << 40),
        _ => (&s[..], 1),
    };
    if let Ok(n) = num.parse::<u64>() {
        return Ok(n * mult);
    }
    num.parse::<f64>().map(|f| (f * mult as f64) as u64).map_err(|_| format!("bad size {s:?}"))
}

pub fn parse_uuid(s: &str) -> Result<[u8; 16], String> {
    let h: String = s.chars().filter(|c| *c != '-').collect();
    if h.len() != 32 { return Err(format!("bad uuid {s:?}")); }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).map_err(|_| format!("bad uuid {s:?}"))?;
    }
    Ok(out)
}

/// A random version-4 UUID (from /dev/urandom).
pub fn random_uuid() -> io::Result<[u8; 16]> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(b)
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---- f2fs helpers (scripts/f2fslx.py) ----

fn f2fs_crc32(data: &[u8]) -> u32 {
    // Raw crc32_le seeded with the magic, no inversion.
    let mut crc = F2FS_MAGIC;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    crc
}

fn seal_cp(b: &mut [u8]) {
    w32(b, 164, 4092);
    let c = f2fs_crc32(&b[..4092]);
    w32(b, 4092, c);
}

fn write_footer(b: &mut [u8], nid: u32, ino: u32, ofs: u32, cp_ver: u64) {
    w32(b, FOOTER, nid);
    w32(b, FOOTER + 4, ino);
    w32(b, FOOTER + 8, ofs << OFFSET_BIT_SHIFT);
    w64(b, FOOTER + 12, cp_ver);
    w32(b, FOOTER + 20, 0);
}

fn summary_block(entries: &[(u32, u8, u16)], node: bool) -> Blk {
    let mut b = vec![0u8; BLOCK];
    for (i, &(nid, ver, ofs)) in entries.iter().enumerate() {
        b[i * 7..i * 7 + 4].copy_from_slice(&nid.to_le_bytes());
        b[i * 7 + 4] = ver;
        b[i * 7 + 5..i * 7 + 7].copy_from_slice(&ofs.to_le_bytes());
    }
    b[4091] = node as u8;
    b
}

fn sit_entry(vblocks: u32, typ: u32, nbits: u32) -> [u8; SIT_ENTRY_SIZE] {
    let mut e = [0u8; SIT_ENTRY_SIZE];
    e[0..2].copy_from_slice(&((vblocks | (typ << 10)) as u16).to_le_bytes());
    for k in 0..nbits as usize {
        e[2 + k / 8] |= 0x80 >> (k % 8);
    }
    e
}

fn ssa_segs_for(total_segs: u64, fixed_meta_segs: u64) -> u64 {
    let mut ssa = 1;
    while ssa * BPS < total_segs - fixed_meta_segs - ssa {
        ssa += 1;
    }
    ssa
}

fn tea(buf: &mut [u32; 4], inp: [u32; 4]) {
    let (mut s, mut b0, mut b1) = (0u32, buf[0], buf[1]);
    let [a, b, c, d] = inp;
    for _ in 0..16 {
        s = s.wrapping_add(0x9E37_79B9);
        b0 = b0.wrapping_add((b1.wrapping_shl(4).wrapping_add(a)) ^ b1.wrapping_add(s) ^ (b1 >> 5).wrapping_add(b));
        b1 = b1.wrapping_add((b0.wrapping_shl(4).wrapping_add(c)) ^ b0.wrapping_add(s) ^ (b0 >> 5).wrapping_add(d));
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

fn str2hashbuf(msg: &[u8], length: usize, num: i32) -> [u32; 4] {
    let mut pad = (length as u32) | ((length as u32) << 8);
    pad |= pad << 16;
    let mut out: Vec<u32> = Vec::new();
    let mut val = pad;
    let mut num = num;
    let length = length.min(num as usize * 4);
    for i in 0..length {
        if i % 4 == 0 { val = pad; }
        val = (msg[i] as u32).wrapping_add(val << 8);
        if i % 4 == 3 {
            out.push(val);
            val = pad;
            num -= 1;
        }
    }
    num -= 1;
    if num >= 0 { out.push(val); }
    while num > 0 {
        out.push(pad);
        num -= 1;
    }
    [out[0], out[1], out[2], out[3]]
}

fn dentry_hash(name: &[u8]) -> u32 {
    if name == b"." || name == b".." { return 0; }
    let mut buf = [0x6745_2301u32, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476];
    let (mut p, mut n) = (0usize, name.len());
    loop {
        tea(&mut buf, str2hashbuf(&name[p..], n, 4));
        p += 16;
        if n <= 16 { break; }
        n -= 16;
    }
    buf[0]
}

const NR_DENTRY_IN_BLOCK: usize = 214;
const DENTRY_ENTRIES: usize = 30;
const DENTRY_NAMES: usize = 30 + NR_DENTRY_IN_BLOCK * 11;
const MAX_DIR_HASH_DEPTH: u32 = 63;

fn bucket_blocks(level: u32) -> u64 { if level < MAX_DIR_HASH_DEPTH / 2 { 2 } else { 4 } }
fn dir_buckets(level: u32) -> u64 {
    if level < MAX_DIR_HASH_DEPTH / 2 { 1 << level } else { 1 << (MAX_DIR_HASH_DEPTH / 2 - 1) }
}
fn dir_block_index(level: u32, bucket: u64) -> u64 {
    (0..level).map(|i| dir_buckets(i) * bucket_blocks(i)).sum::<u64>() + bucket * bucket_blocks(level)
}

/// Lay out (name, ino, DT_* type) entries like Linux's f2fs_add_regular_entry.
fn place_dentries(entries: &[(Vec<u8>, u32, u8)]) -> (BTreeMap<u64, Blk>, u32) {
    let mut blocks: BTreeMap<u64, Blk> = BTreeMap::new();
    let mut used: BTreeMap<u64, usize> = BTreeMap::new();
    let mut depth = 1;
    for (name, ino, ftype) in entries {
        let slots = (name.len() + 7) / 8;
        assert!(slots > 0 && slots <= NR_DENTRY_IN_BLOCK, "bad directory entry name");
        let h = dentry_hash(name);
        let mut done = false;
        for level in 0..=8u32 {
            let first = dir_block_index(level, h as u64 % dir_buckets(level));
            for bi in first..first + bucket_blocks(level) {
                let at = *used.get(&bi).unwrap_or(&0);
                if at + slots > NR_DENTRY_IN_BLOCK { continue; }
                let d = blocks.entry(bi).or_insert_with(|| vec![0u8; BLOCK]);
                for k in at..at + slots { d[k / 8] |= 1 << (k % 8); }
                let e = DENTRY_ENTRIES + at * 11;
                w32(d, e, h);
                w32(d, e + 4, *ino);
                w16(d, e + 8, name.len() as u32);
                d[e + 10] = match ftype { 4 => 2, _ => 0 }; // only DT_DIR is used here
                d[DENTRY_NAMES + at * 8..DENTRY_NAMES + at * 8 + name.len()].copy_from_slice(name);
                used.insert(bi, at + slots);
                depth = depth.max(level + 1);
                done = true;
                break;
            }
            if done { break; }
        }
        assert!(done, "directory too large");
    }
    if blocks.is_empty() { blocks.insert(0, vec![0u8; BLOCK]); }
    (blocks, depth)
}

/// Every non-zero block of a fresh volume plus its length in blocks.
pub fn build(size: u64, uuid: &[u8; 16], time: u64) -> Result<(BTreeMap<u64, Blk>, u64), String> {
    let total_segs = size / (BLOCK as u64 * BPS);
    if total_segs < 32 {
        return Err(format!("{size} bytes is too small for a home volume (64 MiB minimum)"));
    }
    let sit_blocks = (total_segs + SIT_PER_BLK - 1) / SIT_PER_BLK;
    let sit_segs = 2 * ((sit_blocks + BPS - 1) / BPS);
    let (ckpt_segs, nat_segs) = (2u64, 2u64);
    let ssa_segs = ssa_segs_for(total_segs, 1 + ckpt_segs + sit_segs + nat_segs);
    let seg0 = BPS;
    let cp_blk = seg0;
    let sit_blk = cp_blk + ckpt_segs * BPS;
    let nat_blk = sit_blk + sit_segs * BPS;
    let ssa_blk = nat_blk + nat_segs * BPS;
    let main_blk = ssa_blk + ssa_segs * BPS;
    let meta_segs = 1 + ckpt_segs + sit_segs + nat_segs + ssa_segs;
    let main_segs = total_segs - meta_segs;
    let total_blocks = total_segs * BPS;

    let mut blocks: BTreeMap<u64, Blk> = BTreeMap::new();
    fn blk(m: &mut BTreeMap<u64, Blk>, n: u64) -> &mut Blk {
        m.entry(n).or_insert_with(|| vec![0u8; BLOCK])
    }

    // Superblock, both copies.
    let label: Vec<u8> = LABEL.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    for base in 0..2u64 {
        let b = blk(&mut blocks, base);
        let s = 1024;
        w32(b, s, F2FS_MAGIC);
        w16(b, s + 4, 1);
        w16(b, s + 6, 12);
        w32(b, s + 8, 9);
        w32(b, s + 12, 3);
        w32(b, s + 16, 12);
        w32(b, s + 20, 9);
        w32(b, s + 24, 1);
        w32(b, s + 28, 1);
        w64(b, s + 36, total_blocks);
        w32(b, s + 44, main_segs as u32);
        w32(b, s + 48, (total_segs - 1) as u32);
        w32(b, s + 52, ckpt_segs as u32);
        w32(b, s + 56, sit_segs as u32);
        w32(b, s + 60, nat_segs as u32);
        w32(b, s + 64, ssa_segs as u32);
        w32(b, s + 68, main_segs as u32);
        w32(b, s + 72, seg0 as u32);
        w32(b, s + 76, cp_blk as u32);
        w32(b, s + 80, sit_blk as u32);
        w32(b, s + 84, nat_blk as u32);
        w32(b, s + 88, ssa_blk as u32);
        w32(b, s + 92, main_blk as u32);
        w32(b, s + 96, 3);
        w32(b, s + 100, 1);
        w32(b, s + 104, 2);
        b[s + 108..s + 124].copy_from_slice(uuid);
        b[s + SB_LABEL_OFF..s + SB_LABEL_OFF + label.len()].copy_from_slice(&label);
        for o in [SB_VERSION_OFF, SB_INIT_VERSION_OFF] {
            let f = &mut b[s + o..s + o + 256];
            f.fill(0);
            f[..MARKER.len()].copy_from_slice(MARKER);
        }
    }

    // Directories: ino 3 is the root, the skeleton follows from ino 4.
    let mut dirs: Vec<(&str, u16, u32, u32)> = vec![("", 0o040755, 0, 0)];
    dirs.extend(SKELETON.iter().copied());
    let dirname = |p: &str| -> String { p.rfind('/').map(|i| p[..i].to_string()).unwrap_or_default() };
    let basename = |p: &str| -> String { p.rsplit('/').next().unwrap().to_string() };
    let ino_of = |p: &str| -> u32 { 3 + dirs.iter().position(|d| d.0 == p).unwrap() as u32 };
    let n = dirs.len() as u64;

    for (i, &(p, mode, uid, gid)) in dirs.iter().enumerate() {
        let ino = 3 + i as u32;
        let inode_at = main_blk + i as u64;
        let data_at = main_blk + BPS + i as u64;
        let parent = if p.is_empty() { ino } else { ino_of(&dirname(p)) };
        let kids: Vec<&str> = dirs[1..].iter().map(|d| d.0).filter(|c| dirname(c) == p).collect();
        let mut ents: Vec<(Vec<u8>, u32, u8)> = vec![(b".".to_vec(), ino, DT_DIR), (b"..".to_vec(), parent, DT_DIR)];
        for c in &kids {
            ents.push((basename(c).into_bytes(), ino_of(c), DT_DIR));
        }
        let (dblocks, depth) = place_dentries(&ents);
        assert!(dblocks.len() == 1 && dblocks.contains_key(&0) && depth == 1);
        blocks.insert(data_at, dblocks.into_values().next().unwrap());

        let node = blk(&mut blocks, inode_at);
        w16(node, 0, mode as u32);
        w32(node, 4, uid);
        w32(node, 8, gid);
        w32(node, 12, 2 + kids.len() as u32);
        w64(node, 16, BLOCK as u64);
        w64(node, I_BLOCKS, 2);
        w32(node, I_CURRENT_DEPTH, 1);
        w32(node, I_PINO, parent);
        let nm = basename(p).into_bytes();
        w32(node, I_NAMELEN, nm.len() as u32);
        node[I_NAME..I_NAME + nm.len()].copy_from_slice(&nm);
        w32(node, I_ADDR, data_at as u32);
        for off in [I_ATIME, I_CTIME, I_MTIME] {
            w64(node, off, time);
        }
        write_footer(node, ino, ino, 0, 1);

        let per = (BLOCK / NAT_ENTRY_SIZE) as u32;
        let nb = blk(&mut blocks, nat_blk + (ino / per) as u64);
        let o = (ino % per) as usize * NAT_ENTRY_SIZE;
        nb[o] = 1;
        w32(nb, o + 1, ino);
        w32(nb, o + 5, inode_at as u32);
    }

    // NAT entries for the node and meta inodes (blkaddr 1).
    let per = (BLOCK / NAT_ENTRY_SIZE) as u32;
    for dino in [1u32, 2] {
        let nb = blk(&mut blocks, nat_blk + (dino / per) as u64);
        let o = (dino % per) as usize * NAT_ENTRY_SIZE;
        nb[o] = 0;
        w32(nb, o + 1, dino);
        w32(nb, o + 5, 1);
    }

    // Six typed current segments.
    let log_seg: [u32; 6] = [1, 2, 3, 0, 4, 5];
    let log_off = |t: usize| -> u32 { if t == 0 || t == 3 { n as u32 } else { 0 } };
    let used = 2 * n;
    let sb_ = blk(&mut blocks, sit_blk);
    for (t, &seg) in log_seg.iter().enumerate() {
        let used_here = if seg == 0 || seg == 1 { n as u32 } else { 0 };
        let e = sit_entry(used_here, t as u32, used_here);
        sb_[seg as usize * SIT_ENTRY_SIZE..(seg as usize + 1) * SIT_ENTRY_SIZE].copy_from_slice(&e);
    }

    // SSA.
    let owners: Vec<(u32, u8, u16)> = dirs.iter().map(|d| (ino_of(d.0), 1, 0)).collect();
    for (t, &seg) in log_seg.iter().enumerate() {
        let ents: &[(u32, u8, u16)] = if seg == 0 || seg == 1 { &owners } else { &[] };
        blocks.insert(ssa_blk + seg as u64, summary_block(ents, t >= 3));
    }

    // Checkpoint packs.
    let meta = ckpt_segs + sit_segs + nat_segs + ssa_segs;
    let rsvd = 9u64.saturating_sub(meta).max(1);
    let ovp = rsvd;
    let pack = (ckpt_segs / 2) * BPS;
    let free_segs = main_segs - 6;
    let checkpoint = |blocks: &mut BTreeMap<u64, Blk>, at: u64, ver: u64| {
        let mut c = vec![0u8; BLOCK];
        w64(&mut c, 0, ver);
        w64(&mut c, 8, (main_segs - ovp) * BPS);
        w64(&mut c, 16, used);
        w32(&mut c, 24, rsvd as u32);
        w32(&mut c, 28, ovp as u32);
        w32(&mut c, 32, free_segs as u32);
        for t in 0..6usize {
            let seg = log_seg[t];
            if t < 3 {
                w32(&mut c, 84 + t * 4, seg);
                w16(&mut c, 116 + t * 2, log_off(t));
            } else {
                w32(&mut c, 36 + (t - 3) * 4, seg);
                w16(&mut c, 68 + (t - 3) * 2, log_off(t));
            }
        }
        for i in 6..MAX_ACTIVE_LOGS {
            w32(&mut c, 36 + i * 4, 0xFFFF_FFFF);
            w32(&mut c, 84 + i * 4, 0xFFFF_FFFF);
        }
        w32(&mut c, 132, CP_UMOUNT_FLAG);
        w32(&mut c, 136, pack as u32);
        w32(&mut c, 140, (pack - 7) as u32);
        w32(&mut c, 144, n as u32);
        w32(&mut c, 148, n as u32);
        w32(&mut c, 152, 3 + n as u32);
        w32(&mut c, 156, ((sit_segs / 2) * BPS / 8) as u32);
        w32(&mut c, 160, ((nat_segs / 2) * BPS / 8) as u32);
        for t in 0..16 { c[176 + t] = 1; }
        w64(&mut c, 2048, ver);
        w32(&mut c, 3348, log_seg[0]);
        w32(&mut c, 3352, log_seg[3]);
        w16(&mut c, 3356, n as u32);
        w16(&mut c, 3358, n as u32);
        seal_cp(&mut c);
        for t in 0..6usize {
            let ents: &[(u32, u8, u16)] = if t == 0 || t == 3 { &owners } else { &[] };
            blocks.insert(at + pack - 7 + t as u64, summary_block(ents, t >= 3));
        }
        blocks.insert(at + pack - 1, c.clone());
        blocks.insert(at, c);
    };
    checkpoint(&mut blocks, cp_blk, 1);
    checkpoint(&mut blocks, cp_blk + pack, 0);
    Ok((blocks, total_blocks))
}

/// Write a fresh volume of `size` bytes to `path` (a file, or a block device
/// at the partition start). With `head_only` only the head is written and a
/// regular file is truncated just past it (the rest of a fresh volume is
/// zeros); otherwise a regular file is extended to the full size. On a block
/// device the caller must supply an already-zero (or don't-care) remainder.
/// First block of the main area (everything below it is metadata: superblocks,
/// checkpoint, SIT, NAT, SSA). Mirrors the layout math in `build`.
pub fn main_blkaddr(size: u64) -> u64 {
    let total_segs = size / (BLOCK as u64 * BPS);
    let sit_blocks = (total_segs + SIT_PER_BLK - 1) / SIT_PER_BLK;
    let sit_segs = 2 * ((sit_blocks + BPS - 1) / BPS);
    let (ckpt_segs, nat_segs) = (2u64, 2u64);
    let ssa_segs = ssa_segs_for(total_segs, 1 + ckpt_segs + sit_segs + nat_segs);
    (1 + ckpt_segs + sit_segs + nat_segs + ssa_segs) * BPS
}

/// Explicitly write zeros over every metadata block `[0, main_blkaddr)` so
/// stale bytes in the target (e.g. a freed ext4 tail) cannot leak into the SIT,
/// NAT, SSA or the second checkpoint pack.
fn zero_meta(f: &mut std::fs::File, off: u64, size: u64) -> io::Result<()> {
    let end = main_blkaddr(size) * BLOCK as u64;
    let z = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    f.seek(SeekFrom::Start(off))?;
    while pos < end {
        let n = (z.len() as u64).min(end - pos) as usize;
        f.write_all(&z[..n])?;
        pos += n as u64;
    }
    Ok(())
}

pub fn write_home(path: &Path, size: u64, uuid: &[u8; 16], time: u64, head_only: bool) -> io::Result<u64> {
    let (blocks, total_blocks) = build(size, uuid, time)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut f = OpenOptions::new().write(true).create(true).open(path)?;
    let is_file = f.metadata()?.is_file();
    if is_file { f.set_len(0)?; }
    if !is_file || !head_only { zero_meta(&mut f, 0, size)?; }
    for (n, b) in &blocks {
        f.seek(SeekFrom::Start(n * BLOCK as u64))?;
        f.write_all(b)?;
    }
    if is_file {
        let end = if head_only { (blocks.keys().max().unwrap() + 1) * BLOCK as u64 } else { total_blocks * BLOCK as u64 };
        f.set_len(end)?;
    }
    f.sync_all().ok();
    Ok(total_blocks * BLOCK as u64)
}

/// Write the head of a fresh volume of `size` bytes at byte offset `off` of an
/// open block device (or file). Metadata areas are zeroed explicitly; the main area is left alone: the
/// f2fs server zeroes the blocks it allocates. Returns the bytes written.
pub fn write_home_at(f: &mut std::fs::File, off: u64, size: u64, uuid: &[u8; 16], time: u64) -> io::Result<u64> {
    let (blocks, _total) = build(size, uuid, time).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    zero_meta(f, off, size)?;
    let mut n = 0;
    for (b, data) in &blocks {
        f.seek(SeekFrom::Start(off + b * BLOCK as u64))?;
        f.write_all(data)?;
        n += data.len() as u64;
    }
    Ok(n)
}

#[cfg(test)]
mod zero_tests {
    use super::*;

    #[test]
    fn write_home_at_zeroes_metadata_over_garbage() {
        let size = 128u64 << 20;
        let dir = std::env::var("GPT_DUMP_DIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into());
        let off = 3u64 << 20;
        let path = format!("{dir}/home-garbage.img");
        let mut f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        f.set_len(off + size).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&vec![0xA5u8; (off + size) as usize]).unwrap();
        write_home_at(&mut f, off, size, &[7; 16], 1_700_000_000).unwrap();
        // Reference: full write onto a fresh file.
        let rp = format!("{dir}/home-ref.img");
        write_home(Path::new(&rp), size, &[7; 16], 1_700_000_000, false).unwrap();
        let mb = (main_blkaddr(size) * BLOCK as u64) as usize;
        let mut got = vec![0u8; mb];
        f.seek(SeekFrom::Start(off)).unwrap();
        f.read_exact(&mut got).unwrap();
        let want = std::fs::read(&rp).unwrap();
        assert!(got == want[..mb], "metadata area differs from a clean build");
        // Bytes before the window and the main area beyond the head stay as they were.
        let mut pre = [0u8; 16];
        f.seek(SeekFrom::Start(off - 16)).unwrap();
        f.read_exact(&mut pre).unwrap();
        assert_eq!(pre, [0xA5; 16]);
        if std::env::var("GPT_DUMP_DIR").is_err() {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&rp);
        }
    }
}
