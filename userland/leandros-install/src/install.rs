//! `list` and `install`: the privileged backend flow.

use crate::copy;
use crate::dev::Dev;
use crate::disks::{self, Kind};
use crate::esp;
use crate::gpt;
use crate::mkhome;
use crate::plan::{self, DiskInfo, Layout, GIB, MIB};
use crate::progress::{esc, Out};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

#[derive(Default, Clone)]
pub struct Opts {
    pub target: Option<String>,
    pub yes: bool,
    pub dry_run: bool,
    pub root_gib: Option<u64>,
    pub allow_removable: bool,
    pub no_verify: bool,
    pub zero_fill: bool,
    pub json: bool,
    pub dualboot: bool,
    pub use_free: bool,
}

pub fn list(o: &Opts) {
    let out = Out { json: o.json };
    for d in plan::gather(o.allow_removable, plan::MIN_ROOT_BYTES) {
        out.event(&d.json(), &d.human());
    }
}

/// NVMe is read-only unless `nvme.rw=1` is on the cmdline. The kernel (lane
/// nvmerw) lets root enable writes for this boot only: open the whole disk
/// O_RDONLY, BLKROSET(0), then reopen O_RDWR (partition nodes are cleared
/// too). Non-NVMe disks return EINVAL, so call this for NVMe targets only.
pub fn enable_nvme_write(dev: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let f = OpenOptions::new().read(true).open(dev).map_err(|e| format!("{}: {}", dev, e))?;
        let zero: libc::c_int = 0;
        const BLKROSET: libc::c_ulong = 0x125D;
        let r = unsafe { libc::ioctl(f.as_raw_fd(), BLKROSET as _, &zero as *const libc::c_int) };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            let cmd = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
            if cmd.split_whitespace().any(|t| t == "nvme.rw=1") {
                return Ok(());
            }
            return Err(format!("cannot enable writes on {} ({}); NVMe is read-only without nvme.rw=1", dev, e));
        }
    }
    let _ = dev;
    Ok(())
}

fn euid_is_root() -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

fn round_up(v: u64, to: u64) -> u64 {
    (v + to - 1) / to * to
}

pub fn install(o: &Opts) -> Result<(), String> {
    if o.dualboot {
        return crate::dualboot::install(o);
    }
    let out = Out { json: o.json };
    if !o.dry_run && !euid_is_root() {
        return Err("must run as root".into());
    }
    let target = o.target.clone().ok_or("--target /dev/X is required")?;
    let tname = target.strip_prefix("/dev/").unwrap_or(&target).to_string();
    let names = disks::whole_disks();
    if !names.contains(&tname) {
        return Err(format!("{} is not a whole disk", target));
    }
    // Boot stick: its root partition and ESP.
    let (bdisk, bpno) = plan::boot_root_part(&names)
        .ok_or("cannot identify the boot disk (no root=PARTUUID= matching a disk on the command line)")?;
    let bparts = disks::scan_disk(&bdisk);
    let root_part = bparts.iter().find(|p| p.pno == bpno).ok_or("boot root partition not found")?.clone();
    let esp_part = plan::find_part(&bparts, Kind::Esp).ok_or("the boot disk has no ESP")?.clone();
    let sb = copy::read_sb(&format!("/dev/{}", bdisk), root_part.start).map_err(|e| format!("boot root superblock: {}", e))?;
    let root_bytes = match o.root_gib {
        Some(g) => g * GIB,
        None => plan::MIN_ROOT_BYTES.max(round_up(sb.bytes() + GIB, MIB)),
    };
    if root_bytes < sb.bytes() {
        return Err(format!("--root-gib too small for the {} MiB root volume", sb.bytes() >> 20));
    }
    // Eligibility (hard refusals, no override).
    let info: DiskInfo = plan::gather(o.allow_removable, root_bytes)
        .into_iter()
        .find(|d| d.name == tname)
        .ok_or("target disk vanished")?;
    if !info.eligible {
        return Err(format!("refusing {}: {}", target, info.reason));
    }
    let layout = Layout::compute(info.size, root_bytes)?;
    let nvme = info.bus == "nvme";
    let mut guids = [[0u8; 16]; 4];
    for g in guids.iter_mut() {
        *g = gpt::random_guid().map_err(|e| e.to_string())?;
    }
    let disk_guid = gpt::random_guid().map_err(|e| e.to_string())?;
    let root_pu = gpt::guid_text(&guids[1]);
    let home_bytes = layout.home.1 * gpt::SECTOR;

    out.event(
        &format!(
            "{{\"ev\":\"plan\",\"target\":\"{}\",\"bus\":\"{}\",\"size\":{},\"root_partuuid\":\"{}\",\"esp_start\":{},\"esp_sectors\":{},\"root_start\":{},\"root_sectors\":{},\"home_start\":{},\"home_sectors\":{},\"log_start\":{},\"log_sectors\":{},\"f2fs_bytes\":{},\"nvme_rw\":{}}}",
            esc(&target), info.bus, info.size, root_pu, layout.esp.0, layout.esp.1, layout.root.0, layout.root.1,
            layout.home.0, layout.home.1, layout.log.0, layout.log.1, sb.bytes(), nvme
        ),
        &format!(
            "plan: {} ({} MiB, {})\n  p1 ESP  {} MiB @ sector {}\n  p2 root {} MiB @ sector {} (f2fs volume {} MiB)\n  p3 log  {} MiB @ sector {}\n  p4 home {} MiB @ sector {}\n  root PARTUUID {}",
            target, info.size >> 20, info.bus, layout.esp.1 / 2048, layout.esp.0, layout.root.1 / 2048, layout.root.0, sb.bytes() >> 20,
            layout.log.1 / 2048, layout.log.0, layout.home.1 / 2048, layout.home.0, root_pu
        ),
    );

    let sdisk = format!("/dev/{}", bdisk);
    let src_esp = Dev::open_window(&sdisk, esp_part.start, esp_part.size, false).map_err(|e| format!("boot ESP: {}", e))?;
    if o.dry_run {
        let (old, new) = esp::preview_conf(&src_esp, &root_pu, nvme).map_err(|e| format!("boot ESP limine.conf: {}", e))?;
        out.event(
            &format!("{{\"ev\":\"conf\",\"old\":\"{}\",\"new\":\"{}\"}}", esc(&old), esc(&new)),
            &format!("limine.conf after rewrite:\n{}", new),
        );
        out.event("{\"ev\":\"done\",\"dry_run\":true}", "dry run: nothing written");
        return Ok(());
    }
    if !o.yes {
        return Err("refusing to write without --yes".into());
    }

    if nvme {
        enable_nvme_write(&target)?;
    }
    let mut disk = OpenOptions::new().read(true).write(true).open(&target).map_err(|e| format!("{}: {}", target, e))?;
    let io = |e: std::io::Error| e.to_string();

    // 1. GPT
    out.step("gpt", 0);
    let zeros = vec![0u8; MIB as usize];
    disk.seek(SeekFrom::Start(0)).map_err(io)?;
    disk.write_all(&zeros).map_err(io)?;
    let t = gpt::build(layout.total, &disk_guid, &layout.specs(&guids));
    disk.seek(SeekFrom::Start(0)).map_err(io)?;
    disk.write_all(&t.mbr).map_err(io)?;
    disk.write_all(&t.primary_hdr).map_err(io)?;
    disk.write_all(&t.entries).map_err(io)?;
    disk.seek(SeekFrom::Start((layout.total - 33) * gpt::SECTOR)).map_err(io)?;
    disk.write_all(&t.entries).map_err(io)?;
    disk.write_all(&t.backup_hdr).map_err(io)?;
    disk.sync_all().map_err(io)?;
    out.step("gpt", 100);

    // 2. ESP
    out.step("esp", 0);
    let dst_esp = Dev::open_window(&target, layout.esp.0 * gpt::SECTOR, layout.esp.1 * gpt::SECTOR, true).map_err(io)?;
    let r = esp::install_esp(&src_esp, &dst_esp, &root_pu, nvme, |p| out.step("esp", p)).map_err(|e| format!("ESP: {}", e))?;
    out.info(&format!("ESP: {} files, {} bytes, rewrote {}", r.files, r.bytes, r.confs.join(",")));

    // 3. log partition: zero the slot header pages
    out.step("log", 0);
    let log_bytes = layout.log.1 * gpt::SECTOR;
    let slot = (log_bytes / 4).min(16 << 20) & !4095;
    for s in 0..4u64 {
        disk.seek(SeekFrom::Start(layout.log.0 * gpt::SECTOR + s * slot)).map_err(io)?;
        disk.write_all(&zeros[..4096]).map_err(io)?;
    }
    disk.sync_all().map_err(io)?;
    out.step("log", 100);

    // 4. home
    out.step("home", 0);
    let huuid = mkhome::random_uuid().map_err(io)?;
    mkhome::write_home_at(&mut disk, layout.home.0 * gpt::SECTOR, home_bytes, &huuid, mkhome::now()).map_err(|e| format!("home: {}", e))?;
    disk.sync_all().map_err(io)?;
    out.step("home", 100);

    // 5. root: block copy of the live volume, bracketed by remount ro/rw
    out.step("root", 0);
    out.info("System files are read-only while copying");
    let mut guard = copy::RoGuard::new().map_err(|e| format!("remount / read-only: {}", e))?;
    let cr = copy::copy_root(&sdisk, root_part.start, &target, layout.root.0 * gpt::SECTOR, &sb, o.zero_fill, |p| out.step("root", p));
    let rw = guard.release();
    let cr = cr.map_err(|e| format!("root copy: {}", e))?;
    rw.map_err(|e| format!("remount / read-write: {}", e))?;
    out.info(&format!("root: wrote {} MiB, skipped {} MiB of zeros", cr.written >> 20, cr.skipped >> 20));

    // 6. verify
    if !o.no_verify {
        out.step("verify", 0);
        copy::verify_root(&target, layout.root.0 * gpt::SECTOR, sb.bytes(), &cr.sha256, |p| out.step("verify", p)).map_err(|e| e.to_string())?;
        out.step("verify", 100);
    }
    disk.sync_all().map_err(io)?;
    copy::flush_dev(&disk);
    out.event(
        &format!(
            "{{\"ev\":\"done\",\"target\":\"{}\",\"root_partuuid\":\"{}\",\"disk_guid\":\"{}\",\"root_sha256\":\"{}\"}}",
            esc(&target), root_pu, gpt::guid_text(&disk_guid), copy::hex(&cr.sha256)
        ),
        &format!("done: root PARTUUID {} (root sha256 {})", root_pu, copy::hex(&cr.sha256)),
    );
    Ok(())
}
