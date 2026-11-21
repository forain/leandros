//! mbrtool -- MBR partition table, raw block copy and F2FS size for block
//! nodes (`/dev/mmcblk0`, `/dev/loopN`) or image files. Installed as
//! /usr/sbin/mbrtool (same binary as espctl, dispatched on argv[0]; also
//! `espctl mbr ...`).
//!
//!   print  DEV                      the four primary entries + device size
//!   write  DEV [--dry-run] [--backup FILE] N:START:SIZE:TYPE[:boot] | N:- ...
//!          edit entries (512-byte sectors; SIZE `rest` = to the end of DEV;
//!          `N:-` clears entry N), validate (in bounds, no overlaps), print the
//!          new table, then -- unless --dry-run -- save the old sector 0 to
//!          FILE, write sector 0 once, fsync and BLKRRPART.
//!   rrpart DEV                      re-read the partition table
//!   f2fs   DEV [--offset BYTES]     F2FS superblock: filesystem byte length
//!   copy   IN OUT [--skip B] [--seek B] [--count B]
//!          raw copy (1 MiB chunks, fsync, read-back verify)
//!   sha256 IN [--skip B] [--count B]
//!   selftest                        loop-device + root-guard regression test

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;

type R<T> = io::Result<T>;
fn err<T>(m: impl Into<String>) -> R<T> {
    Err(io::Error::new(io::ErrorKind::Other, m.into()))
}

const BLKRRPART: u64 = 0x125F;
const BLKGETSIZE64: u64 = 0x8008_1272;
const LOOP_SET_FD: u64 = 0x4C00;
const LOOP_CLR_FD: u64 = 0x4C01;
const LOOP_SET_STATUS64: u64 = 0x4C04;
const LOOP_CTL_GET_FREE: u64 = 0x4C82;
const LO_FLAGS_PARTSCAN: u32 = 8;
const F2FS_MAGIC: u32 = 0xF2F5_2010;

pub const USAGE: &str = "usage: mbrtool print DEV
       mbrtool write DEV [--dry-run] [--backup FILE] N:START:SIZE:TYPE[:boot]|N:- ...
       mbrtool rrpart DEV
       mbrtool f2fs DEV [--offset BYTES]
       mbrtool copy IN OUT [--skip BYTES] [--seek BYTES] [--count BYTES]
       mbrtool sha256 IN [--skip BYTES] [--count BYTES]
       mbrtool selftest
(START/SIZE in 512-byte sectors, TYPE hex e.g. 0c 83; SIZE may be `rest`)";

fn ioctl(f: &File, req: u64, arg: usize) -> R<i32> {
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn size_of(f: &File) -> R<u64> {
    let mut v: u64 = 0;
    match ioctl(f, BLKGETSIZE64, &mut v as *mut u64 as usize) {
        Ok(_) => Ok(v),
        Err(_) => Ok(f.metadata()?.len()),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Entry { pub boot: u8, pub ptype: u8, pub start: u32, pub size: u32 }

pub fn parse(mbr: &[u8]) -> R<[Entry; 4]> {
    if mbr.len() < 512 || mbr[510] != 0x55 || mbr[511] != 0xAA { return err("no MBR (55AA missing)"); }
    let mut e = [Entry { boot: 0, ptype: 0, start: 0, size: 0 }; 4];
    for (i, x) in e.iter_mut().enumerate() {
        let b = &mbr[446 + 16 * i..462 + 16 * i];
        *x = Entry {
            boot: b[0], ptype: b[4],
            start: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            size: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        };
    }
    Ok(e)
}

/// CHS fields are written as the LBA-only marker (0xFE 0xFF 0xFF), which
/// every LBA-reading firmware (the Pi's included) ignores.
fn encode(mbr: &mut [u8], e: &[Entry; 4]) {
    for (i, x) in e.iter().enumerate() {
        let b = &mut mbr[446 + 16 * i..462 + 16 * i];
        if x.ptype == 0 && x.size == 0 { b.fill(0); continue; }
        b[0] = x.boot;
        b[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        b[4] = x.ptype;
        b[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        b[8..12].copy_from_slice(&x.start.to_le_bytes());
        b[12..16].copy_from_slice(&x.size.to_le_bytes());
    }
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
}

pub fn validate(e: &[Entry; 4], sectors: u64) -> R<()> {
    for (i, a) in e.iter().enumerate() {
        if a.ptype == 0 && a.size == 0 { continue; }
        if a.ptype == 0 { return err(format!("p{}: type 0 with a non-zero size", i + 1)); }
        if a.size == 0 { return err(format!("p{}: zero size", i + 1)); }
        if a.start == 0 { return err(format!("p{}: starts at sector 0 (the MBR)", i + 1)); }
        if a.boot != 0 && a.boot != 0x80 { return err(format!("p{}: boot flag 0x{:02x}", i + 1, a.boot)); }
        let end = a.start as u64 + a.size as u64;
        if sectors != 0 && end > sectors {
            return err(format!("p{}: ends at sector {} past the device end {}", i + 1, end, sectors));
        }
        for (j, b) in e.iter().enumerate().skip(i + 1) {
            if b.ptype == 0 && b.size == 0 { continue; }
            let (bs, be) = (b.start as u64, b.start as u64 + b.size as u64);
            if (a.start as u64) < be && bs < end {
                return err(format!("p{} and p{} overlap", i + 1, j + 1));
            }
        }
    }
    Ok(())
}

fn print_table(path: &str, mbr: &[u8], bytes: u64) {
    let sig = u32::from_le_bytes(mbr[440..444].try_into().unwrap());
    println!("{}: {} bytes ({} sectors), disk id 0x{:08x}", path, bytes, bytes / 512, sig);
    match parse(mbr) {
        Ok(e) => for (i, x) in e.iter().enumerate() {
            if x.ptype == 0 && x.size == 0 { println!("p{}: empty", i + 1); continue; }
            println!("p{}: boot=0x{:02x} type=0x{:02x} start={} size={} end={} ({} MiB)",
                i + 1, x.boot, x.ptype, x.start, x.size, x.start as u64 + x.size as u64,
                x.size as u64 * 512 >> 20);
        },
        Err(e) => println!("{}", e),
    }
}

fn read_sector0(f: &File) -> R<Vec<u8>> {
    let mut b = vec![0u8; 512];
    f.read_exact_at(&mut b, 0)?;
    Ok(b)
}

fn parse_spec(s: &str, sectors: u64, e: &mut [Entry; 4]) -> R<()> {
    let bad = || io::Error::new(io::ErrorKind::Other, format!("bad entry spec `{}`", s));
    let f: Vec<&str> = s.split(':').collect();
    let n: usize = f.first().and_then(|v| v.parse().ok()).filter(|n| (1..=4).contains(n)).ok_or_else(bad)?;
    if f.len() == 2 && f[1] == "-" {
        e[n - 1] = Entry { boot: 0, ptype: 0, start: 0, size: 0 };
        return Ok(());
    }
    if f.len() != 4 && !(f.len() == 5 && f[4] == "boot") { return Err(bad()); }
    let start: u64 = f[1].parse().map_err(|_| bad())?;
    let size: u64 = if f[2] == "rest" {
        sectors.checked_sub(start).filter(|_| sectors != 0).ok_or_else(bad)?
    } else { f[2].parse().map_err(|_| bad())? };
    let ptype = u8::from_str_radix(f[3].trim_start_matches("0x"), 16).map_err(|_| bad())?;
    if start > u32::MAX as u64 || size > u32::MAX as u64 { return err(format!("`{}`: beyond 2 TiB", s)); }
    e[n - 1] = Entry { boot: if f.len() == 5 { 0x80 } else { 0 }, ptype, start: start as u32, size: size as u32 };
    Ok(())
}

fn opt(a: &mut Vec<String>, k: &str) -> Option<String> {
    let i = a.iter().position(|x| x == k)?;
    if i + 1 >= a.len() { return None; }
    let v = a.remove(i + 1);
    a.remove(i);
    Some(v)
}
fn flag(a: &mut Vec<String>, k: &str) -> bool {
    match a.iter().position(|x| x == k) { Some(i) => { a.remove(i); true } None => false }
}
fn num(a: &mut Vec<String>, k: &str) -> R<Option<u64>> {
    match opt(a, k) {
        None => Ok(None),
        Some(v) => v.parse().map(Some).map_err(|_| io::Error::new(io::ErrorKind::Other, format!("{} {}: not a number", k, v))),
    }
}

/// Edit, validate and (unless `dry`) write sector 0, then BLKRRPART.
pub fn write_table(path: &str, specs: &[String], dry: bool, backup: Option<&str>) -> R<[Entry; 4]> {
    let f = OpenOptions::new().read(true).write(!dry).open(path)?;
    let bytes = size_of(&f)?;
    let sectors = bytes / 512;
    let old = read_sector0(&f)?;
    let mut e = parse(&old).unwrap_or([Entry { boot: 0, ptype: 0, start: 0, size: 0 }; 4]);
    for s in specs { parse_spec(s, sectors, &mut e)?; }
    validate(&e, sectors)?;
    let mut new = old.clone();
    encode(&mut new, &e);
    println!("new table{}:", if dry { " (dry run, nothing written)" } else { "" });
    print_table(path, &new, bytes);
    if dry { return Ok(e); }
    if let Some(b) = backup {
        std::fs::write(b, &old)?;
        println!("old sector 0 saved to {}", b);
    }
    f.write_all_at(&new, 0)?;
    f.sync_all()?;
    if read_sector0(&f)? != new { return err("sector 0 read-back differs"); }
    match ioctl(&f, BLKRRPART, 0) {
        Ok(_) => println!("written; partition table re-read"),
        Err(x) => println!("written; BLKRRPART: {} (re-read by hand or reboot)", x),
    }
    Ok(e)
}

fn f2fs_bytes(f: &File, off: u64) -> R<(u64, u32, u64)> {
    let mut b = vec![0u8; 4096];
    f.read_exact_at(&mut b, off)?;
    let o = 1024;
    let magic = u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    if magic != F2FS_MAGIC { return err(format!("no F2FS superblock at byte {} (magic 0x{:08x})", off + 1024, magic)); }
    let log_bs = u32::from_le_bytes(b[o + 16..o + 20].try_into().unwrap());
    let blocks = u64::from_le_bytes(b[o + 36..o + 44].try_into().unwrap());
    if !(9..=16).contains(&log_bs) { return err(format!("F2FS log_blocksize {} out of range", log_bs)); }
    Ok((blocks, log_bs, blocks << log_bs))
}

fn copy(inp: &str, out: &str, skip: u64, seek: u64, count: Option<u64>) -> R<u64> {
    let i = File::open(inp)?;
    let o = OpenOptions::new().write(true).read(true).create(!out.starts_with("/dev/")).open(out)?;
    let total = match count { Some(c) => c, None => size_of(&i)?.saturating_sub(skip) };
    let mut buf = vec![0u8; 1 << 20];
    let mut chk = vec![0u8; 1 << 20];
    let mut done = 0u64;
    while done < total {
        let n = ((total - done) as usize).min(buf.len());
        i.read_exact_at(&mut buf[..n], skip + done)?;
        o.write_all_at(&buf[..n], seek + done)?;
        done += n as u64;
    }
    o.sync_all()?;
    done = 0;
    while done < total {
        let n = ((total - done) as usize).min(buf.len());
        i.read_exact_at(&mut buf[..n], skip + done)?;
        o.read_exact_at(&mut chk[..n], seek + done)?;
        if buf[..n] != chk[..n] { return err(format!("verify mismatch in the 1 MiB chunk at +{}", done)); }
        done += n as u64;
    }
    Ok(total)
}

fn sha_range(inp: &str, skip: u64, count: Option<u64>) -> R<String> {
    use sha2::{Digest, Sha256};
    let i = File::open(inp)?;
    let total = match count { Some(c) => c, None => size_of(&i)?.saturating_sub(skip) };
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    while done < total {
        let n = ((total - done) as usize).min(buf.len());
        i.read_exact_at(&mut buf[..n], skip + done)?;
        h.update(&buf[..n]);
        done += n as u64;
    }
    Ok(h.finalize().iter().map(|b| format!("{:02x}", b)).collect())
}

pub fn run(mut a: Vec<String>) -> R<()> {
    if a.is_empty() { println!("{}", USAGE); return Ok(()); }
    let cmd = a.remove(0);
    match cmd.as_str() {
        "print" if a.len() == 1 => {
            let f = File::open(&a[0])?;
            print_table(&a[0], &read_sector0(&f)?, size_of(&f)?);
        }
        "write" | "set" => {
            let dry = flag(&mut a, "--dry-run");
            let backup = opt(&mut a, "--backup");
            if a.len() < 2 { return err(USAGE); }
            let dev = a.remove(0);
            write_table(&dev, &a, dry, backup.as_deref())?;
        }
        "rrpart" if a.len() == 1 => {
            ioctl(&File::open(&a[0])?, BLKRRPART, 0)?;
            println!("{}: partition table re-read", a[0]);
        }
        "f2fs" => {
            let off = num(&mut a, "--offset")?.unwrap_or(0);
            if a.len() != 1 { return err(USAGE); }
            let (blocks, lbs, bytes) = f2fs_bytes(&File::open(&a[0])?, off)?;
            println!("f2fs: block_count={} block_size={} fs_bytes={} fs_sectors={} end_sector={}",
                blocks, 1u64 << lbs, bytes, bytes / 512, (off + bytes) / 512);
        }
        "copy" => {
            let skip = num(&mut a, "--skip")?.unwrap_or(0);
            let seek = num(&mut a, "--seek")?.unwrap_or(0);
            let count = num(&mut a, "--count")?;
            if a.len() != 2 { return err(USAGE); }
            let n = copy(&a[0], &a[1], skip, seek, count)?;
            println!("copied {} bytes, verified", n);
        }
        "sha256" => {
            let skip = num(&mut a, "--skip")?.unwrap_or(0);
            let count = num(&mut a, "--count")?;
            if a.len() != 1 { return err(USAGE); }
            println!("{}  {}", sha_range(&a[0], skip, count)?, a[0]);
        }
        "selftest" => return selftest(),
        _ => return err(USAGE),
    }
    Ok(())
}

// ── selftest ────────────────────────────────────────────────────────────────

fn blk_size(path: &str) -> Option<u64> {
    File::open(path).ok().and_then(|f| size_of(&f).ok())
}

fn selftest() -> R<()> {
    let mut fails = 0u32;
    let mut check = |ok: bool, what: &str| {
        println!("[{}] {}", if ok { "PASS" } else { "FAIL" }, what);
        if !ok { fails += 1; }
    };
    const MIB: u64 = 1 << 20;
    // 1. A 16 MiB image with p1 (1 MiB @1 MiB, FAT32) and p2 (2 MiB @4 MiB).
    let img = "/tmp/mbrtool-selftest.img";
    {
        let f = File::create(img)?;
        f.set_len(16 * MIB)?;
    }
    write_table(img, &["1:2048:2048:0c".into(), "2:8192:4096:83".into()], false, None)?;
    let ctl = OpenOptions::new().read(true).write(true).open("/dev/loop-control")?;
    let n = ioctl(&ctl, LOOP_CTL_GET_FREE, 0)?;
    let ldev = format!("/dev/loop{}", n);
    let lf = OpenOptions::new().read(true).write(true).open(&ldev)?;
    let imgf = OpenOptions::new().read(true).write(true).open(img)?;
    ioctl(&lf, LOOP_SET_FD, imgf.as_raw_fd() as usize)?;
    let mut info = vec![0u8; 232];
    info[52..56].copy_from_slice(&LO_FLAGS_PARTSCAN.to_le_bytes());
    let r = ioctl(&lf, LOOP_SET_STATUS64, info.as_ptr() as usize);
    check(r.is_ok(), "LOOP_SET_STATUS64 with LO_FLAGS_PARTSCAN");
    let p = |k: u32| format!("{}p{}", ldev, k);
    check(blk_size(&p(1)) == Some(MIB), "partscan made p1 (1 MiB)");
    check(blk_size(&p(2)) == Some(2 * MIB), "partscan made p2 (2 MiB)");
    // Data through p2 lands at the whole-disk offset.
    let pat: Vec<u8> = (0..65536u32).map(|i| (i * 7 + 3) as u8).collect();
    let wr = OpenOptions::new().write(true).open(p(2)).and_then(|f| { f.write_all_at(&pat, 4096)?; f.sync_all() });
    check(wr.is_ok(), "write 64 KiB into p2");
    let mut back = vec![0u8; pat.len()];
    lf.read_exact_at(&mut back, 8192 * 512 + 4096)?;
    check(back == pat, "p2 data visible at its whole-disk offset");
    // 2. Rewrite the table through the whole-disk node: p1 gone, p2 grows, p3 new.
    let w = write_table(&ldev, &["1:-".into(), "2:8192:8192:83".into(), "3:20480:rest:0c".into()], false, None);
    check(w.is_ok(), "MBR rewrite + BLKRRPART on the whole-disk node");
    check(blk_size(&p(1)).is_none(), "p1 node removed after re-read");
    check(blk_size(&p(2)) == Some(4 * MIB), "p2 node resized to 4 MiB");
    check(blk_size(&p(3)) == Some(6 * MIB), "p3 node added (rest = 6 MiB)");
    let mut back2 = vec![0u8; pat.len()];
    let rd = File::open(p(2)).and_then(|f| f.read_exact_at(&mut back2, 4096));
    check(rd.is_ok() && back2 == pat, "p2 data intact after the re-read");
    // Dry run writes nothing; overlaps are refused.
    let before = read_sector0(&lf)?;
    let _ = write_table(&ldev, &["4:100:100:83".into()], true, None);
    check(read_sector0(&lf)? == before, "--dry-run leaves sector 0 alone");
    check(write_table(&ldev, &["4:9000:10:83".into()], true, None).is_err(), "overlapping entry refused");
    let _ = ioctl(&lf, LOOP_CLR_FD, 0);
    let _ = std::fs::remove_file(img);
    // 3. The mounted root's filesystem bytes refuse raw writes (EBUSY), even
    // of the bytes already there.
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let src = mounts.lines().filter_map(|l| {
        let mut it = l.split_whitespace();
        let (s, m) = (it.next()?, it.next()?);
        (m == "/" && s.starts_with("/dev/")).then(|| s.to_string())
    }).last();
    match src {
        Some(s) => match OpenOptions::new().read(true).write(true).open(&s) {
            Ok(f) => {
                let mut sb = vec![0u8; 512];
                f.read_exact_at(&mut sb, 1024)?;
                let e = f.write_at(&sb, 1024);
                check(matches!(&e, Err(x) if x.raw_os_error() == Some(libc::EBUSY)),
                      &format!("raw write into the mounted root ({}) superblock is EBUSY (got {:?})", s, e.as_ref().map_err(|x| x.raw_os_error())));
                // 3b. The guard covers the fs, not the device: the first 4 KiB
                // past the fs end is writable (its own bytes are written back),
                // the last 4 KiB inside stays EBUSY. Skipped on the real SD
                // card (never write it from a test) and without slack.
                let mut hdr = vec![0u8; 44];
                f.read_exact_at(&mut hdr, 1024)?;
                let fs = (u32::from_le_bytes(hdr[0..4].try_into().unwrap()) == 0xF2F5_2010)
                    .then(|| u64::from_le_bytes(hdr[36..44].try_into().unwrap()) << u32::from_le_bytes(hdr[16..20].try_into().unwrap()));
                let dev = size_of(&f).unwrap_or(0);
                match fs {
                    Some(fs) if !s.starts_with("/dev/mmcblk") && fs >= 4096 && dev >= fs + 4096 => {
                        let mut b = vec![0u8; 4096];
                        f.read_exact_at(&mut b, fs)?;
                        let e = f.write_at(&b, fs);
                        check(matches!(e, Ok(4096)), &format!("raw write just past the root fs end ({} @{}) succeeds (got {:?})", s, fs, e.as_ref().map_err(|x| x.raw_os_error())));
                        f.read_exact_at(&mut b, fs - 4096)?;
                        let e = f.write_at(&b, fs - 4096);
                        check(matches!(&e, Err(x) if x.raw_os_error() == Some(libc::EBUSY)), &format!("raw write of the last root fs block is EBUSY (got {:?})", e.as_ref().map_err(|x| x.raw_os_error())));
                    }
                    _ => println!("[SKIP] past-fs-end write on {} (fs {:?}, device {} bytes)", s, fs, dev),
                }
            }
            Err(x) => check(false, &format!("open root device {}: {}", s, x)),
        },
        None => check(false, "no /dev/ source for / in /proc/self/mounts"),
    }
    // 4. A whole SD card node, when present, is root-only.
    if let Ok(m) = std::fs::metadata("/dev/mmcblk0") {
        use std::os::unix::fs::PermissionsExt;
        check(m.permissions().mode() & 0o777 == 0o600, "/dev/mmcblk0 is 0600");
    }
    if fails == 0 { println!("mbrtool selftest: PASS"); Ok(()) } else { err(format!("mbrtool selftest: {} FAILED", fails)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_validate() {
        let mut m = vec![0u8; 512];
        let mut e = [Entry { boot: 0, ptype: 0, start: 0, size: 0 }; 4];
        parse_spec("1:8192:1048576:0c:boot", 1 << 24, &mut e).unwrap();
        parse_spec("2:1056768:rest:83", 1 << 24, &mut e).unwrap();
        encode(&mut m, &e);
        assert_eq!(parse(&m).unwrap(), e);
        assert_eq!(e[1].size as u64, (1u64 << 24) - 1056768);
        validate(&e, 1 << 24).unwrap();
        parse_spec("3:9000:10:83", 1 << 24, &mut e).unwrap();
        assert!(validate(&e, 1 << 24).is_err());
    }
}
