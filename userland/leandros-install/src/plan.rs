//! Disk enumeration, boot-stick detection, eligibility and the target layout.

use crate::disks::{self, Kind, Part};
use crate::gpt::{self, PartSpec};
use crate::progress::esc;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;
pub const ESP_BYTES: u64 = 512 * MIB;
pub const LOG_BYTES: u64 = 64 * MIB;
pub const MIN_HOME_BYTES: u64 = 2 * GIB;
pub const MIN_ROOT_BYTES: u64 = 16 * GIB;

#[derive(Clone, Debug)]
pub struct DiskInfo {
    pub name: String,
    pub dev: String,
    pub size: u64,
    pub bus: &'static str,
    pub model: String,
    pub removable: bool,
    pub ro: bool,
    pub boot: bool,
    pub mounted: bool,
    pub parts: Vec<Part>,
    pub eligible: bool,
    pub reason: String,
    /// Free gaps (first, last inclusive, 512-byte sectors).
    pub gaps: Vec<(u64, u64)>,
    pub dualboot: bool,
    pub dualboot_reason: String,
}

pub fn read_to_string(p: &str) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn sys_u64(disk: &str, leaf: &str) -> Option<u64> {
    read_to_string(&format!("/sys/class/block/{}/{}", disk, leaf)).trim().parse().ok()
}

pub fn bus_of(name: &str) -> &'static str {
    if name.starts_with("nvme") {
        "nvme"
    } else if name.starts_with("vd") {
        "virtio"
    } else if name.starts_with("sd") {
        "sd"
    } else if name.starts_with("mmcblk") {
        "mmc"
    } else {
        "other"
    }
}

fn model_of(name: &str) -> String {
    for leaf in ["device/model", "device/name"] {
        let m = read_to_string(&format!("/sys/class/block/{}/{}", name, leaf));
        if !m.trim().is_empty() {
            return m.trim().to_string();
        }
    }
    String::new()
}

pub fn disk_size(name: &str) -> u64 {
    if let Some(s) = sys_u64(name, "size") {
        if s > 0 {
            return s * 512;
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(f) = File::open(format!("/dev/{}", name)) {
        use std::os::unix::io::AsRawFd;
        let mut sz: u64 = 0;
        const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
        if unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64 as _, &mut sz as *mut u64) } == 0 {
            return sz;
        }
    }
    0
}

/// PARTUUID text of every partition of `disk`: GPT unique GUIDs, or the MBR
/// `<sig>-<nn>` form. (pno, text)
pub fn part_uuids(disk: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut f = match File::open(format!("/dev/{}", disk)) {
        Ok(f) => f,
        Err(_) => return out,
    };
    let mut mbr = [0u8; 512];
    if f.read_exact(&mut mbr).is_err() || mbr[510] != 0x55 || mbr[511] != 0xAA {
        return out;
    }
    if (0..4).any(|i| mbr[446 + i * 16 + 4] == 0xEE) {
        let mut hdr = [0u8; 512];
        if f.seek(SeekFrom::Start(512)).is_err() || f.read_exact(&mut hdr).is_err() || &hdr[0..8] != b"EFI PART" {
            return out;
        }
        let ent_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = (u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize).min(256);
        let esz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        if esz < 128 || esz > 4096 {
            return out;
        }
        let mut ents = vec![0u8; n * esz];
        if f.seek(SeekFrom::Start(ent_lba * 512)).is_err() || f.read_exact(&mut ents).is_err() {
            return out;
        }
        for i in 0..n {
            let e = &ents[i * esz..i * esz + esz];
            if e[0..16] == [0u8; 16] {
                continue;
            }
            let g: [u8; 16] = e[16..32].try_into().unwrap();
            out.push((i as u32 + 1, gpt::guid_text(&g)));
        }
    } else {
        let sig = u32::from_le_bytes(mbr[440..444].try_into().unwrap());
        for i in 0..4 {
            if mbr[446 + i * 16 + 4] != 0 {
                out.push((i as u32 + 1, format!("{:08x}-{:02x}", sig, i + 1)));
            }
        }
    }
    out
}

/// `root=` value of this boot ("PARTUUID=..."), if any.
pub fn boot_root_spec() -> Option<String> {
    crate::conf::cmdline_value(&read_to_string("/proc/cmdline"), "root")
}

/// (disk, partition number) of this boot's root partition, matched by PARTUUID.
pub fn boot_root_part(disks: &[String]) -> Option<(String, u32)> {
    let spec = boot_root_spec()?;
    let want = spec.strip_prefix("PARTUUID=")?.to_ascii_lowercase();
    for d in disks {
        for (pno, u) in part_uuids(d) {
            if u == want {
                return Some((d.clone(), pno));
            }
        }
    }
    None
}

/// Whole disks that hold a partition named as a mount source in /proc/mounts.
pub fn mounted_disks(disks: &[String]) -> Vec<String> {
    let mounts = read_to_string("/proc/mounts");
    let mut out = Vec::new();
    for d in disks {
        let pre = format!("/dev/{}", d);
        let hit = mounts.lines().filter_map(|l| l.split_whitespace().next()).any(|src| {
            src == pre
                || src.strip_prefix(pre.as_str()).map_or(false, |r| {
                    let r = r.strip_prefix('p').unwrap_or(r);
                    !r.is_empty() && r.bytes().all(|c| c.is_ascii_digit())
                })
        });
        if hit {
            out.push(d.clone());
        }
    }
    out
}

pub fn min_disk_bytes(root_bytes: u64) -> u64 {
    ESP_BYTES + MIB + root_bytes + MIN_HOME_BYTES + LOG_BYTES + MIB
}

pub fn gather(allow_removable: bool, root_bytes: u64) -> Vec<DiskInfo> {
    let names = disks::whole_disks();
    let boot = boot_root_part(&names).map(|(d, _)| d);
    let mounted = mounted_disks(&names);
    let mut v = Vec::new();
    for n in &names {
        let size = disk_size(n);
        let removable = sys_u64(n, "removable").unwrap_or(0) != 0;
        let ro = sys_u64(n, "ro").unwrap_or(0) != 0;
        let bus = bus_of(n);
        let is_boot = boot.as_deref() == Some(n.as_str());
        let is_mounted = mounted.contains(n);
        let reason = if is_boot {
            "boot disk"
        } else if is_mounted {
            "has mounted partitions"
        } else if size == 0 {
            "unknown size"
        } else if size < min_disk_bytes(root_bytes) {
            "too small"
        } else if ro && bus != "nvme" {
            "read-only"
        } else if removable && !allow_removable {
            "removable"
        } else {
            ""
        };
        let parts = disks::scan_disk(n);
        let gaps = if is_boot || is_mounted { Vec::new() } else { crate::dualboot::gaps_of(&format!("/dev/{}", n), size) };
        let (dualboot, dualboot_reason) =
            crate::dualboot::eligibility(is_boot, is_mounted, size, parts.iter().any(|p| p.kind == Kind::Esp), &gaps);
        v.push(DiskInfo {
            name: n.clone(),
            dev: format!("/dev/{}", n),
            size,
            bus,
            model: model_of(n),
            removable,
            ro,
            boot: is_boot,
            mounted: is_mounted,
            parts,
            eligible: reason.is_empty(),
            reason: reason.to_string(),
            gaps,
            dualboot,
            dualboot_reason,
        });
    }
    v
}

impl DiskInfo {
    pub fn json(&self) -> String {
        let parts: Vec<String> = self
            .parts
            .iter()
            .map(|p| format!("{{\"n\":{},\"kind\":\"{}\",\"size\":{}}}", p.pno, esc(&p.kind.name()), p.size))
            .collect();
        let gaps: Vec<String> = self.gaps.iter().map(|g| format!("{{\"start\":{},\"sectors\":{}}}", g.0, g.1 - g.0 + 1)).collect();
        format!(
            "{{\"ev\":\"disk\",\"name\":\"{}\",\"dev\":\"{}\",\"size\":{},\"bus\":\"{}\",\"model\":\"{}\",\"removable\":{},\"ro\":{},\"boot\":{},\"eligible\":{},\"reason\":\"{}\",\"parts\":[{}],\"gaps\":[{}],\"dualboot\":{},\"dualboot_reason\":\"{}\"}}",
            esc(&self.name), esc(&self.dev), self.size, self.bus, esc(&self.model), self.removable, self.ro, self.boot, self.eligible, esc(&self.reason), parts.join(","),
            gaps.join(","), self.dualboot, esc(&self.dualboot_reason)
        )
    }

    pub fn human(&self) -> String {
        let flags = format!("{}{}{}", if self.boot { " BOOT" } else { "" }, if self.removable { " removable" } else { "" }, if self.ro { " ro" } else { "" });
        let kinds: Vec<String> = self.parts.iter().map(|p| format!("p{}:{}", p.pno, p.kind.name())).collect();
        format!(
            "{:<10} {:>8} MiB {:<6}{}  {}  [{}]",
            self.name,
            self.size >> 20,
            self.bus,
            flags,
            if self.eligible { "eligible".to_string() } else { format!("NOT eligible: {}", self.reason) },
            kinds.join(" ")
        ) + &format!(
            "  | dual-boot: {}{}",
            if self.dualboot { "yes".to_string() } else { format!("no ({})", self.dualboot_reason) },
            self.gaps.iter().map(|g| format!(" gap@{}+{}MiB", g.0, (g.1 - g.0 + 1) / 2048)).collect::<String>()
        )
    }
}

/// The installed-disk layout, in 512-byte sectors.
#[derive(Clone, Debug)]
pub struct Layout {
    pub total: u64,
    pub esp: (u64, u64),
    pub root: (u64, u64),
    pub log: (u64, u64),
    pub home: (u64, u64),
}

impl Layout {
    /// `(first, last_inclusive)` of each partition as (start, len) sectors.
    pub fn compute(disk_bytes: u64, root_bytes: u64) -> Result<Layout, String> {
        let total = disk_bytes / gpt::SECTOR;
        let s = |b: u64| b / gpt::SECTOR;
        let esp = (2048, s(ESP_BYTES));
        let root_start = esp.0 + esp.1;
        let root = (root_start, s(root_bytes));
        let root_end = root.0 + root.1;
        let end_excl = total - 33; // first sector of the backup entries
        let log_start = (end_excl - s(LOG_BYTES)) & !2047;
        let log = (log_start, s(LOG_BYTES));
        if log_start <= root_end || (log_start - root_end) * gpt::SECTOR < MIN_HOME_BYTES {
            return Err(format!(
                "disk too small: need at least {} MiB, have {} MiB",
                min_disk_bytes(root_bytes) >> 20,
                disk_bytes >> 20
            ));
        }
        let home = (root_end, log_start - root_end);
        Ok(Layout { total, esp, root, log, home })
    }

    pub fn specs(&self, guids: &[[u8; 16]; 4]) -> Vec<PartSpec> {
        let mk = |t, g, r: (u64, u64), name| PartSpec { ptype: t, guid: g, first: r.0, last: r.0 + r.1 - 1, name, attrs: if t == gpt::ESP_TYPE { 0 } else { gpt::ATTR_NO_AUTO } };
        vec![
            mk(gpt::ESP_TYPE, guids[0], self.esp, "LEANDROS ESP"),
            mk(gpt::LROOT_TYPE, guids[1], self.root, "LEANDROS root"),
            mk(gpt::LLOG_TYPE, guids[2], self.log, "LEANDROS-LOG"),
            mk(gpt::LHOME_TYPE, guids[3], self.home, "LEANDROS home"),
        ]
    }
}

pub fn find_part(parts: &[Part], kind: Kind) -> Option<&Part> {
    parts.iter().find(|p| p.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_24g() {
        let l = Layout::compute(24 * GIB, 16 * GIB).unwrap();
        assert_eq!(l.esp.0, 2048);
        assert_eq!(l.root.0, 2048 + ESP_BYTES / 512);
        assert!(l.home.0 + l.home.1 <= l.log.0);
        assert_eq!(l.log.0 % 2048, 0);
    }

    #[test]
    fn too_small() {
        assert!(Layout::compute(18 * GIB, 16 * GIB).is_err());
    }
}
