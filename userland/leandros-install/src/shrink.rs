//! Dual-boot v1b session A: shrink the user's ext4 filesystem (and nothing
//! else; the GPT stays untouched, session B moves the partition end).
//!
//! `shrink --part DEV --to SIZE [--dry-run] --yes [--json]`
//! `shrink --recover [--part DEV] [--dir D] --yes`   (e2undo after a crash)
//! `shrink-status [--dir D] [--mark-verified]`
//!
//! All ext4 writes go through e2fsck and resize2fs (e2fsprogs 1.47.1); this
//! module reads the superblock itself and gates on it. Every write to the
//! disk runs under a BLKWRWIN window covering only the ext4 partition.
//!
//! Undo durability: resize2fs/e2fsck write `*.e2undo` files on the stick's
//! f2fs, which may run `f2fs.fsync=lazy`. The f2fs server never defers an
//! fsync on a path ending in `.e2undo` (servers/f2fs), so the undo records
//! are on the medium before the fsync returns, which e2fsprogs's undo
//! manager does before it writes the protected block.

use crate::dualboot::{backup, gpt_lines, lbs, on_battery, set_window};
use crate::gpt::{self, Gpt};
use crate::mkhome;
use crate::progress::{esc, Out};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
pub const BACKUP_ROOT: &str = "/home/dualboot-backup";

#[derive(Default, Clone)]
pub struct Opts {
    pub part: Option<String>,
    pub to: Option<u64>,
    pub dry_run: bool,
    pub yes: bool,
    pub json: bool,
    pub recover: bool,
    pub dir: Option<String>,
    pub root: Option<String>,
    pub mark_verified: bool,
    pub check_sb: Option<String>,
}

// ---------------------------------------------------------------- superblock

#[derive(Debug, Clone, Default)]
pub struct Sb {
    pub blocks: u64,
    pub free_blocks: u64,
    pub bs: u64,
    pub inodes: u64,
    pub state: u16,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    pub last_orphan: u32,
    pub error_count: u32,
    pub mmp_block: u64,
}

const KNOWN_COMPAT: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x100 | 0x200 | 0x400 | 0x800 | 0x1000;
const KNOWN_INCOMPAT: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x40 | 0x80 | 0x100 | 0x200 | 0x400 | 0x1000 | 0x2000 | 0x4000 | 0x8000 | 0x10000 | 0x20000;
const KNOWN_RO: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x100 | 0x200 | 0x400 | 0x800 | 0x1000 | 0x2000 | 0x4000 | 0x8000 | 0x10000;

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

pub fn parse_sb(b: &[u8]) -> Result<Sb, String> {
    if b.len() < 1024 || u16::from_le_bytes([b[0x38], b[0x39]]) != 0xEF53 {
        return Err("not an ext2/3/4 filesystem (no 0xEF53 magic)".into());
    }
    let incompat = le32(b, 0x60);
    let hi = |o: usize| if incompat & 0x80 != 0 { (le32(b, o) as u64) << 32 } else { 0 };
    Ok(Sb {
        blocks: le32(b, 0x04) as u64 | hi(0x150),
        free_blocks: le32(b, 0x0C) as u64 | hi(0x158),
        bs: 1024u64 << le32(b, 0x18),
        inodes: le32(b, 0x00) as u64,
        state: u16::from_le_bytes([b[0x3A], b[0x3B]]),
        compat: le32(b, 0x5C),
        incompat,
        ro_compat: le32(b, 0x64),
        last_orphan: le32(b, 0xE8),
        error_count: le32(b, 0x194),
        mmp_block: u64::from_le_bytes(b[0x168..0x170].try_into().unwrap()),
    })
}

/// Superblock problems that refuse a shrink (id, message).
pub fn sb_gates(s: &Sb) -> Vec<(&'static str, String)> {
    let mut v = vec![];
    if s.state & 1 == 0 || s.state & 2 != 0 {
        v.push(("S1", format!("filesystem state {:#x} is not clean (boot Linux and shut down cleanly, or run e2fsck)", s.state)));
    }
    if s.incompat & 0x4 != 0 {
        v.push(("S2", "the journal needs recovery (mount the filesystem once in Linux and shut down cleanly)".into()));
    }
    if s.error_count != 0 || s.state & 2 != 0 {
        v.push(("S3", format!("the superblock records {} filesystem errors", s.error_count)));
    }
    if s.last_orphan != 0 || s.ro_compat & 0x10000 != 0 {
        v.push(("S4", "orphaned inodes are pending (mount and unmount in Linux first)".into()));
    }
    if s.incompat & 0x100 != 0 || s.mmp_block != 0 {
        v.push(("S5", "multi-mount protection is in use".into()));
    }
    let unk = (s.compat & !KNOWN_COMPAT, s.incompat & !KNOWN_INCOMPAT, s.ro_compat & !KNOWN_RO);
    if unk != (0, 0, 0) {
        v.push(("S6", format!("feature bits unknown to e2fsprogs 1.47.1 (compat {:#x} incompat {:#x} ro {:#x})", unk.0, unk.1, unk.2)));
    }
    v
}

// ----------------------------------------------------------------- helpers

fn tool(name: &str) -> String {
    for d in ["/usr/sbin", "/sbin", "/usr/bin", "/bin"] {
        let p = format!("{}/{}", d, name);
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    name.to_string()
}

pub struct Run {
    pub rc: i32,
    pub text: String,
}

fn run(name: &str, args: &[&str]) -> Result<Run, String> {
    let o = Command::new(tool(name)).args(args).stdin(Stdio::null()).output().map_err(|e| format!("{}: {}", name, e))?;
    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&o.stderr));
    Ok(Run { rc: o.status.code().unwrap_or(-1), text })
}

fn tail(s: &str) -> String {
    let l: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    l[l.len().saturating_sub(3)..].join(" | ")
}

fn sync_all() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::sync();
    }
}

fn free_on(path: &str) -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(c) = std::ffi::CString::new(path) {
            let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0 {
                return s.f_bavail as u64 * s.f_frsize as u64;
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        u64::MAX
    }
}

fn sha_file(p: &str) -> Result<String, String> {
    let mut f = File::open(p).map_err(|e| format!("{}: {}", p, e))?;
    let mut h = Sha256::new();
    let mut b = vec![0u8; MIB as usize];
    loop {
        let n = f.read(&mut b).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(crate::copy::hex(&h.finalize()))
}

/// Durable small file: write, fsync, sync(2).
fn put(path: &str, data: &str) -> Result<(), String> {
    let tmp = format!("{}.new", path);
    let mut f = File::create(&tmp).map_err(|e| format!("{}: {}", tmp, e))?;
    f.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    sync_all();
    Ok(())
}

fn gate(out: &Out, id: &str, name: &str, ok: bool, msg: &str) {
    out.event(
        &format!("{{\"ev\":\"gate\",\"id\":\"{}\",\"name\":\"{}\",\"status\":\"{}\",\"msg\":\"{}\"}}", id, esc(name), if ok { "pass" } else { "fail" }, esc(msg)),
        &format!("gate {} {}: {} ({})", id, name, if ok { "pass" } else { "fail" }, msg),
    );
}

/// "/dev/nvme0n1p2" -> ("/dev/nvme0n1", 2); "/dev/sda2" -> ("/dev/sda", 2).
pub fn split_part(p: &str) -> Option<(String, usize)> {
    let digits = p.chars().rev().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits == p.len() {
        return None;
    }
    let (head, n) = p.split_at(p.len() - digits);
    let head = match head.strip_suffix('p') {
        Some(h) if h.ends_with(|c: char| c.is_ascii_digit()) => h,
        _ => head,
    };
    Some((head.to_string(), n.parse().ok()?))
}

struct Part {
    disk: File,
    g: Gpt,
    n: usize,
    first: u64,
    len: u64,
    ss: u64,
    dev: String,
}

fn open_part(dev: &str) -> Result<Part, String> {
    let (disk_path, n) = split_part(dev).ok_or_else(|| format!("{}: not a partition node", dev))?;
    let mut disk = OpenOptions::new().read(true).write(true).open(&disk_path).map_err(|e| format!("{}: {}", disk_path, e))?;
    let ss = lbs(&disk);
    let total = disk.seek(SeekFrom::End(0)).map_err(|e| e.to_string())? / ss;
    let g = Gpt::read(&mut disk, ss, total)?;
    let e = g.entry(n - 1).ok_or_else(|| format!("{} is not in the GPT", dev))?;
    let (first, len) = (e.first * ss / 512, (e.last - e.first + 1) * ss / 512);
    Ok(Part { disk, g, n, first, len, ss, dev: dev.to_string() })
}

impl Part {
    fn window(&self) -> Result<(), String> {
        set_window(&self.disk, &[(self.first, self.len)])
    }
    fn close(&self) {
        let _ = set_window(&self.disk, &[]);
    }
    fn read_sb(&self) -> Result<Sb, String> {
        read_sb_path(&self.dev)
    }
}

pub fn read_sb_path(dev: &str) -> Result<Sb, String> {
    let mut f = File::open(dev).map_err(|e| format!("{}: {}", dev, e))?;
    let mut b = vec![0u8; 2048];
    f.seek(SeekFrom::Start(1024)).and_then(|_| f.read_exact(&mut b[..1024])).map_err(|e| format!("{}: {}", dev, e))?;
    parse_sb(&b[..1024])
}

/// Hibernated swap anywhere on the disk (S1SUSPEND, S2SUSPEND, ULSUSPEND).
fn hibernated(p: &mut Part) -> Vec<usize> {
    let mut v = vec![];
    for (i, e) in p.g.used() {
        let mut b = [0u8; 10];
        if p.disk.seek(SeekFrom::Start(e.first * p.ss + 4086)).is_ok() && p.disk.read_exact(&mut b).is_ok() {
            if b.starts_with(b"S1SUSPEND") || b.starts_with(b"S2SUSPEND") || b.starts_with(b"ULSUSPEND") {
                v.push(i + 1);
            }
        }
    }
    v
}

fn mounted(dev: &str) -> bool {
    crate::plan::read_to_string("/proc/mounts").lines().any(|l| l.split(' ').next() == Some(dev))
}

// -------------------------------------------------------- debugfs file walk

fn debugfs_script(dev: &str, script: &str, dir: &str) -> Result<String, String> {
    let p = format!("{}/dbgfs.cmd", dir);
    std::fs::write(&p, script).map_err(|e| e.to_string())?;
    Ok(run("debugfs", &["-f", &p, dev])?.text)
}

/// All regular files as (inode, path). One debugfs run per directory depth.
fn walk(dev: &str, dir: &str) -> Result<Vec<(u64, String)>, String> {
    let mut level: Vec<(u64, String)> = vec![(2, String::new())];
    let mut files: BTreeMap<u64, String> = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    while !level.is_empty() {
        let script: String = level.iter().map(|(i, _)| format!("ls -p <{}>\n", i)).collect();
        let text = debugfs_script(dev, &script, dir)?;
        let by_ino: BTreeMap<u64, String> = level.iter().cloned().collect();
        let mut cur = String::new();
        let mut next = vec![];
        for l in text.lines() {
            if let Some(c) = l.split("debugfs: ").nth(1).filter(|_| l.contains("debugfs:")) {
                let ino = c.split('<').nth(1).and_then(|s| s.split('>').next()).and_then(|s| s.parse::<u64>().ok());
                cur = ino.and_then(|i| by_ino.get(&i).cloned()).unwrap_or_default();
                continue;
            }
            let f: Vec<&str> = l.split('/').collect();
            if !l.starts_with('/') || f.len() < 7 {
                continue;
            }
            let (ino, mode, name) = (f[1].parse::<u64>().unwrap_or(0), u32::from_str_radix(f[2], 8).unwrap_or(0), f[5]);
            if name == "." || name == ".." || ino == 0 {
                continue;
            }
            let path = format!("{}/{}", cur, name);
            match mode & 0o170000 {
                0o040000 if seen.insert(ino) => next.push((ino, path)),
                0o100000 => {
                    files.entry(ino).or_insert(path);
                }
                _ => {}
            }
        }
        level = next;
    }
    Ok(files.into_iter().collect())
}

/// Highest physical block of every file in `files` that reaches `limit` or
/// beyond (those are the files whose data resize2fs has to move).
fn high_files(dev: &str, files: &[(u64, String)], limit: u64, dir: &str) -> Result<Vec<(u64, String)>, String> {
    let mut hits = vec![];
    for chunk in files.chunks(4000) {
        let script: String = chunk.iter().map(|(i, _)| format!("ex <{}>\n", i)).collect();
        let text = debugfs_script(dev, &script, dir)?;
        let mut cur = 0u64;
        let mut max = 0u64;
        let mut noext = vec![];
        let mut flush = |cur: u64, max: u64, hits: &mut Vec<(u64, String)>| {
            if cur != 0 && max >= limit {
                if let Some((i, p)) = chunk.iter().find(|(i, _)| *i == cur) {
                    hits.push((*i, p.clone()));
                }
            }
        };
        for l in text.lines() {
            if l.contains("debugfs:") {
                flush(cur, max, &mut hits);
                max = 0;
                cur = l.split('<').nth(1).and_then(|s| s.split('>').next()).and_then(|s| s.parse().ok()).unwrap_or(0);
                continue;
            }
            let t: Vec<&str> = l.split_whitespace().collect();
            if t.len() >= 9 && t[0].contains('/') && t[3] == "-" && t[6] == "-" {
                max = max.max(t[7].parse().unwrap_or(0));
            } else if t.len() == 6 && t[0].contains('/') && t[3] == "-" {
                max = max.max(t[5].parse().unwrap_or(0));
            } else if l.contains("extent") && l.contains("not") {
                noext.push(cur);
            }
        }
        flush(cur, max, &mut hits);
        for ino in noext {
            let t = run("debugfs", &["-R", &format!("blocks <{}>", ino), dev])?.text;
            let m = t.split_whitespace().filter_map(|x| x.parse::<u64>().ok()).max().unwrap_or(0);
            if m >= limit {
                if let Some((i, p)) = files.iter().find(|(i, _)| *i == ino) {
                    hits.push((*i, p.clone()));
                }
            }
        }
    }
    Ok(hits)
}

fn hash_path(dev: &str, path: &str) -> Result<String, String> {
    if path.contains('"') || path.contains('\n') {
        return Ok("skipped".into());
    }
    let mut c = Command::new(tool("debugfs"))
        .args(["-R", &format!("cat \"{}\"", path), dev])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut o = c.stdout.take().unwrap();
    let mut h = Sha256::new();
    let mut b = vec![0u8; MIB as usize];
    loop {
        let n = o.read(&mut b).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    let _ = c.wait();
    Ok(crate::copy::hex(&h.finalize()))
}

// ------------------------------------------------------------------- state

fn state_write(dir: &str, kv: &[(&str, String)]) -> Result<(), String> {
    let s: String = kv.iter().map(|(k, v)| format!("{}={}\n", k, v)).collect();
    put(&format!("{}/state", dir), &s)
}

fn state_read(dir: &str) -> BTreeMap<String, String> {
    crate::plan::read_to_string(&format!("{}/state", dir))
        .lines()
        .filter_map(|l| l.split_once('=').map(|(a, b)| (a.to_string(), b.to_string())))
        .collect()
}

fn latest_dir(root: &str) -> Option<String> {
    let mut v: Vec<String> = std::fs::read_dir(root).ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("shrink-")).collect();
    v.sort();
    v.pop().map(|n| format!("{}/{}", root, n))
}

// --------------------------------------------------------------- the shrink

/// Parse resize2fs -p output ("Begin pass N (max = M)" then 40 'X') into a
/// 0..99 percentage.
struct Prog {
    pass: u32,
    x: u32,
}

impl Prog {
    fn feed(&mut self, chunk: &str, out: &Out, last: &mut u32) {
        let mut rest = chunk;
        while !rest.is_empty() {
            if let Some(i) = rest.find("Begin pass ") {
                let n = rest[i + 11..].split(|c: char| !c.is_ascii_digit()).next().and_then(|s| s.parse().ok()).unwrap_or(self.pass);
                self.pass = n;
                self.x = 0;
                self.x += rest[..i].matches('X').count() as u32;
                rest = &rest[i + 11..];
                continue;
            }
            self.x += rest.matches('X').count() as u32;
            break;
        }
        let pct = ((self.pass.saturating_sub(1) * 40 + self.x.min(40)) * 100 / 200).min(99);
        if pct != *last {
            *last = pct;
            out.step("resize", pct);
        }
    }
}

fn run_resize(dev: &str, undo: &str, sectors: u64, out: &Out) -> Result<(), String> {
    let size = format!("{}s", sectors);
    let mut c = Command::new(tool("resize2fs"))
        .args(["-z", undo, "-p", dev, &size])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("resize2fs: {}", e))?;
    let mut so = c.stdout.take().unwrap();
    let mut p = Prog { pass: 1, x: 0 };
    let mut last = 101u32;
    let mut all = String::new();
    let mut b = [0u8; 512];
    loop {
        let n = so.read(&mut b).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let s = String::from_utf8_lossy(&b[..n]).into_owned();
        p.feed(&s, out, &mut last);
        all.push_str(&s);
    }
    let mut err = String::new();
    if let Some(mut e) = c.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let st = c.wait().map_err(|e| e.to_string())?;
    if !st.success() {
        return Err(format!("resize2fs failed (rc {}): {}", st.code().unwrap_or(-1), tail(&format!("{}{}", all.replace(['\x08', 'X', '-'], ""), err))));
    }
    Ok(())
}

/// Allowed-in-trivial-fixes: free counts and bitmap differences only.
pub fn trivial_fsck(text: &str) -> bool {
    text.lines().all(|l| {
        let t = l.trim();
        t.is_empty()
            || t.starts_with("Pass ")
            || t.starts_with("e2fsck ")
            || t.contains(": ")  && t.contains("blocks") && t.contains("files") // summary line
            || t.contains("Free blocks count wrong")
            || t.contains("Free inodes count wrong")
            || t.contains("differences:")
            || t.starts_with("+(") || t.starts_with("-(") || t.starts_with("+") || t.starts_with("-")
            || t.contains("Fix? no")
            || t.contains("WARNING: Filesystem still has errors")
            || t.contains("***** FILE SYSTEM WAS MODIFIED")
            || t.contains("Warning: skipping journal recovery")
            || t.contains("has gone ")  // 'has gone N days without being checked'
            || t.contains("check forced")
            || t.contains("recovering journal")
            || t.contains("Inode bitmap differences")
    })
}

fn fsck_n(dev: &str) -> Result<Run, String> {
    run("e2fsck", &["-fn", dev])
}

fn parse_min_blocks(t: &str) -> Option<u64> {
    t.lines().find_map(|l| l.strip_prefix("Estimated minimum size of the filesystem:")).and_then(|s| s.trim().parse().ok())
}

pub fn shrink(o: &Opts) -> Result<(), String> {
    let out = Out { json: o.json };
    if let Some(f) = &o.check_sb {
        // Superblock gates only, on any file or device (used by the tests).
        let sb = read_sb_path(f)?;
        let g = sb_gates(&sb);
        for (id, m) in &g {
            gate(&out, id, "sb", false, m);
        }
        return if g.is_empty() { out.info("superblock gates pass"); Ok(()) } else { Err(format!("refused: {}", g.iter().map(|x| x.0).collect::<Vec<_>>().join(","))) };
    }
    if o.recover {
        return recover(o, &out);
    }
    let dev = o.part.clone().ok_or("--part required")?;
    let to = o.to.ok_or("--to required")?;
    if !o.yes && !o.dry_run {
        return Err("refusing to modify the disk without --yes".into());
    }
    let mut p = open_part(&dev)?;
    let sb = p.read_sb()?;
    let root = o.root.clone().unwrap_or_else(|| BACKUP_ROOT.to_string());
    let mut fails: Vec<String> = vec![];
    let mut g = |id: &str, name: &str, ok: bool, msg: &str| {
        gate(&out, id, name, ok, msg);
        if !ok {
            fails.push(format!("{}: {}", id, msg));
        }
    };
    let sbg = sb_gates(&sb);
    for (id, name) in [("S1", "clean"), ("S2", "journal"), ("S3", "errors"), ("S4", "orphans"), ("S5", "mmp"), ("S6", "features")] {
        match sbg.iter().find(|(i, _)| *i == id) {
            Some((_, m)) => g(id, name, false, m),
            None => g(id, name, true, "ok"),
        }
    }
    let h = hibernated(&mut p);
    g("S7", "swap", h.is_empty(), &if h.is_empty() { "no hibernated swap".to_string() } else { format!("partition(s) {:?} hold a hibernation image (resume or discard it in Linux)", h) });
    g("S0", "mounted", !mounted(&dev), "the partition is not mounted");
    g("S0b", "power", !on_battery(), "mains power");
    // Size.
    let dump = run("dumpe2fs", &["-h", &dev])?;
    g("S6b", "features-dumpe2fs", !dump.text.contains("FEATURE_") && dump.rc == 0, "dumpe2fs lists only known features");
    let minr = run("resize2fs", &["-P", &dev])?;
    let min_blocks = parse_min_blocks(&minr.text).ok_or_else(|| format!("resize2fs -P: {}", tail(&minr.text)))?;
    let (min_bytes, cur_bytes, used_bytes) = (min_blocks * sb.bs, sb.blocks * sb.bs, (sb.blocks - sb.free_blocks) * sb.bs);
    let new_blocks = to / sb.bs;
    out.event(
        &format!("{{\"ev\":\"shrink_info\",\"part\":\"{}\",\"fs_bytes\":{},\"used_bytes\":{},\"min_bytes\":{},\"min_target_bytes\":{},\"block_size\":{},\"part_bytes\":{}}}", esc(&dev), cur_bytes, used_bytes, min_bytes, min_bytes + 2 * GIB, sb.bs, p.len * 512),
        &format!("{}: fs {} MiB, used {} MiB, minimum {} MiB (+2 GiB margin)", dev, cur_bytes >> 20, used_bytes >> 20, min_bytes >> 20),
    );
    g("S8", "target-size", new_blocks * sb.bs >= min_bytes + 2 * GIB && new_blocks < sb.blocks, &format!("target {} MiB, needs >= {} MiB and < {} MiB", to >> 20, (min_bytes + 2 * GIB) >> 20, cur_bytes >> 20));
    // Stick space: undo + e2image + rehearsal clone.
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {}", root, e))?;
    let moved = (cur_bytes - new_blocks.min(sb.blocks) * sb.bs).min(used_bytes);
    let need = moved + used_bytes / 50 + 768 * MIB;
    let free = free_on(&root);
    g("S9", "stick-space", free >= need, &format!("{} MiB free on {}, need about {} MiB for the undo file, e2image and the rehearsal clone", free >> 20, root, need >> 20));
    // fsck -fn
    let fck = fsck_n(&dev)?;
    let triv = fck.rc == 0 || (fck.rc & !1 == 0) || trivial_fsck(&fck.text);
    g("S10", "fsck", triv, &format!("e2fsck -fn rc {}{}", fck.rc, if triv { String::new() } else { format!(": {}", tail(&fck.text)) }));
    if !fails.is_empty() {
        return Err(format!("refused: {}", fails.join("; ")));
    }
    if o.dry_run {
        out.event(
            &format!("{{\"ev\":\"done\",\"dry_run\":true,\"part\":\"{}\",\"new_bytes\":{},\"fsck_rc\":{}}}", esc(&dev), new_blocks * sb.bs, fck.rc),
            &format!("dry run: would shrink {} to {} MiB; nothing was written", dev, new_blocks * sb.bs >> 20),
        );
        return Ok(());
    }

    // ---- 2. backup
    let dir = format!("{}/shrink-{}", root, mkhome::now());
    out.step("backup", 0);
    let esp = p.g.used().into_iter().find(|(_, e)| e.ptype == gpt::ESP_TYPE).map(|(_, e)| e);
    let esp = esp.ok_or("no EFI system partition on the disk")?;
    let disk_path = split_part(&dev).unwrap().0;
    let mut files = backup(&disk_path, &p.g, esp.first, esp.last - esp.first + 1, &dir, &out)?;
    let e2i = format!("{}/p{}.e2i", dir, p.n);
    let r = run("e2image", &[&dev, &e2i])?;
    if r.rc != 0 {
        return Err(format!("e2image failed: {}", tail(&r.text)));
    }
    put(&format!("{}/dumpe2fs.txt", dir), &dump.text)?;
    put(&format!("{}/gpt-at-shrink.txt", dir), &gpt_lines(&p.g).join("\n"))?;
    let want = sha_file(&e2i)?;
    {
        let f = File::open(&e2i).map_err(|e| e.to_string())?;
        let _ = f.sync_all();
    }
    sync_all();
    if sha_file(&e2i)? != want {
        return Err("e2image read-back mismatch".into());
    }
    put(&format!("{}/E2IMAGE.sha256", dir), &format!("{}  p{}.e2i\n", want, p.n))?;
    files.push(format!("p{}.e2i", p.n));
    out.step("backup", 100);
    let undo_fsck = format!("{}/fsck.e2undo", dir);
    let undo_rs = format!("{}/resize.e2undo", dir);
    let mut st = vec![
        ("phase", "prepared".to_string()),
        ("part", dev.clone()),
        ("orig_blocks", sb.blocks.to_string()),
        ("new_blocks", new_blocks.to_string()),
        ("block_size", sb.bs.to_string()),
        ("fsck_undo", undo_fsck.clone()),
        ("resize_undo", undo_rs.clone()),
        ("backup_dir", dir.clone()),
    ];
    state_write(&dir, &st)?;
    put(&format!("{}/shrink-latest", root), &format!("{}\n", dir))?;

    // ---- 3. rehearsal on a metadata clone
    out.step("rehearse", 0);
    let clone = format!("{}/rehearsal.raw", dir);
    let r = run("e2image", &["-r", &dev, &clone])?;
    if r.rc != 0 {
        return Err(format!("rehearsal: e2image -r failed: {}", tail(&r.text)));
    }
    out.step("rehearse", 40);
    let r = run("resize2fs", &["-f", &clone, &format!("{}s", new_blocks * sb.bs / 512)])?;
    if r.rc != 0 {
        let _ = std::fs::remove_file(&clone);
        return Err(format!("refused: the rehearsal resize failed on the metadata clone: {}", tail(&r.text)));
    }
    out.step("rehearse", 80);
    let c = fsck_n(&clone)?;
    let _ = std::fs::remove_file(&clone);
    if c.rc != 0 {
        return Err(format!("refused: the rehearsed filesystem is not clean: {}", tail(&c.text)));
    }
    out.step("rehearse", 100);

    // ---- 5a. hash files whose data has to move
    out.step("hash", 0);
    let flist = walk(&dev, &dir)?;
    out.step("hash", 20);
    let hi = high_files(&dev, &flist, new_blocks * sb.bs / sb.bs, &dir)?;
    let mut hashes: Vec<(String, String)> = vec![];
    for (k, (_, path)) in hi.iter().enumerate() {
        hashes.push((path.clone(), hash_path(&dev, path)?));
        out.step("hash", 20 + (k as u32 + 1) * 80 / hi.len() as u32);
    }
    put(&format!("{}/hashes.txt", dir), &hashes.iter().map(|(p, h)| format!("{}  {}\n", h, p)).collect::<String>())?;
    out.info(&format!("{} of {} files have blocks above the new end; hashed", hi.len(), flist.len()));

    // ---- 4. the real run
    p.window()?;
    let res = real_run(&dev, &mut st, &dir, &undo_fsck, &undo_rs, new_blocks, &out, &hashes, sb.bs);
    p.close();
    match res {
        Ok(()) => {}
        Err(e) => {
            // Roll back with e2undo (window re-opened).
            out.info(&format!("rolling back: {}", e));
            let _ = state_write(&dir, &[("phase", "rollback".into()), ("part", dev.clone()), ("fsck_undo", undo_fsck.clone()), ("resize_undo", undo_rs.clone()), ("backup_dir", dir.clone()), ("orig_blocks", sb.blocks.to_string()), ("new_blocks", new_blocks.to_string()), ("block_size", sb.bs.to_string())]);
            let rb = rollback(&dev, &mut p, &undo_fsck, &undo_rs, &sb, &out);
            let _ = state_write(&dir, &[("phase", if rb.is_ok() { "rolled_back" } else { "rollback_failed" }.into()), ("part", dev.clone()), ("backup_dir", dir.clone())]);
            return Err(match rb {
                Ok(()) => format!("{}; the filesystem was restored with e2undo and verified", e),
                Err(e2) => format!("{}; rollback FAILED: {}", e, e2),
            });
        }
    }
    let _ = state_write(&dir, &[("phase", "done".into()), ("part", dev.clone()), ("orig_blocks", sb.blocks.to_string()), ("new_blocks", new_blocks.to_string()), ("block_size", sb.bs.to_string()), ("fsck_undo", undo_fsck), ("resize_undo", undo_rs), ("backup_dir", dir.clone())]);
    let nsb = read_sb_path(&dev)?;
    out.event(
        &format!("{{\"ev\":\"done\",\"part\":\"{}\",\"new_bytes\":{},\"old_bytes\":{},\"backup_dir\":\"{}\",\"state\":\"boot_linux\",\"hashed\":{}}}", esc(&dev), nsb.blocks * nsb.bs, cur_bytes, esc(&dir), hashes.len()),
        &format!("shrunk {} to {} MiB; boot Linux once to verify, then run shrink-status", dev, nsb.blocks * nsb.bs >> 20),
    );
    Ok(())
}

fn real_run(dev: &str, st: &mut Vec<(&str, String)>, dir: &str, undo_fsck: &str, undo_rs: &str, new_blocks: u64, out: &Out, hashes: &[(String, String)], bs: u64) -> Result<(), String> {
    let set = |ph: &str, st: &mut Vec<(&str, String)>| {
        for kv in st.iter_mut() {
            if kv.0 == "phase" {
                kv.1 = ph.to_string();
            }
        }
        state_write(dir, st)
    };
    set("fscking", st)?;
    out.step("fsck", 0);
    let f = run("e2fsck", &["-fy", "-z", undo_fsck, dev])?;
    if f.rc & !1 != 0 {
        return Err(format!("e2fsck -fy failed (rc {}): {}", f.rc, tail(&f.text)));
    }
    out.step("fsck", 100);
    set("resizing", st)?;
    out.step("resize", 0);
    run_resize(dev, undo_rs, new_blocks * bs / 512, out)?;
    out.step("resize", 100);
    set("verifying", st)?;
    out.step("verify", 0);
    let c = fsck_n(dev)?;
    if c.rc != 0 {
        return Err(format!("e2fsck -fn after the resize is not clean (rc {}): {}", c.rc, tail(&c.text)));
    }
    out.step("verify", 50);
    for (path, want) in hashes {
        let got = hash_path(dev, path)?;
        if &got != want {
            return Err(format!("file {} changed during the resize (sha256 mismatch)", path));
        }
    }
    out.step("verify", 100);
    Ok(())
}

fn rollback(dev: &str, p: &mut Part, undo_fsck: &str, undo_rs: &str, sb: &Sb, out: &Out) -> Result<(), String> {
    p.window()?;
    let mut msgs = vec![];
    for u in [undo_rs, undo_fsck] {
        if !std::path::Path::new(u).exists() {
            continue;
        }
        let r = run("e2undo", &[u, dev])?;
        out.info(&format!("e2undo {}: rc {} {}", u, r.rc, tail(&r.text)));
        if r.rc != 0 {
            msgs.push(format!("e2undo {}: {}", u, tail(&r.text)));
        }
    }
    p.close();
    sync_all();
    let c = fsck_n(dev)?;
    let nsb = read_sb_path(dev)?;
    if c.rc != 0 {
        return Err(format!("after e2undo e2fsck -fn rc {}: {} {}", c.rc, tail(&c.text), msgs.join("; ")));
    }
    if nsb.blocks != sb.blocks {
        return Err(format!("after e2undo the size is {} blocks, expected {}", nsb.blocks, sb.blocks));
    }
    Ok(())
}

fn recover(o: &Opts, out: &Out) -> Result<(), String> {
    let root = o.root.clone().unwrap_or_else(|| BACKUP_ROOT.to_string());
    let dir = o.dir.clone().or_else(|| latest_dir(&root)).ok_or("no shrink session found")?;
    let s = state_read(&dir);
    let phase = s.get("phase").cloned().unwrap_or_default();
    if matches!(phase.as_str(), "done" | "verified" | "rolled_back") {
        return Err(format!("session {} is in phase {}; nothing to recover", dir, phase));
    }
    let dev = o.part.clone().or_else(|| s.get("part").cloned()).ok_or("--part required")?;
    if !o.yes {
        return Err("refusing to modify the disk without --yes".into());
    }
    let orig: u64 = s.get("orig_blocks").and_then(|v| v.parse().ok()).ok_or("state lacks orig_blocks")?;
    let mut p = open_part(&dev)?;
    let sb = Sb { blocks: orig, ..Default::default() };
    out.step("recover", 0);
    let ufsck = s.get("fsck_undo").cloned().unwrap_or_default();
    let urs = s.get("resize_undo").cloned().unwrap_or_default();
    let r = rollback(&dev, &mut p, &ufsck, &urs, &sb, out);
    if r.is_err() && phase == "prepared" {
        // Crashed before any ext4 write: only the clean check matters.
        let c = fsck_n(&dev)?;
        if c.rc == 0 && read_sb_path(&dev)?.blocks == orig {
            let _ = state_write(&dir, &[("phase", "rolled_back".into()), ("part", dev.clone()), ("backup_dir", dir.clone())]);
            out.step("recover", 100);
            out.event("{\"ev\":\"done\",\"recovered\":true}", "recovered: nothing had been written");
            return Ok(());
        }
    }
    r?;
    state_write(&dir, &[("phase", "rolled_back".into()), ("part", dev.clone()), ("backup_dir", dir.clone())])?;
    out.step("recover", 100);
    out.event(&format!("{{\"ev\":\"done\",\"recovered\":true,\"blocks\":{}}}", orig), "recovered: the filesystem is back at its original size and clean");
    Ok(())
}

/// `shrink-status`: none | interrupted | boot_linux | ready_b.
pub fn status(o: &Opts) -> Result<(), String> {
    let out = Out { json: o.json };
    let root = o.root.clone().unwrap_or_else(|| BACKUP_ROOT.to_string());
    let Some(dir) = o.dir.clone().or_else(|| latest_dir(&root)) else {
        out.event("{\"ev\":\"shrink_status\",\"state\":\"none\"}", "none");
        return Ok(());
    };
    let mut s = state_read(&dir);
    let ph = s.get("phase").cloned().unwrap_or_default();
    if o.mark_verified {
        if ph != "done" {
            return Err(format!("session is in phase {}; only a finished session can be marked verified", ph));
        }
        s.insert("phase".into(), "verified".into());
        let kv: Vec<(&str, String)> = s.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        state_write(&dir, &kv)?;
    }
    let ph = s.get("phase").cloned().unwrap_or_default();
    let state = match ph.as_str() {
        "done" => "boot_linux",
        "verified" => "ready_b",
        "rolled_back" | "prepared" | "" => "none",
        _ => "interrupted",
    };
    let nb = s.get("new_blocks").cloned().unwrap_or_default();
    let bs = s.get("block_size").cloned().unwrap_or_default();
    out.event(
        &format!("{{\"ev\":\"shrink_status\",\"state\":\"{}\",\"phase\":\"{}\",\"dir\":\"{}\",\"new_blocks\":\"{}\",\"block_size\":\"{}\"}}", state, esc(&ph), esc(&dir), nb, bs),
        &format!("{} (phase {}, {})", state, ph, dir),
    );
    Ok(())
}
