//! `espctl pi ...`: the real-device side of picard.rs (vdc, reboot(2), /proc).

use crate::picard::*;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;

const VDC: &str = "/dev/vdc";
const BLKWRWIN: libc::c_ulong = 0x4108_12F0;
const BLKFLSBUF: libc::c_ulong = 0x1261;
const BLKISBOOTFAT: libc::c_ulong = 0x8004_12F1;
const MAGIC1: libc::c_long = 0xfee1_dead_u32 as libc::c_long;
const MAGIC2: libc::c_long = 672274793;
const CMD_RESTART: libc::c_long = 0x0123_4567;
const CMD_RESTART2: libc::c_long = 0xA1B2_C3D4_u32 as libc::c_long;
// Assumed L1 interface (kernel/src/power.rs in lane/pitrykern):
const CMD_CONFIRM: libc::c_long = 0x5442_4F4B;
const CMD_HOLD: libc::c_long = 0x5442_4844; // arg != 0: hold the deadline, 0: release

pub struct Vdc {
    path: String,
    rd: File,
    wr: Option<File>,
}
/// True only when the kernel says `path` is the Pi boot-FAT device (BLKISBOOTFAT == 1).
/// Any failure (old kernel, not a block device) counts as "no".
pub fn is_boot_fat(path: &str) -> bool {
    let Ok(f) = File::open(path) else { return false };
    let mut v: i32 = 0;
    unsafe { libc::ioctl(f.as_raw_fd(), BLKISBOOTFAT as _, &mut v as *mut i32) == 0 && v == 1 }
}

impl Vdc {
    pub fn open(path: &str) -> io::Result<Vdc> {
        Ok(Vdc { path: path.into(), rd: File::open(path)?, wr: None })
    }
}
impl Disk for Vdc {
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        self.rd.read_exact_at(buf, lba * 512)
    }
    fn write(&mut self, lba: u64, data: &[u8]) -> io::Result<()> {
        let w = self.wr.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no write window"))?;
        // 512 B pieces first up to 4 KiB alignment, then bulk, then the tail:
        // the kernel's sector-exact path takes exactly 512 B writes.
        let mut off = lba * 512;
        let mut d = data;
        while !d.is_empty() && (d.len() < 4096 || off % 4096 != 0) {
            w.write_all_at(&d[..512], off)?;
            off += 512;
            d = &d[512..];
        }
        if !d.is_empty() {
            w.write_all_at(d, off)?;
        }
        Ok(())
    }
    fn allow(&mut self, r: &[(u64, u64)]) -> io::Result<()> {
        #[repr(C)]
        struct WrWin { count: u32, flags: u32, r: [[u64; 2]; 16] }
        let f = OpenOptions::new().read(true).write(true).open(&self.path)?;
        let mut w = WrWin { count: r.len() as u32, flags: 0, r: [[0; 2]; 16] };
        for (i, x) in r.iter().enumerate() {
            w.r[i] = [x.0, x.1];
        }
        if unsafe { libc::ioctl(f.as_raw_fd(), BLKWRWIN as _, &w as *const WrWin) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.wr = Some(f); // keeps the window alive
        Ok(())
    }
    fn fresh(&mut self) -> io::Result<()> {
        unsafe { libc::ioctl(self.rd.as_raw_fd(), BLKFLSBUF as _, 0) }; // best effort
        self.rd = File::open(&self.path)?;
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(w) = &self.wr {
            w.sync_all()?;
        }
        Ok(())
    }
}

pub struct Real;
fn reboot(cmd: libc::c_long, arg: *const libc::c_char) -> io::Result<()> {
    let r = unsafe { libc::syscall(libc::SYS_reboot, MAGIC1, MAGIC2, cmd, arg) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
impl Sys for Real {
    fn has_word(&self, w: &str) -> bool {
        // exit-status style: the command line is never printed
        std::fs::read_to_string("/proc/cmdline").map_or(false, |c| c.split_whitespace().any(|t| t == w))
    }
    fn tag(&self) -> String {
        let mut u: libc::utsname = unsafe { std::mem::zeroed() };
        unsafe { libc::uname(&mut u) };
        let v: Vec<u8> = u.version.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        String::from_utf8_lossy(&v).into_owned()
    }
    fn hold(&self) -> io::Result<()> {
        reboot(CMD_HOLD, 1 as *const libc::c_char)
    }
    fn confirm(&self) -> io::Result<()> {
        reboot(CMD_CONFIRM, std::ptr::null())
    }
    fn reboot_try(&self) -> io::Result<()> {
        unsafe { libc::sync() };
        reboot(CMD_RESTART2, b"0 tryboot\0".as_ptr() as *const libc::c_char)
    }
    fn reboot_plain(&self) -> io::Result<()> {
        unsafe { libc::sync() };
        reboot(CMD_RESTART, std::ptr::null())
    }
    fn good_initrd(&self) -> io::Result<Vec<u8>> {
        std::fs::read("/boot/pi/good/initrd")
    }
    fn now(&self) -> String {
        String::new()
    }
}

const USAGE: &str = "usage: espctl pi <status|setup|stage|try|commit|abort|rollback> [--dev /dev/vdc] [--dry-run]
  setup  [--kernel-size B] [--initrd-size B] [--accept-root-sector-risk]
  stage  <0|1|2|auto> --kernel F --initrd F --tag T [--sha256 H] [--initrd-sha256 H]
  commit [--skip-hold]";

pub fn run(mut a: Vec<String>, dev: Option<String>) -> io::Result<()> {
    let opt = |a: &mut Vec<String>, n: &str| -> Option<String> {
        let i = a.iter().position(|x| x == n)?;
        let v = a.get(i + 1)?.clone();
        a.drain(i..i + 2);
        Some(v)
    };
    let flag = |a: &mut Vec<String>, n: &str| a.iter().position(|x| x == n).map(|i| a.remove(i)).is_some();
    if a.is_empty() {
        println!("{}", USAGE);
        return Ok(());
    }
    let sub = a.remove(0);
    let dry = flag(&mut a, "--dry-run");
    let dev = dev.unwrap_or_else(|| VDC.to_string());
    let bf = is_boot_fat(&dev);
    if !matches!(sub.as_str(), "status" | "setup" | "stage" | "try" | "commit" | "abort" | "rollback") {
        println!("{}", USAGE);
        return Ok(());
    }
    if sub == "status" {
        println!("boot-FAT device ({}): {}", dev, if bf { "yes" } else { "NO (writes refused)" });
    }
    write_guard(&sub, &dev, bf)?;
    let mut f = Fat::open(Vdc::open(&dev)?)?;
    f.dry = dry;
    let sys = Real;
    let out = match sub.as_str() {
        "status" => status(&mut f, &sys)?,
        "setup" => {
            let ksz = opt(&mut a, "--kernel-size").and_then(|v| v.parse().ok()).unwrap_or(DEF_KERNEL);
            let isz = opt(&mut a, "--initrd-size").and_then(|v| v.parse().ok()).unwrap_or(DEF_INITRD);
            setup(&mut f, &SetupOpts { ksz, isz, accept_root_risk: flag(&mut a, "--accept-root-sector-risk") })?
        }
        "stage" => {
            let kp = opt(&mut a, "--kernel");
            let ip = opt(&mut a, "--initrd");
            let tag = opt(&mut a, "--tag");
            let sha_k = opt(&mut a, "--sha256");
            let sha_i = opt(&mut a, "--initrd-sha256");
            let slot = a.first().cloned().ok_or_else(|| io::Error::new(io::ErrorKind::Other, USAGE))?;
            let (Some(kp), Some(ip), Some(tag)) = (kp, ip, tag) else { return Err(io::Error::new(io::ErrorKind::Other, USAGE)) };
            let slot = if slot == "auto" { None } else { Some(slot.parse::<u8>().map_err(|_| io::Error::new(io::ErrorKind::Other, "bad slot"))?) };
            let t = stage(&mut f, &sys, &StageOpts { slot, kernel: std::fs::read(kp)?, initrd: std::fs::read(ip)?, tag, sha_k, sha_i })?;
            format!("staged slot s{}{}", t, if dry { " (dry-run: nothing written)" } else { "" })
        }
        "try" => try_(&mut f, &sys, dry)?,
        "commit" => {
            let skip = flag(&mut a, "--skip-hold");
            commit(&mut f, &sys, skip)?
        }
        "abort" => {
            if dry { "would reboot plainly".to_string() } else { sys.reboot_plain()?; "rebooting".into() }
        }
        "rollback" => rollback(&mut f)?,
        _ => {
            println!("{}", USAGE);
            return Ok(());
        }
    };
    println!("{}{}", out, if dry && sub != "stage" { "  [dry-run]" } else { "" });
    Ok(())
}

