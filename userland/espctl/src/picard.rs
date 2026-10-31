//! picard -- Pi 5 card kernel slots via firmware tryboot (`espctl pi ...`).
//!
//! The card's boot FAT (MBR p1, `/dev/vdc`) is NOT mounted through fatfs: every
//! write is an explicit raw sector write on an allowlist of sectors (a BLKWRWIN
//! window per range), ordered so that a power cut at any sector boundary leaves
//! a bootable card (see lane note artifacts/notes/lane-pitryctl-2026-10-31.md):
//!
//!   setup   one-time. Allocates fully-free FAT sectors (128-cluster groups),
//!           writes data, then chains, then directories, then ONE root-dir
//!           sector holding the new entries `slots/`, `tryboot.txt`,
//!           `cmdline_try.txt`. Never touches config.txt/kernel.img/...
//!   stage   bulk-write kernel/initrd (zero-filled tail) into slots/sN/, verify
//!           from the medium, set the digit in tryboot.txt sector 0.
//!   try     reboot(RESTART2,"0 tryboot").
//!   commit  copy the ONE differing 512 B sector of tryboot.txt into config.txt.
//!   config.txt layout: sector 0 = `os_prefix=slots/sN/` + comments only; an
//!   `[all]` block with every essential line lives in a later sector (M2).
//!
//! Secrets: nothing here ever prints cmdline or cmdline*.txt contents.

use std::collections::HashMap;
use std::io;

pub type R<T> = io::Result<T>;
fn e<T>(m: impl Into<String>) -> R<T> {
    Err(io::Error::new(io::ErrorKind::Other, m.into()))
}
fn sha(b: &[u8]) -> String {
    crate::sha256(b)
}

pub const SS: usize = 512;
pub const NSLOTS: u8 = 3;
pub const DEF_KERNEL: u64 = 32 << 20;
pub const DEF_INITRD: u64 = 16 << 20;
pub const TRY_LINE: &str = "cmdline=cmdline_try.txt\n";
pub const DTB: &str = "bcm2712-rpi-5-b.dtb";

// ---------------------------------------------------------------- disk

pub trait Disk {
    /// Read `buf.len()` bytes (multiple of 512) at sector `lba`.
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> R<()>;
    /// Write whole sectors; a 512 B write is exactly one program cycle.
    fn write(&mut self, lba: u64, data: &[u8]) -> R<()>;
    /// Open write windows over exactly these (start, len) sector ranges.
    fn allow(&mut self, _ranges: &[(u64, u64)]) -> R<()> {
        Ok(())
    }
    /// Make the next read come from the medium, not a cache.
    fn fresh(&mut self) -> R<()> {
        Ok(())
    }
    fn flush(&mut self) -> R<()> {
        Ok(())
    }
}

fn merge(mut v: Vec<u64>) -> Vec<(u64, u64)> {
    v.sort();
    v.dedup();
    let mut out: Vec<(u64, u64)> = vec![];
    for l in v {
        match out.last_mut() {
            Some(r) if r.0 + r.1 == l => r.1 += 1,
            _ => out.push((l, 1)),
        }
    }
    out
}

// ---------------------------------------------------------------- FAT

#[derive(Clone, Debug)]
pub struct Ent {
    pub name: String,
    pub short: [u8; 11],
    pub attr: u8,
    pub first: u32,
    pub size: u32,
    pub lba: u64,
    pub idx: usize,
}
pub struct Dir {
    pub ents: Vec<Ent>,
    /// (lba, index) of the first 0x00 entry (the end marker).
    pub end: Option<(u64, usize)>,
}

fn le16(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u64
}
fn le32(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64
}
fn lfn_sum(s: &[u8; 11]) -> u8 {
    let mut c = 0u8;
    for &b in s {
        c = ((c & 1) << 7).wrapping_add(c >> 1).wrapping_add(b);
    }
    c
}

pub struct Fat<D: Disk> {
    pub d: D,
    pub spc: u64,
    rsv: u64,
    nfat: u64,
    pub fatsz: u64,
    root: u32,
    pub data0: u64,
    pub ncl: u32,
    allowed: Vec<(u64, u64)>,
    fcache: HashMap<u64, Vec<u8>>,
    pub dry: bool,
    pub nwrites: usize,
}

impl<D: Disk> Fat<D> {
    pub fn open(mut d: D) -> R<Self> {
        let mut b = vec![0u8; SS];
        d.read(0, &mut b)?;
        if b[510] != 0x55 || b[511] != 0xAA || &b[82..87] != b"FAT32" || le16(&b, 11) != 512 || le16(&b, 17) != 0 || le16(&b, 22) != 0 {
            return e("not a 512 B-sector FAT32 volume");
        }
        let spc = b[13] as u64;
        let rsv = le16(&b, 14);
        let nfat = b[16] as u64;
        let fatsz = le32(&b, 36);
        let total = le32(&b, 32);
        let data0 = rsv + nfat * fatsz;
        if spc == 0 || nfat == 0 || total <= data0 {
            return e("bad FAT32 BPB");
        }
        let ncl = ((total - data0) / spc) as u32;
        Ok(Fat { d, spc, rsv, nfat, fatsz, root: le32(&b, 44) as u32, data0, ncl, allowed: vec![], fcache: HashMap::new(), dry: false, nwrites: 0 })
    }
    pub fn cluster_bytes(&self) -> u64 {
        self.spc * 512
    }
    /// Cluster size >= 4 KiB and 4 KiB-aligned data area: needed so a bulk
    /// slot write never read-modify-writes a neighbour's sectors.
    pub fn check_align(&self) -> R<()> {
        if self.cluster_bytes() < 4096 || self.data0 % 8 != 0 {
            return e("FAT geometry not 4 KiB aligned (cluster < 4 KiB or data start not a multiple of 8 sectors)");
        }
        Ok(())
    }
    pub fn cl_lba(&self, c: u32) -> u64 {
        self.data0 + (c as u64 - 2) * self.spc
    }
    pub fn rd(&mut self, lba: u64, n: usize) -> R<Vec<u8>> {
        let mut v = vec![0u8; n * SS];
        self.d.read(lba, &mut v)?;
        Ok(v)
    }
    fn fat_sec(&mut self, i: u64) -> R<Vec<u8>> {
        if let Some(v) = self.fcache.get(&i) {
            return Ok(v.clone());
        }
        let v = self.rd(self.rsv + i, 1)?;
        self.fcache.insert(i, v.clone());
        Ok(v)
    }
    pub fn fat_get(&mut self, c: u32) -> R<u32> {
        let s = self.fat_sec(c as u64 / 128)?;
        let o = (c as usize % 128) * 4;
        Ok((le32(&s, o) as u32) & 0x0FFF_FFFF)
    }
    pub fn chain(&mut self, first: u32) -> R<Vec<u32>> {
        let mut v = vec![];
        let mut c = first;
        while c >= 2 && c < 0x0FFF_FFF0 {
            if c >= self.ncl + 2 || v.len() > self.ncl as usize {
                return e("FAT chain out of range or looping");
            }
            v.push(c);
            c = self.fat_get(c)?;
        }
        Ok(v)
    }
    pub fn dir(&mut self, first: u32) -> R<Dir> {
        let mut ents = vec![];
        let mut end = None;
        let mut lfn: Vec<(u8, Vec<u16>)> = vec![];
        let mut lsum = 0u8;
        'o: for c in self.chain(first)? {
            for s in 0..self.spc {
                let lba = self.cl_lba(c) + s;
                let b = self.rd(lba, 1)?;
                for i in 0..16 {
                    let x = &b[i * 32..i * 32 + 32];
                    if x[0] == 0 {
                        end = Some((lba, i));
                        break 'o;
                    }
                    if x[0] == 0xE5 {
                        lfn.clear();
                        continue;
                    }
                    if x[11] == 0x0F {
                        let mut u = vec![];
                        for &o in &[1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30] {
                            u.push(le16(x, o) as u16);
                        }
                        lsum = x[13];
                        lfn.push((x[0] & 0x1F, u));
                        continue;
                    }
                    if x[11] & 0x08 != 0 {
                        lfn.clear();
                        continue;
                    }
                    let mut short = [0u8; 11];
                    short.copy_from_slice(&x[..11]);
                    let name = if !lfn.is_empty() && lsum == lfn_sum(&short) {
                        lfn.sort_by_key(|p| p.0);
                        let mut u: Vec<u16> = vec![];
                        for (_, p) in &lfn {
                            u.extend(p);
                        }
                        let n = u.iter().position(|&c| c == 0 || c == 0xFFFF).unwrap_or(u.len());
                        String::from_utf16_lossy(&u[..n])
                    } else {
                        let lower = |s: &[u8], f: bool| {
                            let t: String = s.iter().take_while(|&&c| c != b' ').map(|&c| c as char).collect();
                            if f { t.to_lowercase() } else { t }
                        };
                        let b8 = lower(&short[..8], x[12] & 8 != 0);
                        let e3 = lower(&short[8..], x[12] & 16 != 0);
                        if e3.is_empty() { b8 } else { format!("{}.{}", b8, e3) }
                    };
                    lfn.clear();
                    ents.push(Ent {
                        name,
                        short,
                        attr: x[11],
                        first: ((le16(x, 20) << 16) | le16(x, 26)) as u32,
                        size: le32(x, 28) as u32,
                        lba,
                        idx: i,
                    });
                }
            }
        }
        Ok(Dir { ents, end })
    }
    pub fn lookup(&mut self, path: &str) -> R<Option<Ent>> {
        let mut cur = self.root;
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for (i, p) in parts.iter().enumerate() {
            let d = self.dir(cur)?;
            let Some(en) = d.ents.into_iter().find(|x| x.name.eq_ignore_ascii_case(p)) else { return Ok(None) };
            if i + 1 == parts.len() {
                return Ok(Some(en));
            }
            if en.attr & 0x10 == 0 {
                return Ok(None);
            }
            cur = en.first;
        }
        Ok(None)
    }
    pub fn need(&mut self, path: &str) -> R<Ent> {
        match self.lookup(path)? {
            Some(x) => Ok(x),
            None => e(format!("{} not found on the card (run `pi setup`?)", path)),
        }
    }
    pub fn file_lbas(&mut self, en: &Ent) -> R<Vec<u64>> {
        let need = (en.size as u64 + 511) / 512;
        let mut v = vec![];
        for c in self.chain(en.first)? {
            for s in 0..self.spc {
                if (v.len() as u64) < need {
                    v.push(self.cl_lba(c) + s);
                }
            }
        }
        if (v.len() as u64) < need {
            return e(format!("{}: chain shorter than size", en.name));
        }
        Ok(v)
    }
    /// Read sectors from the medium, coalescing contiguous runs.
    pub fn read_lbas(&mut self, l: &[u64]) -> R<Vec<u8>> {
        let mut out = Vec::with_capacity(l.len() * SS);
        let mut i = 0;
        while i < l.len() {
            let mut j = i + 1;
            while j < l.len() && l[j] == l[j - 1] + 1 {
                j += 1;
            }
            out.extend(self.rd(l[i], j - i)?);
            i = j;
        }
        Ok(out)
    }
    pub fn read_file(&mut self, en: &Ent) -> R<Vec<u8>> {
        let l = self.file_lbas(en)?;
        let mut v = self.read_lbas(&l)?;
        v.truncate(en.size as usize);
        Ok(v)
    }
    pub fn allow(&mut self, lbas: Vec<u64>) -> R<()> {
        let r = merge(lbas);
        if r.len() > 16 {
            return e(format!("write set needs {} windows (max 16)", r.len()));
        }
        if !self.dry {
            self.d.allow(&r)?;
        }
        self.allowed = r;
        Ok(())
    }
    pub fn w(&mut self, lba: u64, data: &[u8]) -> R<()> {
        let n = (data.len() / SS) as u64;
        if data.len() % SS != 0 || n == 0 || !self.allowed.iter().any(|r| lba >= r.0 && lba + n <= r.0 + r.1) {
            // a multi-run write may span ranges: check per sector
            for k in 0..n {
                if !self.allowed.iter().any(|r| lba + k >= r.0 && lba + k < r.0 + r.1) {
                    return e(format!("write to sector {} is outside the allowlist", lba + k));
                }
            }
        }
        self.nwrites += n as usize;
        self.fcache.clear();
        if self.dry {
            return Ok(());
        }
        self.d.write(lba, data)
    }
    pub fn fresh(&mut self) -> R<()> {
        self.fcache.clear();
        self.d.fresh()
    }
    pub fn free_clusters(&mut self) -> R<u32> {
        let mut n = 0;
        for s in 0..self.fatsz {
            let b = self.fat_sec(s)?;
            for i in 0..128usize {
                let c = s as usize * 128 + i;
                if c >= 2 && c < self.ncl as usize + 2 && le32(&b, i * 4) & 0x0FFF_FFFF == 0 {
                    n += 1;
                }
            }
        }
        Ok(n)
    }
    pub fn max_used(&mut self) -> R<u32> {
        let mut m = 1;
        for s in 0..self.fatsz {
            let b = self.fat_sec(s)?;
            for i in 0..128usize {
                let c = s as usize * 128 + i;
                if c >= 2 && c < self.ncl as usize + 2 && le32(&b, i * 4) & 0x0FFF_FFFF != 0 {
                    m = c as u32;
                }
            }
        }
        Ok(m)
    }
}

// ---------------------------------------------------------------- dir entries

const DATE: u16 = ((2026 - 1980) << 9) | (10 << 5) | 31;

fn mk_ent(short: &[u8; 11], nt: u8, attr: u8, first: u32, size: u32) -> [u8; 32] {
    let mut x = [0u8; 32];
    x[..11].copy_from_slice(short);
    x[11] = attr;
    x[12] = nt;
    for o in [14usize, 22] {
        x[o + 2..o + 4].copy_from_slice(&DATE.to_le_bytes());
    }
    x[18..20].copy_from_slice(&DATE.to_le_bytes());
    x[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
    x[26..28].copy_from_slice(&(first as u16).to_le_bytes());
    x[28..32].copy_from_slice(&size.to_le_bytes());
    x
}
fn short_of(name: &str) -> Option<([u8; 11], u8)> {
    let (b, x) = match name.rsplit_once('.') {
        Some((b, x)) => (b, x),
        None => (name, ""),
    };
    let ok = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric());
    if b.is_empty() || b.len() > 8 || x.len() > 3 || !ok(b) || !ok(x) {
        return None;
    }
    let low = name.chars().all(|c| !c.is_ascii_uppercase());
    let mut s = [b' '; 11];
    for (i, c) in b.bytes().enumerate() {
        s[i] = c.to_ascii_uppercase();
    }
    for (i, c) in x.bytes().enumerate() {
        s[8 + i] = c.to_ascii_uppercase();
    }
    Some((s, if low { 0x18 } else { 0 }))
}
fn alias(s: &str) -> [u8; 11] {
    let mut a = [b' '; 11];
    a[..s.len()].copy_from_slice(s.as_bytes());
    a
}
/// Directory entries (LFN first) for `name`; `al` is the 8.3 alias for long names.
fn mk_entries(name: &str, al: Option<[u8; 11]>, attr: u8, first: u32, size: u32) -> Vec<[u8; 32]> {
    if let Some((s, nt)) = short_of(name) {
        return vec![mk_ent(&s, nt, attr, first, size)];
    }
    let al = al.expect("long name needs an alias");
    let u: Vec<u16> = name.encode_utf16().collect();
    let n = (u.len() + 12) / 13;
    let mut out = vec![];
    for i in (0..n).rev() {
        let mut x = [0xFFu8; 32];
        x[0] = (i as u8 + 1) | if i + 1 == n { 0x40 } else { 0 };
        x[11] = 0x0F;
        x[12] = 0;
        x[13] = lfn_sum(&al);
        x[26] = 0;
        x[27] = 0;
        for (k, &o) in [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].iter().enumerate() {
            let idx = i * 13 + k;
            let v: u16 = if idx < u.len() { u[idx] } else if idx == u.len() { 0 } else { 0xFFFF };
            x[o..o + 2].copy_from_slice(&v.to_le_bytes());
        }
        out.push(x);
    }
    out.push(mk_ent(&al, 0, attr, first, size));
    out
}

// ---------------------------------------------------------------- config.txt

fn is_comment(l: &[u8]) -> bool {
    let t = String::from_utf8_lossy(l);
    let t = t.trim();
    t.is_empty() || t.starts_with('#')
}
fn key_of(l: &[u8]) -> String {
    let t = String::from_utf8_lossy(l);
    t.trim().split(|c: char| c == '=' || c.is_whitespace()).next().unwrap_or("").to_string()
}
fn pad(r: usize) -> Vec<u8> {
    match r {
        0 => vec![],
        1 => b"\n".to_vec(),
        _ => {
            let mut v = vec![b'#'];
            v.extend(std::iter::repeat(b' ').take(r - 2));
            v.push(b'\n');
            v
        }
    }
}
fn split_lines(b: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = b.split(|&c| c == b'\n').collect();
    if v.last().map_or(false, |l| l.is_empty()) {
        v.pop();
    }
    v
}
fn with_nl(l: &[u8]) -> Vec<u8> {
    let mut v = l.to_vec();
    if v.last() != Some(&b'\n') {
        v.push(b'\n');
    }
    v
}
fn head0(cfg: &[u8]) -> R<usize> {
    let a0 = cfg[..SS].iter().rposition(|&c| c == b'\n').map(|p| p + 1).unwrap_or(0);
    if a0 < 20 {
        return e("config.txt sector 0 has no room for an os_prefix line");
    }
    Ok(a0)
}
pub fn current_slot(cfg: &[u8]) -> Option<u8> {
    let h = &cfg[..SS.min(cfg.len())];
    let p = h.windows(17).position(|w| w == b"os_prefix=slots/s")?;
    let d = *h.get(p + 17)?;
    (d.is_ascii_digit() && h.get(p + 18) == Some(&b'/')).then(|| d - b'0')
}

/// Step (a) of the migration (M2): rewrite ONE later sector S (comments only) to
/// `[all]` + every non-comment line that lives in sector 0's head. Same length.
/// Returns (new config, S).
pub fn migrate_a(cfg: &[u8]) -> R<(Vec<u8>, usize)> {
    let l = cfg.len();
    if l < 2 * SS || std::str::from_utf8(cfg).is_err() {
        return e("config.txt too small or not UTF-8");
    }
    if split_lines(cfg).iter().any(|x| String::from_utf8_lossy(x).trim().starts_with('[')) {
        return e("config.txt has [section] headers; migrate by hand");
    }
    let a0 = head0(cfg)?;
    let nl = cfg[a0..].iter().position(|&c| c == b'\n').map(|p| a0 + p + 1).unwrap_or(l);
    let mut re: Vec<Vec<u8>> = vec![];
    for x in split_lines(&cfg[..a0]).into_iter().chain(split_lines(&cfg[a0..nl])) {
        if !is_comment(x) {
            re.push(with_nl(x));
        }
    }
    for r in &re {
        let k = key_of(r);
        if split_lines(cfg).iter().filter(|x| !is_comment(x) && key_of(x) == k).count() != 1 {
            return e(format!("key `{}` is not unique in config.txt; migrate by hand", k));
        }
    }
    let mut body = b"[all]\n".to_vec();
    for r in &re {
        body.extend(r);
    }
    let mut s = l / SS;
    while s > 1 {
        s -= 1;
        let (lo, hi) = (s * SS, s * SS + SS);
        let f = cfg[lo..hi].iter().position(|&c| c == b'\n').map(|p| lo + p);
        let la = cfg[lo..hi].iter().rposition(|&c| c == b'\n').map(|p| lo + p);
        let (Some(f), Some(la)) = (f, la) else { continue };
        if f >= la {
            continue;
        }
        let ps = cfg[..lo].iter().rposition(|&c| c == b'\n').map(|p| p + 1).unwrap_or(0);
        let ne = cfg[hi..].iter().position(|&c| c == b'\n').map(|p| hi + p + 1).unwrap_or(l);
        if !(is_comment(&cfg[ps..=f]) && is_comment(&cfg[la + 1..ne]) && split_lines(&cfg[f + 1..=la]).iter().all(|x| is_comment(x))) {
            continue;
        }
        let mid = la - f;
        if body.len() > mid {
            continue;
        }
        let mut a = cfg.to_vec();
        let mut nm = body.clone();
        nm.extend(pad(mid - body.len()));
        a[f + 1..=la].copy_from_slice(&nm);
        return Ok((a, s));
    }
    e("no comments-only sector can hold the [all] block; migrate by hand")
}
/// Step (b): set the os_prefix slot digit in sector 0 (creating the line if
/// absent). Refuses until step (a) put the essential lines under `[all]`.
pub fn set_prefix(cfg: &[u8], slot: u8) -> R<Vec<u8>> {
    let mut c = cfg.to_vec();
    if current_slot(&c).is_some() {
        let p = c[..SS].windows(17).position(|w| w == b"os_prefix=slots/s").unwrap();
        c[p + 17] = b'0' + slot;
        return Ok(c);
    }
    let a0 = head0(&c)?;
    let ap = c.windows(6).position(|w| w == b"[all]\n").filter(|&p| p >= SS);
    let Some(ap) = ap else { return e("config.txt migration step (a) not applied") };
    let after = c[ap..].to_vec();
    let nl = c[a0..].iter().position(|&x| x == b'\n').map(|p| a0 + p + 1).unwrap_or(c.len());
    for x in split_lines(&c[..a0]).into_iter().chain(split_lines(&c[a0..nl])) {
        if !is_comment(x) && !split_lines(&after).iter().any(|y| *y == x) {
            return e("config.txt migration step (a) incomplete (essential line missing under [all])");
        }
    }
    let mut h = format!("os_prefix=slots/s{}/\n", slot).into_bytes();
    h.extend(pad(a0 - h.len()));
    c[..a0].copy_from_slice(&h);
    Ok(c)
}
pub fn try_text(cfg: &[u8]) -> Vec<u8> {
    let mut v = cfg.to_vec();
    if v.last() != Some(&b'\n') {
        v.push(b'\n');
    }
    v.extend(TRY_LINE.as_bytes());
    v
}
pub fn derive_try(base: &[u8], marker: &str) -> Vec<u8> {
    let nl = base.last() == Some(&b'\n');
    let mut v = base.to_vec();
    while v.last().map_or(false, |c| c.is_ascii_whitespace()) {
        v.pop();
    }
    v.extend(format!(" leandros.tryboot={}", marker).bytes());
    if nl {
        v.push(b'\n');
    }
    v
}

// ---------------------------------------------------------------- info

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub tag: String,
    pub state: String,
    pub kernel: String,
    pub klen: u64,
    pub initrd: String,
    pub ilen: u64,
}
impl Info {
    pub fn bytes(&self) -> Vec<u8> {
        let mut v = format!("tag={}\nstate={:<8}\nkernel={}\nklen={}\ninitrd={}\nilen={}\n", self.tag, self.state, self.kernel, self.klen, self.initrd, self.ilen).into_bytes();
        assert!(v.len() < SS, "info too long");
        v.resize(SS - 1, b' ');
        v.push(b'\n');
        v
    }
    pub fn parse(b: &[u8]) -> Info {
        let mut i = Info::default();
        for l in String::from_utf8_lossy(b).lines() {
            if let Some((k, v)) = l.split_once('=') {
                let v = v.trim().to_string();
                match k {
                    "tag" => i.tag = v,
                    "state" => i.state = v,
                    "kernel" => i.kernel = v,
                    "klen" => i.klen = v.parse().unwrap_or(0),
                    "initrd" => i.initrd = v,
                    "ilen" => i.ilen = v.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        i
    }
}

pub fn slot_path(n: u8, f: &str) -> String {
    format!("slots/s{}/{}", n, f)
}

// ---------------------------------------------------------------- system hooks

pub trait Sys {
    fn has_word(&self, w: &str) -> bool;
    fn tag(&self) -> String;
    fn hold(&self) -> R<()>;
    fn confirm(&self) -> R<()>;
    fn reboot_try(&self) -> R<()>;
    fn reboot_plain(&self) -> R<()>;
    fn good_initrd(&self) -> R<Vec<u8>>;
    fn now(&self) -> String;
}

// ---------------------------------------------------------------- setup

pub struct SetupOpts {
    pub ksz: u64,
    pub isz: u64,
    pub accept_root_risk: bool,
}
const CRITICAL: &[&str] = &["config.txt", "kernel.img", "initrd.cpio", "cmdline.txt", "overlays", "kernel_2712.img", "cmdline_try.txt", "tryboot.txt", "slots"];

struct Obj {
    name: String,
    dir: bool,
    size: u32,
    data: Vec<u8>,
    first: u32,
    n: u32,
    bulk: bool,
}

pub fn setup<D: Disk>(f: &mut Fat<D>, o: &SetupOpts) -> R<String> {
    f.check_align()?;
    let root = f.dir(f.root)?;
    for n in ["slots", "tryboot.txt", "cmdline_try.txt"] {
        if root.ents.iter().any(|x| x.name.eq_ignore_ascii_case(n)) {
            return e(format!("{} already exists in the root (setup already ran?)", n));
        }
    }
    let cfg_e = f.need("config.txt")?;
    f.need("kernel.img")?;
    let cmd_e = f.need("cmdline.txt")?;
    let dtb_e = f.need(DTB)?;
    let cfg = f.read_file(&cfg_e)?;
    let cmd = f.read_file(&cmd_e)?;
    let dtb = f.read_file(&dtb_e)?;
    let (a, _s) = migrate_a(&cfg)?;
    // root sector for the new entries
    let (end_lba, end_idx) = root.end.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "root directory has no free entry"))?;
    let new_root_ents: Vec<[u8; 32]> = vec![];
    let _ = new_root_ents;
    let cb = f.cluster_bytes();
    let ncl = |sz: u64| ((sz + cb - 1) / cb).max(1) as u32;
    // objects: small first (contiguous), bulk last
    let mut objs: Vec<Obj> = vec![];
    let mut add = |name: String, dir: bool, size: u64, data: Vec<u8>, bulk: bool| {
        objs.push(Obj { name, dir, size: size as u32, n: if dir { 1 } else { ncl(size) }, data, first: 0, bulk });
    };
    let tb = try_text(&a);
    add("tryboot.txt".into(), false, tb.len() as u64, tb.clone(), false);
    add("cmdline_try.txt".into(), false, cmd.len() as u64 + 26, derive_try(&cmd, "noprefix"), false);
    add("slots".into(), true, 0, vec![], false);
    for n in 0..NSLOTS {
        add(format!("slots/s{}", n), true, 0, vec![], false);
        add(slot_path(n, "cmdline.txt"), false, cmd.len() as u64, cmd.clone(), false);
        let ct = derive_try(&cmd, "1");
        add(slot_path(n, "cmdline_try.txt"), false, ct.len() as u64, ct, false);
        add(slot_path(n, DTB), false, dtb.len() as u64, dtb.clone(), false);
        add(slot_path(n, "info"), false, SS as u64, Info { tag: "-".into(), state: "free".into(), ..Default::default() }.bytes(), false);
    }
    for n in 0..NSLOTS {
        add(slot_path(n, "kernel.img"), false, o.ksz, vec![], true);
        add(slot_path(n, "initrd.cpio"), false, o.isz, vec![], true);
    }
    // fix cmdline_try root size to the real derived length
    objs[1].size = objs[1].data.len() as u32;
    objs[1].n = ncl(objs[1].data.len() as u64);
    let total: u32 = objs.iter().map(|x| x.n).sum();
    let base = (f.max_used()? + 1 + 127) / 128 * 128;
    let base = base.max(2);
    if (base as u64 + total as u64) > f.ncl as u64 + 2 {
        return e(format!("not enough free space: need {} clusters from {}", total, base));
    }
    let groups_end = (base as u64 + total as u64 + 127) / 128 * 128;
    for s in (base as u64 / 128)..groups_end / 128 {
        if s >= f.fatsz {
            return e("allocation runs past the FAT");
        }
        for fat in 0..f.nfat {
            if f.rd(f.rsv + fat * f.fatsz + s, 1)?.iter().any(|&b| b != 0) {
                return e("FAT sector not entirely free");
            }
        }
    }
    let mut c = base;
    for x in objs.iter_mut() {
        x.first = c;
        c += x.n;
    }
    let find = |objs: &Vec<Obj>, nm: &str| objs.iter().find(|x| x.name == nm).map(|x| x.first).unwrap();
    // 1. data (small, non-dir files)
    let mut writes: Vec<(u64, Vec<u8>)> = vec![];
    for x in objs.iter().filter(|x| !x.dir && !x.bulk) {
        let mut d = x.data.clone();
        d.resize((x.n as u64 * cb) as usize, 0);
        writes.push((f.cl_lba(x.first), d));
    }
    // 2. chains: FAT1 and FAT2 images
    let mut fatsec: HashMap<u64, Vec<u8>> = HashMap::new();
    for x in &objs {
        for k in 0..x.n {
            let cl = x.first + k;
            let v: u32 = if k + 1 == x.n { 0x0FFF_FFFF } else { cl + 1 };
            let s = fatsec.entry(cl as u64 / 128).or_insert_with(|| vec![0u8; SS]);
            let o = (cl as usize % 128) * 4;
            s[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    let mut keys: Vec<u64> = fatsec.keys().copied().collect();
    keys.sort();
    for fat in (0..f.nfat).rev() {
        for &s in &keys {
            writes.push((f.rsv + fat * f.fatsz + s, fatsec[&s].clone()));
        }
    }
    // 3. directories, children before parents
    let mk_dir = |entries: Vec<[u8; 32]>, first: u32, parent: u32| {
        let mut d = vec![0u8; cb as usize];
        let mut all = vec![mk_ent(&alias("."), 0, 0x10, first, 0), mk_ent(&alias(".."), 0, 0x10, parent, 0)];
        all.extend(entries);
        for (i, x) in all.iter().enumerate() {
            d[i * 32..i * 32 + 32].copy_from_slice(x);
        }
        d
    };
    let slots_first = find(&objs, "slots");
    for n in 0..NSLOTS {
        let mut es = vec![];
        let dir = find(&objs, &format!("slots/s{}", n));
        let sz = |nm: &str| objs.iter().find(|x| x.name == slot_path(n, nm)).unwrap().size;
        let fr = |nm: &str| find(&objs, &slot_path(n, nm));
        es.extend(mk_entries("kernel.img", None, 0x20, fr("kernel.img"), sz("kernel.img")));
        es.extend(mk_entries("initrd.cpio", Some(alias("INITRD~1CPI")), 0x20, fr("initrd.cpio"), sz("initrd.cpio")));
        es.extend(mk_entries("cmdline.txt", None, 0x20, fr("cmdline.txt"), sz("cmdline.txt")));
        es.extend(mk_entries("cmdline_try.txt", Some(alias("CMDLIN~1TXT")), 0x20, fr("cmdline_try.txt"), sz("cmdline_try.txt")));
        es.extend(mk_entries(DTB, Some(alias("BCM271~1DTB")), 0x20, fr(DTB), sz(DTB)));
        es.extend(mk_entries("info", None, 0x20, fr("info"), sz("info")));
        if es.len() + 2 > (cb / 32) as usize {
            return e("slot directory entries do not fit one cluster");
        }
        writes.push((f.cl_lba(dir), mk_dir(es, dir, slots_first)));
    }
    let mut es = vec![];
    for n in 0..NSLOTS {
        es.extend(mk_entries(&format!("s{}", n), None, 0x10, find(&objs, &format!("slots/s{}", n)), 0));
    }
    writes.push((f.cl_lba(slots_first), mk_dir(es, slots_first, 0)));
    // 4. the single root sector
    let tbo = &objs[0];
    let cto = &objs[1];
    let mut ne: Vec<[u8; 32]> = vec![];
    ne.extend(mk_entries("slots", None, 0x10, slots_first, 0));
    ne.extend(mk_entries("tryboot.txt", None, 0x20, tbo.first, tbo.size));
    ne.extend(mk_entries("cmdline_try.txt", Some(alias("CMDLIN~1TXT")), 0x20, cto.first, cto.size));
    if root.ents.iter().any(|x| x.short == alias("CMDLIN~1TXT")) {
        return e("root already has a CMDLIN~1.TXT alias");
    }
    if end_idx + ne.len() > 16 {
        return e("the root directory sector has no room for the new entries");
    }
    let crit: Vec<String> = root.ents.iter().filter(|x| x.lba == end_lba && (CRITICAL.contains(&x.name.to_lowercase().as_str()) || x.name.ends_with(".dtb"))).map(|x| x.name.clone()).collect();
    if !crit.is_empty() && !o.accept_root_risk {
        return e(format!("the root-dir sector that takes the new entries also holds firmware-critical entries ({}); pass --accept-root-sector-risk to proceed", crit.join(",")));
    }
    let mut rs = f.rd(end_lba, 1)?;
    for (i, x) in ne.iter().enumerate() {
        rs[(end_idx + i) * 32..(end_idx + i) * 32 + 32].copy_from_slice(x);
    }
    writes.push((end_lba, rs));
    // go
    let mut lbas = vec![];
    for (l, d) in &writes {
        lbas.extend(*l..*l + (d.len() / SS) as u64);
    }
    f.allow(lbas)?;
    for (l, d) in &writes {
        f.w(*l, d)?;
    }
    f.d.flush()?;
    Ok(format!("setup: {} clusters from {} ({} sector writes, last = root sector {}), root-risk={}", total, base, f.nwrites, end_lba, !crit.is_empty()))
}

// ---------------------------------------------------------------- state

pub struct State {
    pub cfg_e: Ent,
    pub cfg: Vec<u8>,
    pub tb_e: Ent,
    pub tb: Vec<u8>,
    pub infos: Vec<Info>,
    pub cur: Option<u8>,
    pub tb_slot: Option<u8>,
}
pub fn load<D: Disk>(f: &mut Fat<D>) -> R<State> {
    let cfg_e = f.need("config.txt")?;
    let cfg = f.read_file(&cfg_e)?;
    let tb_e = f.need("tryboot.txt")?;
    let tb = f.read_file(&tb_e)?;
    let mut infos = vec![];
    for n in 0..NSLOTS {
        let ie = f.need(&slot_path(n, "info"))?;
        infos.push(Info::parse(&f.read_file(&ie)?));
    }
    Ok(State { cur: current_slot(&cfg), tb_slot: current_slot(&tb), cfg_e, cfg, tb_e, tb, infos })
}
/// Sectors (index within the file) where config.txt and the trial differ, over
/// the first `len(config)` bytes.
pub fn diff_sectors(cfg: &[u8], tb: &[u8]) -> Vec<usize> {
    (0..(cfg.len() + SS - 1) / SS).filter(|&k| {
        let (a, b) = (k * SS, ((k + 1) * SS).min(cfg.len()));
        tb.get(a..b) != Some(&cfg[a..b])
    }).collect()
}
fn silent_cmdline_check<D: Disk>(f: &mut Fat<D>, n: u8) -> R<()> {
    let r = f.need("cmdline.txt")?;
    let root = f.read_file(&r)?;
    let s = f.need(&slot_path(n, "cmdline.txt"))?;
    let t = f.need(&slot_path(n, "cmdline_try.txt"))?;
    let (sb, tbb) = (f.read_file(&s)?, f.read_file(&t)?);
    if sb != root || tbb != derive_try(&root, "1") {
        return e(format!("slot s{} cmdline copies differ from the root cmdline.txt; re-run setup (contents not shown)", n));
    }
    Ok(())
}
fn verify_slot<D: Disk>(f: &mut Fat<D>, n: u8, i: &Info) -> R<()> {
    f.fresh()?;
    let k = f.need(&slot_path(n, "kernel.img"))?;
    let kb = f.read_file(&k)?;
    let r = f.need(&slot_path(n, "initrd.cpio"))?;
    let rb = f.read_file(&r)?;
    if i.klen as usize > kb.len() || i.ilen as usize > rb.len() || sha(&kb[..i.klen as usize]) != i.kernel || sha(&rb[..i.ilen as usize]) != i.initrd {
        return e(format!("slot s{} content does not match its info (sha)", n));
    }
    if kb[i.klen as usize..].iter().any(|&b| b != 0) || rb[i.ilen as usize..].iter().any(|&b| b != 0) {
        return e(format!("slot s{} has a non-zero tail", n));
    }
    Ok(())
}
fn write_sector_of<D: Disk>(f: &mut Fat<D>, en: &Ent, k: usize, data: &[u8]) -> R<u64> {
    let l = f.file_lbas(en)?;
    let lba = l[k];
    f.w(lba, data)?;
    Ok(lba)
}
fn set_info<D: Disk>(f: &mut Fat<D>, n: u8, i: &Info) -> R<()> {
    let ie = f.need(&slot_path(n, "info"))?;
    write_sector_of(f, &ie, 0, &i.bytes())?;
    Ok(())
}

// ---------------------------------------------------------------- stage

pub struct StageOpts {
    pub slot: Option<u8>,
    pub kernel: Vec<u8>,
    pub initrd: Vec<u8>,
    pub tag: String,
    pub sha_k: Option<String>,
    pub sha_i: Option<String>,
}
pub fn stage<D: Disk>(f: &mut Fat<D>, sys: &dyn Sys, o: &StageOpts) -> R<u8> {
    f.check_align()?;
    let st = load(f)?;
    if o.tag.is_empty() || o.tag.len() > 100 || o.tag.contains('\n') {
        return e("bad --tag");
    }
    if o.sha_k.as_ref().map_or(false, |h| *h != sha(&o.kernel)) || o.sha_i.as_ref().map_or(false, |h| *h != sha(&o.initrd)) {
        return e("kernel/initrd sha256 does not match the expected value");
    }
    if sha(&o.initrd) != sha(&sys.good_initrd()?) {
        return e("initrd sha256 != /boot/pi/good/initrd (only the good initrd may reach the card)");
    }
    let bad = diff_sectors(&st.cfg, &st.tb);
    if bad.iter().any(|&k| k != 0) {
        return e("config.txt and tryboot.txt differ beyond sector 0: commit the pending migration step first");
    }
    let cur = st.cur;
    let prev = st.infos.iter().position(|i| i.state == "prev").map(|p| p as u8);
    let t = match o.slot {
        Some(t) => t,
        None => (0..NSLOTS).find(|n| Some(*n) != cur && Some(*n) != prev).ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no free slot"))?,
    };
    if t >= NSLOTS || Some(t) == cur || Some(t) == prev {
        return e("slot is the good or previous slot (or out of range)");
    }
    silent_cmdline_check(f, t)?;
    let ke = f.need(&slot_path(t, "kernel.img"))?;
    let re = f.need(&slot_path(t, "initrd.cpio"))?;
    if o.kernel.len() as u64 > ke.size as u64 || o.initrd.len() as u64 > re.size as u64 {
        return e("kernel or initrd larger than the slot");
    }
    let (kl, rl) = (f.file_lbas(&ke)?, f.file_lbas(&re)?);
    let ie = f.need(&slot_path(t, "info"))?;
    let il = f.file_lbas(&ie)?;
    let tbl = f.file_lbas(&st.tb_e)?;
    let mut al = kl.clone();
    al.extend(&rl);
    al.extend(&il);
    al.push(tbl[0]);
    f.allow(al)?;
    let mut info = Info { tag: o.tag.clone(), state: "writing".into(), kernel: sha(&o.kernel), klen: o.kernel.len() as u64, initrd: sha(&o.initrd), ilen: o.initrd.len() as u64 };
    set_info(f, t, &info)?;
    for (data, en, l) in [(&o.kernel, &ke, &kl), (&o.initrd, &re, &rl)] {
        let mut b = data.clone();
        b.resize(l.len() * SS, 0);
        let _ = en;
        let mut i = 0;
        while i < l.len() {
            let mut j = i + 1;
            while j < l.len() && l[j] == l[j - 1] + 1 && (j - i) < 2048 {
                j += 1;
            }
            f.w(l[i], &b[i * SS..j * SS])?;
            i = j;
        }
    }
    f.d.flush()?;
    if !f.dry {
        verify_slot(f, t, &info)?;
    }
    info.state = "staged".into();
    set_info(f, t, &info)?;
    let newtb = set_prefix(&st.tb[..st.cfg.len()], t)?;
    let mut s0 = newtb[..SS].to_vec();
    if st.tb.len() < SS || st.cfg.len() < SS {
        return e("config too small");
    }
    s0.truncate(SS);
    f.w(tbl[0], &s0)?;
    f.d.flush()?;
    Ok(t)
}

// ---------------------------------------------------------------- try / commit / rollback / abort

pub fn try_<D: Disk>(f: &mut Fat<D>, sys: &dyn Sys, dry: bool) -> R<String> {
    let st = load(f)?;
    let d = diff_sectors(&st.cfg, &st.tb);
    let what = if d == [0] {
        let t = st.tb_slot.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "tryboot.txt names no slot"))?;
        if Some(t) == st.cur {
            return e("tryboot.txt names the good slot");
        }
        let i = st.infos[t as usize].clone();
        if i.state != "staged" {
            return e(format!("slot s{} is not staged", t));
        }
        verify_slot(f, t, &i)?;
        format!("slot s{} ({})", t, i.tag)
    } else if d.len() == 1 && st.tb_slot.is_none() {
        "migration step (a): factory kernel with the flag".to_string()
    } else if d.is_empty() && st.tb_slot.is_none() {
        "flag-only probe: factory kernel".to_string()
    } else {
        return e("tryboot.txt is not a single pending change");
    };
    if dry {
        return Ok(format!("would sync + reboot(RESTART2,\"0 tryboot\") into {}", what));
    }
    sys.reboot_try()?;
    Ok(format!("rebooting into {}", what))
}

pub fn commit<D: Disk>(f: &mut Fat<D>, sys: &dyn Sys, skip_hold: bool) -> R<String> {
    let st = load(f)?;
    let d = diff_sectors(&st.cfg, &st.tb);
    if d.len() != 1 {
        return e(format!("tryboot.txt must differ from config.txt in exactly one sector (found {})", d.len()));
    }
    let k = d[0];
    let mut post: Option<(u8, Option<u8>)> = None;
    if k == 0 {
        let t = st.tb_slot.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "tryboot.txt names no slot"))?;
        if Some(t) == st.cur {
            return e("trial slot is already the config slot");
        }
        if !sys.has_word("leandros.tryboot=1") {
            return e("not running a tryboot trial (marker absent)");
        }
        let i = st.infos[t as usize].clone();
        if i.state != "staged" || !sys.tag().contains(&i.tag) {
            return e("running kernel tag does not match the staged slot");
        }
        verify_slot(f, t, &i)?;
        post = Some((t, st.cur));
    } else if !sys.has_word("leandros.tryboot=noprefix") && !sys.has_word("leandros.tryboot=1") {
        return e("not running a tryboot trial (marker absent)");
    }
    // the exact bytes of the one sector
    let (a, b) = (k * SS, ((k + 1) * SS).min(st.cfg.len()));
    let cl = f.file_lbas(&st.cfg_e)?;
    let mut sec = f.rd(cl[k], 1)?;
    sec[..b - a].copy_from_slice(&st.tb[a..b]);
    if !skip_hold && !f.dry {
        sys.hold()?;
    }
    f.allow(vec![cl[k]])?;
    f.w(cl[k], &sec)?;
    f.d.flush()?;
    if !f.dry {
        f.fresh()?;
        if f.rd(cl[k], 1)? != sec {
            return e("config.txt sector read-back mismatch");
        }
        sys.confirm()?;
    }
    if let Some((t, old)) = post {
        // non-critical bookkeeping, each a single info sector
        let mut lb = vec![];
        for n in 0..NSLOTS {
            let ie = f.need(&slot_path(n, "info"))?;
            lb.extend(f.file_lbas(&ie)?);
        }
        f.allow(lb)?;
        for n in 0..NSLOTS {
            let mut i = st.infos[n as usize].clone();
            let ns = if n == t { "good" } else if Some(n) == old { "prev" } else if i.state == "prev" { "free" } else { continue };
            i.state = ns.into();
            set_info(f, n, &i)?;
        }
        return Ok(format!("committed: config.txt sector {} -> slot s{}", k, t));
    }
    Ok(format!("committed: config.txt sector {} (migration step)", k))
}

pub fn rollback<D: Disk>(f: &mut Fat<D>) -> R<String> {
    let st = load(f)?;
    let cur = st.cur.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "config.txt has no os_prefix"))?;
    let p = st.infos.iter().position(|i| i.state == "prev").ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no previous slot"))? as u8;
    verify_slot(f, p, &st.infos[p as usize].clone())?;
    silent_cmdline_check(f, p)?;
    let nc = set_prefix(&st.cfg, p)?;
    let cl = f.file_lbas(&st.cfg_e)?;
    let tl = f.file_lbas(&st.tb_e)?;
    let mut lb = vec![cl[0], tl[0]];
    for n in 0..NSLOTS {
        let ie = f.need(&slot_path(n, "info"))?;
        lb.extend(f.file_lbas(&ie)?);
    }
    f.allow(lb)?;
    f.w(cl[0], &nc[..SS])?;
    f.d.flush()?;
    f.fresh()?;
    if f.rd(cl[0], 1)? != nc[..SS] {
        return e("config.txt sector read-back mismatch");
    }
    f.w(tl[0], &nc[..SS])?;
    let (mut a, mut b) = (st.infos[cur as usize].clone(), st.infos[p as usize].clone());
    a.state = "prev".into();
    b.state = "good".into();
    set_info(f, p, &b)?;
    set_info(f, cur, &a)?;
    f.d.flush()?;
    Ok(format!("rolled back: s{} -> s{}", cur, p))
}

pub fn status<D: Disk>(f: &mut Fat<D>, sys: &dyn Sys) -> R<String> {
    let st = load(f)?;
    let mut s = String::new();
    let free = f.free_clusters()?;
    s += &format!("fat32 spc={} data0={} clusters={} free={} ({} MiB) aligned={}\n", f.spc, f.data0, f.ncl, free, free as u64 * f.cluster_bytes() >> 20, f.check_align().is_ok());
    s += &format!("config.txt len={} sha8={} os_prefix={}\n", st.cfg.len(), &sha(&st.cfg)[..8], st.cur.map_or("none".into(), |n| format!("slots/s{}/", n)));
    s += &format!("tryboot.txt os_prefix={} pending-sectors={:?}\n", st.tb_slot.map_or("none".into(), |n| format!("slots/s{}/", n)), diff_sectors(&st.cfg, &st.tb));
    for (n, i) in st.infos.iter().enumerate() {
        s += &format!("s{} state={} tag={} klen={} k8={} ilen={} i8={}\n", n, i.state, i.tag, i.klen, i.kernel.get(..8).unwrap_or("-"), i.ilen, i.initrd.get(..8).unwrap_or("-"));
    }
    for n in 0..NSLOTS {
        s += &format!("s{} cmdline-copies={}\n", n, if silent_cmdline_check(f, n).is_ok() { "match" } else { "MISMATCH" });
    }
    s += &format!("running tag={} trial-marker={} noprefix-marker={}\n", sys.tag(), sys.has_word("leandros.tryboot=1"), sys.has_word("leandros.tryboot=noprefix"));
    Ok(s)
}

#[cfg(test)]
#[path = "picard_tests.rs"]
mod tests;

/// Every `pi` verb except `status` writes (or plans writes): refuse unless the
/// kernel confirmed (BLKISBOOTFAT) that the opened device is the boot-FAT one.
pub fn write_guard(sub: &str, dev: &str, boot_fat: bool) -> std::io::Result<()> {
    if sub == "status" || boot_fat {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("espctl pi {}: refusing, {} is not the boot-FAT device (kernel BLKISBOOTFAT != 1)", sub, dev),
    ))
}
