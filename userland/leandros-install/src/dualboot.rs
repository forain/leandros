//! Dual-boot install into existing unallocated space (`--mode dualboot
//! --use-free`, v1a). Never writes an existing partition except the three
//! files and the marker on the shared ESP. Every write goes through a
//! `BLKWRWIN` window held open for the whole run.

use crate::copy;
use crate::dev::Dev;
use crate::esp;
use crate::gpt::{self, Gpt};
use crate::install::Opts;
use crate::mkhome;
use crate::plan::{self, GIB, LOG_BYTES, MIB, MIN_HOME_BYTES, MIN_ROOT_BYTES};
use crate::progress::{esc, Out};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

pub const BOOT_BYTES: u64 = 512 * MIB;
const S: u64 = gpt::SECTOR;

pub fn min_gap_bytes() -> u64 {
    BOOT_BYTES + MIN_ROOT_BYTES + MIN_HOME_BYTES + LOG_BYTES
}

/// Logical block size of an open disk (BLKSSZGET); 512 when unknown.
pub fn lbs(f: &File) -> u64 {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let mut v: libc::c_int = 0;
        const BLKSSZGET: libc::c_ulong = 0x1268;
        if unsafe { libc::ioctl(f.as_raw_fd(), BLKSSZGET as _, &mut v as *mut libc::c_int) } == 0 && v > 0 {
            return v as u64;
        }
    }
    let _ = f;
    512
}

/// Free gaps (first, last inclusive, in sectors) of a disk, best effort.
pub fn gaps_of(dev: &str, size: u64) -> Vec<(u64, u64)> {
    let mut f = match File::open(dev) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let ss = lbs(&f);
    match Gpt::read(&mut f, ss, size / ss) {
        Ok(g) if ss == 512 => g.gaps(),
        _ => Vec::new(),
    }
}

/// Dual-boot eligibility for `list`: (eligible, reason).
pub fn eligibility(is_boot: bool, mounted: bool, size: u64, has_esp: bool, gaps: &[(u64, u64)]) -> (bool, String) {
    let r = if is_boot {
        "boot disk"
    } else if mounted {
        "has mounted partitions"
    } else if size == 0 {
        "unknown size"
    } else if !has_esp {
        "no EFI system partition"
    } else if gaps.is_empty() {
        "no free space (or no readable 512-byte GPT)"
    } else if gaps.iter().map(|g| (g.1 - g.0 + 1) * S).max().unwrap_or(0) < min_gap_bytes() {
        "free space too small"
    } else {
        ""
    };
    (r.is_empty(), r.to_string())
}

/// Partition extents inside a gap as (start, sectors).
#[derive(Clone, Debug)]
pub struct DbLayout {
    pub boot: (u64, u64),
    pub root: (u64, u64),
    pub home: (u64, u64),
    pub log: (u64, u64),
}

pub fn compute(gap: (u64, u64), root_bytes: u64) -> Result<DbLayout, String> {
    let s = |b: u64| b / S;
    let end_excl = gap.1 + 1;
    let boot = (gap.0, s(BOOT_BYTES));
    let root = (boot.0 + boot.1, s(root_bytes));
    let log = (end_excl - s(LOG_BYTES), s(LOG_BYTES));
    let home_start = root.0 + root.1;
    if log.0 <= home_start || (log.0 - home_start) * S < MIN_HOME_BYTES {
        return Err(format!(
            "free space too small: need {} MiB, largest gap is {} MiB",
            (BOOT_BYTES + root_bytes + MIN_HOME_BYTES + LOG_BYTES) >> 20,
            (end_excl - gap.0) * S >> 20
        ));
    }
    Ok(DbLayout { boot, root, home: (home_start, log.0 - home_start), log })
}

struct Gate<'a> {
    out: &'a Out,
    failed: Vec<String>,
}

impl<'a> Gate<'a> {
    fn pass(&self, id: &str, name: &str, msg: &str) {
        self.emit(id, name, "pass", msg);
    }
    fn na(&self, id: &str, name: &str, msg: &str) {
        self.emit(id, name, "na", msg);
    }
    fn fail(&mut self, id: &str, name: &str, msg: &str) {
        self.emit(id, name, "fail", msg);
        self.failed.push(id.to_string());
    }
    fn emit(&self, id: &str, name: &str, status: &str, msg: &str) {
        self.out.event(
            &format!("{{\"ev\":\"gate\",\"id\":\"{}\",\"name\":\"{}\",\"status\":\"{}\",\"msg\":\"{}\"}}", id, esc(name), status, esc(msg)),
            &format!("gate {} {}: {} ({})", id, name, status, msg),
        );
    }
}

/// `BLKWRWIN` on `f` (a whole-disk node). Ranges are (start, len) in 512-byte
/// units; an empty list closes the window.
#[repr(C)]
struct WrWin {
    count: u32,
    flags: u32,
    r: [[u64; 2]; 16],
}

pub fn set_window(f: &File, ranges: &[(u64, u64)]) -> Result<(), String> {
    if ranges.len() > 16 {
        return Err("too many write window ranges".into());
    }
    let mut w = WrWin { count: ranges.len() as u32, flags: 0, r: [[0; 2]; 16] };
    for (i, r) in ranges.iter().enumerate() {
        w.r[i] = [r.0, r.1];
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        const BLKWRWIN: libc::c_ulong = 0x4108_12F0;
        let r = unsafe { libc::ioctl(f.as_raw_fd(), BLKWRWIN as _, &w as *const WrWin) };
        if r != 0 {
            return Err(format!("BLKWRWIN: {} (kernel without write windows?)", std::io::Error::last_os_error()));
        }
    }
    let _ = (f, w);
    Ok(())
}

fn hexs(b: &[u8]) -> String {
    copy::hex(b)
}

fn sync_all_fs() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::sync();
    }
}

fn free_bytes(path: &str) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let c = std::ffi::CString::new(path).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0 {
            return Some(s.f_bavail as u64 * s.f_frsize as u64);
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Some(u64::MAX)
    }
}

/// True when a mains supply exists and none is online.
pub fn on_battery() -> bool {
    let Ok(rd) = std::fs::read_dir("/sys/class/power_supply") else { return false };
    let (mut mains, mut online) = (0, 0);
    for e in rd.flatten() {
        let p = e.path();
        if plan::read_to_string(&format!("{}/type", p.display())).trim() == "Mains" {
            mains += 1;
            if plan::read_to_string(&format!("{}/online", p.display())).trim() == "1" {
                online += 1;
            }
        }
    }
    mains > 0 && online == 0
}

fn read_at(f: &mut File, off: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(&mut v)?;
    Ok(v)
}

/// Protective MBR must be pure 0xEE (no hybrid), primary and backup entry
/// arrays identical.
fn check_tables(f: &mut File, ss: u64, total: u64) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let mbr = read_at(f, 0, 512).map_err(io)?;
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return Err("no protective MBR".into());
    }
    let types: Vec<u8> = (0..4).map(|i| mbr[446 + i * 16 + 4]).collect();
    if types.iter().filter(|&&t| t == 0xEE).count() != 1 || types.iter().any(|&t| t != 0 && t != 0xEE) {
        return Err("MBR is not a pure protective 0xEE (hybrid MBR?)".into());
    }
    let arr = |f: &mut File, hdr_lba: u64| -> Result<Vec<u8>, String> {
        let h = read_at(f, hdr_lba * ss, 512).map_err(io)?;
        if &h[0..8] != b"EFI PART" {
            return Err(format!("no GPT header at LBA {}", hdr_lba));
        }
        let lba = u64::from_le_bytes(h[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(h[80..84].try_into().unwrap()) as usize;
        let es = u32::from_le_bytes(h[84..88].try_into().unwrap()) as usize;
        if n == 0 || n > 4096 || es < 128 || es > 4096 {
            return Err("bad GPT entry geometry".into());
        }
        read_at(f, lba * ss, n * es).map_err(io)
    };
    if arr(f, 1)? != arr(f, total - 1)? {
        return Err("primary and backup GPT entry arrays differ".into());
    }
    Ok(())
}

pub fn gpt_lines(g: &Gpt) -> Vec<String> {
    g.used()
        .iter()
        .map(|(i, e)| {
            format!(
                "p{} {}..{} type {} guid {} attrs {:016x} name \"{}\"",
                i + 1,
                e.first,
                e.last,
                gpt::guid_text(&e.ptype),
                gpt::guid_text(&e.guid),
                e.attrs,
                e.name
            )
        })
        .collect()
}

fn gpt_json(g: &Gpt) -> String {
    let v: Vec<String> = g
        .used()
        .iter()
        .map(|(i, e)| {
            format!(
                "{{\"n\":{},\"first\":{},\"last\":{},\"type\":\"{}\",\"guid\":\"{}\",\"attrs\":\"{:016x}\",\"name\":\"{}\"}}",
                i + 1,
                e.first,
                e.last,
                gpt::guid_text(&e.ptype),
                gpt::guid_text(&e.guid),
                e.attrs,
                esc(&e.name)
            )
        })
        .collect();
    format!("[{}]", v.join(","))
}

/// Back up the first/last MiB, the GPT dump and the whole ESP to `dir` on the
/// stick, with real syncs (the stick may run `f2fs.fsync=lazy`, under which
/// fsync returns at once; `sync()` is always synchronous) and a read-back
/// comparison. Returns the file list.
pub fn backup(target: &str, g: &Gpt, esp_first: u64, esp_len: u64, dir: &str, out: &Out) -> Result<Vec<String>, String> {
    let io = |e: std::io::Error| e.to_string();
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir, e))?;
    let mut disk = File::open(target).map_err(io)?;
    let total_bytes = g.total * g.ss;
    let mut sums: Vec<(String, String)> = Vec::new();
    fn wr(dir: &str, sums: &mut Vec<(String, String)>, name: &str, data: &[u8]) -> Result<(), String> {
        let io = |e: std::io::Error| e.to_string();
        let p = format!("{}/{}", dir, name);
        let mut f = File::create(&p).map_err(|e| format!("{}: {}", p, e))?;
        f.write_all(data).map_err(io)?;
        f.sync_all().map_err(io)?;
        sums.push((name.to_string(), hexs(&Sha256::digest(data))));
        Ok(())
    }
    wr(dir, &mut sums, "disk-first-1MiB.bin", &read_at(&mut disk, 0, MIB as usize).map_err(io)?)?;
    wr(dir, &mut sums, "disk-last-1MiB.bin", &read_at(&mut disk, total_bytes - MIB, MIB as usize).map_err(io)?)?;
    let mut dump = gpt_lines(g).join("\n");
    dump.push('\n');
    wr(dir, &mut sums, "gpt.txt", dump.as_bytes())?;
    out.step("backup", 10);
    // Full ESP image, all-zero MiB chunks left as holes, hash over the whole stream.
    let p = format!("{}/esp.img", dir);
    let mut ef = File::create(&p).map_err(|e| format!("{}: {}", p, e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; MIB as usize];
    let (mut off, total) = (0u64, esp_len * S);
    disk.seek(SeekFrom::Start(esp_first * S)).map_err(io)?;
    while off < total {
        let n = ((total - off) as usize).min(buf.len());
        disk.read_exact(&mut buf[..n]).map_err(io)?;
        h.update(&buf[..n]);
        if buf[..n].iter().any(|&b| b != 0) {
            ef.seek(SeekFrom::Start(off)).map_err(io)?;
            ef.write_all(&buf[..n]).map_err(io)?;
        }
        off += n as u64;
        out.step("backup", 10 + (off * 70 / total) as u32);
    }
    ef.set_len(total).map_err(io)?;
    ef.sync_all().map_err(io)?;
    drop(ef);
    sums.push(("esp.img".to_string(), hexs(&h.finalize())));
    let restore = format!(
        "LeandrOS dual-boot install backup of {target}\n\n\
         Restore from any Linux live system (adjust the device):\n\
         # partition table (first and last MiB):\n\
         dd if=disk-first-1MiB.bin of={target} bs=1M count=1 conv=notrunc\n\
         dd if=disk-last-1MiB.bin of={target} bs=1M seek=$(( $(blockdev --getsz {target}) / 2048 - 1 )) conv=notrunc\n\
         partprobe {target}\n\
         # EFI system partition (sector {esp_first}, {esp_len} sectors), only if its files were damaged:\n\
         dd if=esp.img of={target} bs=512 seek={esp_first} count={esp_len} conv=notrunc\n\n\
         The install only adds partitions in free space and four files on the ESP\n\
         (EFI/leandros/*, loader/entries/leandros.conf); deleting the new partitions\n\
         and those files restores the original state. Checksums: SHA256SUMS\n"
    );
    wr(dir, &mut sums, "RESTORE.txt", restore.as_bytes())?;
    let sumtxt: String = sums.iter().map(|(n, s)| format!("{}  {}\n", s, n)).collect();
    wr(dir, &mut sums, "SHA256SUMS", sumtxt.as_bytes())?;
    sync_all_fs();
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    sync_all_fs();
    // Read everything back and compare against the recorded hashes.
    for (n, want) in &sums {
        let mut f = File::open(format!("{}/{}", dir, n)).map_err(io)?;
        let mut h = Sha256::new();
        loop {
            let k = f.read(&mut buf).map_err(io)?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
        }
        if &hexs(&h.finalize()) != want {
            return Err(format!("backup read-back mismatch for {}", n));
        }
    }
    out.step("backup", 100);
    Ok(sums.into_iter().map(|(n, _)| n).chain(["SHA256SUMS".to_string()]).collect())
}

pub fn install(o: &Opts) -> Result<(), String> {
    use crate::disks::{self, Kind};
    let out = Out { json: o.json };
    if !o.use_free && !o.after_shrink {
        return Err("--mode dualboot needs --use-free or --after-shrink (shrink the Linux partition first with `shrink`)".into());
    }
    if !o.dry_run && !(cfg!(not(target_os = "linux")) || unsafe_euid_root()) {
        return Err("must run as root".into());
    }
    let target = o.target.clone().ok_or("--target /dev/X is required")?;
    let tname = target.strip_prefix("/dev/").unwrap_or(&target).to_string();
    let names = disks::whole_disks();
    if !names.contains(&tname) {
        return Err(format!("{} is not a whole disk", target));
    }
    let mut gate = Gate { out: &out, failed: Vec::new() };
    // G1: tools, stick, privileges.
    let (bdisk, bpno) = plan::boot_root_part(&names)
        .ok_or("cannot identify the boot disk (no root=PARTUUID= matching a disk on the command line)")?;
    if bdisk == tname {
        gate.fail("G1", "boot stick", "the target is the boot disk");
    }
    let bparts = disks::scan_disk(&bdisk);
    let root_part = bparts.iter().find(|p| p.pno == bpno).ok_or("boot root partition not found")?.clone();
    let esp_part = plan::find_part(&bparts, Kind::Esp).ok_or("the boot disk has no ESP")?.clone();
    let sb = copy::read_sb(&format!("/dev/{}", bdisk), root_part.start).map_err(|e| format!("boot root superblock: {}", e))?;
    if gate.failed.is_empty() {
        gate.pass("G1", "stick and privileges", &format!("running from {} (root p{}), f2fs {} MiB; sync() is used for durability, whatever f2fs.fsync is", bdisk, bpno, sb.bytes() >> 20));
    }
    if plan::mounted_disks(&names).contains(&tname) {
        gate.fail("G2", "disk", "the target has mounted partitions");
    }
    let mut disk = File::open(&target).map_err(|e| format!("{}: {}", target, e))?;
    let ss = lbs(&disk);
    let size = plan::disk_size(&tname);
    if ss != 512 {
        gate.fail("G2", "disk", &format!("logical block size {} (only 512 is supported)", ss));
    }
    let total = size / 512;
    let g = match Gpt::read(&mut disk, 512, total) {
        Ok(g) if !g.from_backup => match check_tables(&mut disk, 512, total) {
            Ok(()) => {
                gate.pass("G2", "disk", "valid GPT (primary = backup), pure protective MBR, 512-byte sectors");
                Some(g)
            }
            Err(e) => {
                gate.fail("G2", "disk", &e);
                None
            }
        },
        Ok(_) => {
            gate.fail("G2", "disk", "primary GPT is damaged (only the backup is valid)");
            None
        }
        Err(e) => {
            gate.fail("G2", "disk", &format!("no usable GPT: {}", e));
            None
        }
    };
    let Some(g) = g else {
        return Err(format!("preflight failed: {}", gate.failed.join(",")));
    };
    // Session B (v1b): the on-disk GPT is `g_disk`; `g` is the table with p2 cut back
    // to the shrunk filesystem, which opens the gap the v1a flow below installs into.
    let g_disk = g.clone();
    let mut g = g;
    let mut sessb: Option<crate::shrink::SessB> = None;
    if o.after_shrink {
        match crate::shrink::session_b(&g_disk, &target, o.backup_root.as_deref()) {
            Ok(sb_) => {
                let desc = format!("{}: ext4 shrunk and Linux booted; p2 sectors {}..{} become {}..{}", sb_.dev, g_disk.entry(sb_.slot).map_or(0, |e| e.first), sb_.old_last, g_disk.entry(sb_.slot).map_or(0, |e| e.first), sb_.new_last);
                gate.pass("B1", "shrink session", &desc);
                if sb_.new_last < sb_.old_last {
                    g.shrink(sb_.slot, sb_.new_last)?;
                }
                sessb = Some(sb_);
            }
            Err(e) => {
                gate.fail("B1", "shrink session", &e);
                return Err(format!("preflight failed: {}", gate.failed.join(",")));
            }
        }
    }
    // The ESP.
    let esps: Vec<_> = g.used().into_iter().filter(|(_, e)| e.ptype == gpt::ESP_TYPE).collect();
    let esp_ent = if esps.len() == 1 { Some(esps[0].1.clone()) } else { None };
    // G5: space.
    let root_bytes = match o.root_gib {
        Some(n) => n * GIB,
        None => MIN_ROOT_BYTES.max((sb.bytes() + GIB + MIB - 1) / MIB * MIB),
    };
    if root_bytes < sb.bytes() {
        return Err(format!("--root-gib too small for the {} MiB root volume", sb.bytes() >> 20));
    }
    let gaps = g.gaps();
    let big = gaps.iter().copied().max_by_key(|g| g.1 - g.0);
    let layout = match big.map(|b| compute(b, root_bytes)) {
        Some(Ok(l)) => {
            gate.pass("G5", "size", &format!("gap {}..{} ({} MiB): boot 512 MiB, root {} MiB, home {} MiB, log 64 MiB", big.unwrap().0, big.unwrap().1, (big.unwrap().1 - big.unwrap().0 + 1) * S >> 20, root_bytes >> 20, l.home.1 * S >> 20));
            Some(l)
        }
        Some(Err(e)) => {
            gate.fail("G5", "size", &e);
            None
        }
        None => {
            gate.fail("G5", "size", "no free space on the disk");
            None
        }
    };
    // G6: the ESP.
    let sdisk = format!("/dev/{}", bdisk);
    let src_esp = Dev::open_window(&sdisk, esp_part.start, esp_part.size, false).map_err(|e| format!("boot ESP: {}", e))?;
    let efi_name = crate::conf::EFI_NAME;
    let limine_bin = {
        let fs = fatfs::FileSystem::new(src_esp.handle(), fatfs::FsOptions::new()).map_err(|e| format!("boot ESP: {}", e))?;
        let mut f = fs.root_dir().open_file(&format!("EFI/BOOT/{}", efi_name)).map_err(|e| format!("boot ESP {}: {}", efi_name, e))?;
        let mut v = Vec::new();
        f.read_to_end(&mut v).map_err(|e| e.to_string())?;
        v
    };
    match &esp_ent {
        None => gate.fail("G6", "ESP", &format!("{} EFI system partitions on the target (need exactly 1)", esps.len())),
        Some(e) => {
            let d = Dev::open_window(&target, e.first * S, (e.last - e.first + 1) * S, false).map_err(|e| e.to_string())?;
            match esp::probe_shared(&d) {
                Err(er) => gate.fail("G6", "ESP", &format!("cannot read the ESP: {}", er)),
                Ok(p) if !p.fat32 => gate.fail("G6", "ESP", "the ESP is not FAT32"),
                Ok(p) if p.dirty => gate.fail("G6", "ESP", "the ESP has its dirty bit set (fsck it first)"),
                Ok(p) if p.has_ours => gate.fail("G6", "ESP", "LeandrOS files already exist on the ESP (repair flow: leandros-install repair-esp)"),
                Ok(p) if !p.has_entries_dir => gate.fail("G6", "ESP", "no /loader/entries (not a systemd-boot ESP)"),
                Ok(p) if p.free_bytes < MIB + limine_bin.len() as u64 => gate.fail("G6", "ESP", "not enough free space on the ESP"),
                Ok(p) => gate.pass("G6", "ESP", &format!("FAT32, clean, systemd-boot layout, {} KiB free, no LeandrOS files yet", p.free_bytes >> 10)),
            }
        }
    }
    // G7: backup space on the stick.
    let esp_bytes = esp_ent.as_ref().map_or(0, |e| (e.last - e.first + 1) * S);
    let backup_dir = format!("/home/dualboot-backup/bk-{}", mkhome::now());
    match free_bytes("/home") {
        Some(f) if f >= esp_bytes + 2 * MIB + 2 * GIB => gate.pass("G7", "backup space", &format!("{} MiB free in /home", f >> 20)),
        Some(f) => gate.fail("G7", "backup space", &format!("{} MiB free in /home, need {} MiB", f >> 20, (esp_bytes + 2 * MIB + 2 * GIB) >> 20)),
        None => gate.fail("G7", "backup space", "cannot stat /home"),
    }
    // G8: power.
    if on_battery() {
        gate.fail("G8", "power", "running on battery; connect AC power");
    } else {
        gate.pass("G8", "power", "AC online or no battery information");
    }
    gate.na("G3", "Linux clean", "n/a in free-space mode: the Linux filesystem is not touched");
    gate.na("G4", "fsck", "n/a in free-space mode");
    gate.na("G9", "rehearsal", "n/a in free-space mode");
    gate.na("G10", "NVMe write qualification", "n/a: writes are confined by BLKWRWIN to the gap, the GPT areas and the ESP");
    gate.na("G11", "verify", "n/a in free-space mode");
    gate.na("G12", "checkpoint", "n/a in free-space mode");
    if !gate.failed.is_empty() {
        return Err(format!("preflight failed: {}", gate.failed.join(",")));
    }
    let layout = layout.unwrap();
    let esp_ent = esp_ent.unwrap();
    let nvme = plan::bus_of(&tname) == "nvme";

    // Plan: the exact new GPT.
    let mut guids = [[0u8; 16]; 4];
    for x in guids.iter_mut() {
        *x = gpt::random_guid().map_err(|e| e.to_string())?;
    }
    let (boot_pu, root_pu, home_pu, log_pu) = (gpt::guid_text(&guids[0]), gpt::guid_text(&guids[1]), gpt::guid_text(&guids[2]), gpt::guid_text(&guids[3]));
    let rw = format!("blk.rw=PARTUUID={},PARTUUID={},PARTUUID={},PARTUUID={}", root_pu, home_pu, log_pu, boot_pu);
    let mut ng = g.clone();
    let add = |ng: &mut Gpt, t, guid, r: (u64, u64), name: &str| ng.add(t, guid, r.0, r.0 + r.1 - 1, name, gpt::ATTR_NO_AUTO);
    add(&mut ng, gpt::LBOOT_TYPE, guids[0], layout.boot, "LEANDROS boot")?;
    add(&mut ng, gpt::LROOT_TYPE, guids[1], layout.root, "LEANDROS root")?;
    add(&mut ng, gpt::LHOME_TYPE, guids[2], layout.home, "LEANDROS home")?;
    add(&mut ng, gpt::LLOG_TYPE, guids[3], layout.log, "LEANDROS-LOG")?;
    if let Some(sb_) = &sessb {
        let (old, new) = (g_disk.entry(sb_.slot).unwrap(), g.entry(sb_.slot).unwrap());
        let fmt = |e: &gpt::Entry| format!("{}..{} ({} sectors) {}", e.first, e.last, e.last - e.first + 1, gpt::guid_text(&e.guid));
        out.event(
            &format!("{{\"ev\":\"shrink_gpt\",\"slot\":{},\"old_last\":{},\"new_last\":{},\"first\":{}}}", sb_.slot + 1, old.last, new.last, old.first),
            &format!("p{} entry:\n  old: {}\n  new: {}", sb_.slot + 1, fmt(&old), fmt(&new)),
        );
    }
    let shared = esp::shared_files(efi_name, &limine_bin, &boot_pu);
    let esp_list: Vec<String> = shared.iter().map(|(p, d)| format!("{{\"path\":\"/{}\",\"bytes\":{}}}", esc(p), d.len())).collect();
    out.event(
        &format!(
            "{{\"ev\":\"plan\",\"mode\":\"dualboot\",\"target\":\"{}\",\"bus\":\"{}\",\"size\":{},\"disk_guid\":\"{}\",\"root_partuuid\":\"{}\",\"home_partuuid\":\"{}\",\"log_partuuid\":\"{}\",\"boot_partuuid\":\"{}\",\"cmdline_rw\":\"{}\",\"backup_dir\":\"{}\",\"old_gpt\":{},\"new_gpt\":{},\"esp_files\":[{}],\"f2fs_bytes\":{}}}",
            esc(&target), plan::bus_of(&tname), size, gpt::guid_text(&g.disk_guid), root_pu, home_pu, log_pu, boot_pu, esc(&rw), esc(&backup_dir), gpt_json(&g_disk), gpt_json(&ng), esp_list.join(","), sb.bytes()
        ),
        &format!(
            "plan: {} ({} MiB, {})\nold GPT:\n  {}\nnew GPT:\n  {}\nESP files to create:\n  {}\ncmdline: root=PARTUUID={} {}\nbackup: {}",
            target, size >> 20, plan::bus_of(&tname), gpt_lines(&g_disk).join("\n  "), gpt_lines(&ng).join("\n  "),
            shared.iter().map(|(p, d)| format!("/{} ({} bytes)", p, d.len())).collect::<Vec<_>>().join("\n  "), root_pu, rw, backup_dir
        ),
    );
    if o.dry_run {
        let (old, new) = esp::preview_conf_rw(&src_esp, &root_pu, &rw).map_err(|e| format!("boot ESP limine.conf: {}", e))?;
        out.event(
            &format!("{{\"ev\":\"conf\",\"old\":\"{}\",\"new\":\"{}\"}}", esc(&old), esc(&new)),
            &format!("boot partition limine.conf after rewrite:\n{}", new),
        );
        out.event("{\"ev\":\"done\",\"dry_run\":true}", "dry run: nothing written");
        return Ok(());
    }
    if !o.yes {
        return Err("refusing to write without --yes".into());
    }
    let _ = nvme;

    // Backups first.
    out.step("backup", 0);
    let files = backup(&target, &g_disk, esp_ent.first, esp_ent.last - esp_ent.first + 1, &backup_dir, &out)?;
    out.info(&format!("backup: {} ({})", backup_dir, files.join(", ")));

    // The write window: GPT areas and the gap only.
    let anchor = OpenOptions::new().read(true).write(true).open(&target).map_err(|e| format!("{}: {}", target, e))?;
    let gap = big.unwrap();
    if nvme {
        crate::install::enable_nvme_write(&target)?;
    }
    let w1 = [(1, g.first_usable - 1), (g.last_usable + 1, total - g.last_usable - 1), (gap.0, gap.1 - gap.0 + 1)];
    set_window(&anchor, &w1)?;
    // Re-read the table under the window; it must be what we planned from.
    let mut anchor_rw = anchor.try_clone().map_err(|e| e.to_string())?;
    let g2 = Gpt::read(&mut anchor_rw, 512, total)?;
    if gpt_lines(&g2) != gpt_lines(&g_disk) {
        return Err("the partition table changed since the preflight".into());
    }
    let io = |e: std::io::Error| e.to_string();

    out.step("gpt", 0);
    if let Some(sb_) = &sessb {
        // Checkpoint: the shrunk-p2 table alone (backup copy first, then primary, then verified).
        // The disk is valid and the freed gap usable if anything fails after this point.
        if sb_.new_last < sb_.old_last {
            g.write(&mut anchor_rw)?;
            anchor_rw.sync_all().map_err(io)?;
        }
    }
    ng.write(&mut anchor_rw)?;
    anchor_rw.sync_all().map_err(io)?;
    out.step("gpt", 100);

    // Boot partition: the stick's ESP layout plus the owner marker.
    out.step("boot", 0);
    let dst = Dev::open_window(&target, layout.boot.0 * S, layout.boot.1 * S, true).map_err(io)?;
    let marker = vec![("leandros/owner".to_string(), format!("leandros-boot {}\n", boot_pu).into_bytes())];
    let r = esp::install_esp_ext(&src_esp, &dst, &root_pu, Some(&rw), *b"LEANDROSBT ", &marker, |p| out.step("boot", p)).map_err(|e| format!("boot partition: {}", e))?;
    out.info(&format!("boot: {} files, {} bytes, rewrote {}", r.files, r.bytes, r.confs.join(",")));
    drop(dst);

    out.step("log", 0);
    let zeros = vec![0u8; 4096];
    let slot = (layout.log.1 * S / 4).min(16 << 20) & !4095;
    for s in 0..4u64 {
        anchor_rw.seek(SeekFrom::Start(layout.log.0 * S + s * slot)).map_err(io)?;
        anchor_rw.write_all(&zeros).map_err(io)?;
    }
    anchor_rw.sync_all().map_err(io)?;
    out.step("log", 100);

    out.step("home", 0);
    let huuid = mkhome::random_uuid().map_err(io)?;
    mkhome::write_home_at(&mut anchor_rw, layout.home.0 * S, layout.home.1 * S, &huuid, mkhome::now()).map_err(|e| format!("home: {}", e))?;
    anchor_rw.sync_all().map_err(io)?;
    out.step("home", 100);

    out.step("root", 0);
    out.info("System files are read-only while copying");
    let mut guard = copy::RoGuard::new().map_err(|e| format!("remount / read-only: {}", e))?;
    let cr = copy::copy_root(&sdisk, root_part.start, &target, layout.root.0 * S, &sb, o.zero_fill, |p| out.step("root", p));
    let rwr = guard.release();
    let cr = cr.map_err(|e| format!("root copy: {}", e))?;
    rwr.map_err(|e| format!("remount / read-write: {}", e))?;
    out.info(&format!("root: wrote {} MiB, skipped {} MiB of zeros", cr.written >> 20, cr.skipped >> 20));
    if !o.no_verify {
        out.step("verify", 0);
        copy::verify_root(&target, layout.root.0 * S, sb.bytes(), &cr.sha256, |p| out.step("verify", p)).map_err(|e| e.to_string())?;
        out.step("verify", 100);
    }
    anchor_rw.sync_all().map_err(io)?;
    copy::flush_dev(&anchor_rw);

    // Last: the ESP, with a window that covers only the ESP.
    out.step("esp", 0);
    set_window(&anchor, &[(esp_ent.first, esp_ent.last - esp_ent.first + 1)])?;
    let edev = Dev::open_window(&target, esp_ent.first * S, (esp_ent.last - esp_ent.first + 1) * S, true).map_err(io)?;
    esp::write_shared(&edev, &shared).map_err(|e| format!("shared ESP: {}", e))?;
    drop(edev);
    anchor_rw.sync_all().map_err(io)?;
    copy::flush_dev(&anchor_rw);
    set_window(&anchor, &[])?;
    out.step("esp", 100);

    if let Some(sb_) = &sessb {
        crate::shrink::finish_b(&sb_.dir);
    }
    out.event(
        &format!(
            "{{\"ev\":\"done\",\"target\":\"{}\",\"mode\":\"dualboot\",\"root_partuuid\":\"{}\",\"boot_partuuid\":\"{}\",\"disk_guid\":\"{}\",\"root_sha256\":\"{}\",\"backup_dir\":\"{}\"}}",
            esc(&target), root_pu, boot_pu, gpt::guid_text(&g.disk_guid), copy::hex(&cr.sha256), esc(&backup_dir)
        ),
        &format!("done: root PARTUUID {} boot PARTUUID {} (root sha256 {}) backup {}", root_pu, boot_pu, copy::hex(&cr.sha256), backup_dir),
    );
    Ok(())
}

fn unsafe_euid_root() -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_in_gap() {
        // 24 GiB gap starting at 1 MiB-aligned sector 2048.
        let gap = (2048u64, 2048 + 24 * 2097152 - 1);
        let l = compute(gap, 16 * GIB).unwrap();
        assert_eq!(l.boot.0, 2048);
        assert_eq!(l.root.0, l.boot.0 + l.boot.1);
        assert_eq!(l.home.0, l.root.0 + l.root.1);
        assert_eq!(l.log.0 + l.log.1, gap.1 + 1);
        assert_eq!(l.home.0 + l.home.1, l.log.0);
        for p in [l.boot, l.root, l.home, l.log] {
            assert_eq!(p.0 % 2048, 0);
        }
    }

    #[test]
    fn gap_too_small() {
        assert!(compute((2048, 2048 + 10 * 2097152 - 1), 16 * GIB).is_err());
    }

    #[test]
    fn eligibility_reasons() {
        let big = [(2048u64, 2048 + 24 * 2097152 - 1)];
        assert!(eligibility(false, false, 1 << 40, true, &big).0);
        assert_eq!(eligibility(true, false, 1 << 40, true, &big).1, "boot disk");
        assert!(!eligibility(false, false, 1 << 40, false, &big).0);
        assert!(!eligibility(false, false, 1 << 40, true, &[]).0);
        assert!(!eligibility(false, false, 1 << 40, true, &[(2048, 2048 + 4096)]).0);
    }
}
