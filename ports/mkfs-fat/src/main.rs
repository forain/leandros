// mkfs.fat -- a dosfstools-CLI-compatible FAT32 formatter, for LeandrOS and
// for host use. Only the invocation disks-rs's `Formatter` makes is a hard
// requirement:
//
//   mkfs.fat [-i <volume-id-hex>] [-n <LABEL>] -F 32 <device>
//
// plus tolerating (and ignoring) the common dosfstools flags -v, -I,
// -S <n>, -s <n> and the `--` end-of-options marker, so that any future
// caller which happens to pass those does not fail to launch us.
//
// Formatting itself is delegated to the `fatfs` crate's `format_volume`,
// forced to FatType::Fat32 (never let it downgrade to FAT12/16 the way
// dosfstools' own -F 32 flag forces it, per disks-rs's comment on
// `variant_arg`).
//
// Device size is measured ourselves rather than trusting fatfs's internal
// `seek(SeekFrom::End(0))`, because LeandrOS block device nodes may report 0
// from a plain seek; when that happens we fall back to ioctl BLKGETSIZE64
// (request 0x8008_1272). The measured sector count is then handed to
// `FormatVolumeOptions::total_sectors` explicitly so fatfs never needs its
// own end-seek at all.

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::os::raw::{c_int, c_ulong};
use std::os::unix::io::AsRawFd;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use fatfs::{FatType, FormatVolumeOptions};

const BANNER: &str = "mkfs.fat (LeandrOS mkfs-fat) 1.0 (2026-09-14)";

/// Linux BLKGETSIZE64: _IOR(0x12, 114, size_t)
const BLKGETSIZE64: c_ulong = 0x8008_1272;

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

struct Args {
    volume_id: Option<u32>,
    label: Option<String>,
    fat_size: Option<u32>,
    device: Option<String>,
}

fn usage_error(msg: &str) -> ! {
    eprintln!("mkfs.fat: {msg}");
    eprintln!("usage: mkfs.fat [-i vol-id] [-n label] [-v] [-I] [-S sec-size] [-s spc] -F 32 device");
    std::process::exit(1);
}

fn parse_args() -> Args {
    let mut volume_id = None;
    let mut label = None;
    let mut fat_size = None;
    let mut device = None;
    let mut end_of_opts = false;

    let raw: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        let a = raw[i].as_str();

        if end_of_opts {
            device = Some(raw[i].clone());
            i += 1;
            continue;
        }

        match a {
            "--" => end_of_opts = true,
            "-v" | "-I" => {}
            "-i" => {
                i += 1;
                let v = raw.get(i).unwrap_or_else(|| usage_error("-i requires an argument"));
                volume_id = Some(parse_hex_u32(v));
            }
            "-n" => {
                i += 1;
                let v = raw.get(i).unwrap_or_else(|| usage_error("-n requires an argument"));
                label = Some(v.clone());
            }
            "-F" => {
                i += 1;
                let v = raw.get(i).unwrap_or_else(|| usage_error("-F requires an argument"));
                fat_size = Some(v.parse::<u32>().unwrap_or_else(|_| usage_error("invalid -F value")));
            }
            "-S" => {
                i += 1;
                raw.get(i).unwrap_or_else(|| usage_error("-S requires an argument"));
                // Sector size: accepted and ignored, we always format at 512.
            }
            "-s" => {
                i += 1;
                raw.get(i).unwrap_or_else(|| usage_error("-s requires an argument"));
                // Sectors-per-cluster: accepted and ignored, fatfs picks its own.
            }
            other if other.starts_with('-') && other.len() > 1 => {
                usage_error(&format!("unrecognized option '{other}'"));
            }
            _ => device = Some(raw[i].clone()),
        }
        i += 1;
    }

    Args { volume_id, label, fat_size, device }
}

fn parse_hex_u32(s: &str) -> u32 {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u32::from_str_radix(s, 16).unwrap_or_else(|_| usage_error(&format!("invalid volume id '{s}'")))
}

/// dosfstools uppercases, truncates/space-pads the label to the 11-byte FAT
/// short-name-style field, and rejects a handful of characters. We uppercase
/// ASCII and replace anything not safely representable (non-ASCII, and the
/// short-name-illegal characters) with '_', which matches dosfstools' leniency
/// for the common case without pulling in its full validation table.
fn label_to_fat11(label: &str) -> [u8; 11] {
    const ILLEGAL: &[u8] = b"\"*+,./:;<=>?[\\]|";
    let mut out = [b' '; 11];
    for (i, ch) in label.chars().take(11).enumerate() {
        let b = if ch.is_ascii() {
            let up = ch.to_ascii_uppercase() as u8;
            if ILLEGAL.contains(&up) {
                b'_'
            } else {
                up
            }
        } else {
            b'_'
        };
        out[i] = b;
    }
    out
}

fn default_volume_id() -> u32 {
    // dosfstools derives its default volume id from the current time when
    // none is given on the command line; we do the same rather than reusing
    // fatfs's fixed 0x12345678 default for every call.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as u32;
    let nanos = now.subsec_nanos();
    secs ^ nanos.rotate_left(16)
}

/// seek(SeekFrom::End(0)) first; if that reports 0 (as a LeandrOS block
/// device node may for a device whose size isn't known to plain lseek),
/// fall back to ioctl BLKGETSIZE64.
fn device_size_bytes(file: &File) -> io::Result<u64> {
    let mut f = file.try_clone()?;
    let end = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(0))?;
    if end != 0 {
        return Ok(end);
    }

    let mut size: u64 = 0;
    let ret = unsafe { ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut size as *mut u64) };
    if ret == 0 && size != 0 {
        return Ok(size);
    }

    Err(io::Error::new(
        io::ErrorKind::Other,
        "could not determine device size (seek(End) was 0 and BLKGETSIZE64 failed)",
    ))
}

fn run() -> Result<(), String> {
    let args = parse_args();

    match args.fat_size {
        Some(32) => {}
        Some(other) => return Err(format!("unsupported FAT size -F {other}; only 32 is supported")),
        None => return Err("missing required -F 32".to_string()),
    }

    let device = args.device.ok_or_else(|| "no device specified".to_string())?;

    // Print the banner before touching the device, matching dosfstools.
    println!("{BANNER}");

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&device)
        .map_err(|e| format!("cannot open {device}: {e}"))?;

    let total_bytes = device_size_bytes(&file).map_err(|e| format!("{device}: {e}"))?;
    const BYTES_PER_SECTOR: u16 = 512;
    let total_sectors = total_bytes / BYTES_PER_SECTOR as u64;
    if total_sectors == 0 {
        return Err(format!("{device}: device reports zero size"));
    }
    if total_sectors > u32::MAX as u64 {
        return Err(format!("{device}: device too large for a 32-bit sector count"));
    }

    let volume_id = args.volume_id.unwrap_or_else(default_volume_id);

    let mut opts = FormatVolumeOptions::new()
        .fat_type(FatType::Fat32)
        .bytes_per_sector(BYTES_PER_SECTOR)
        .total_sectors(total_sectors as u32)
        .volume_id(volume_id);

    if let Some(label) = &args.label {
        opts = opts.volume_label(label_to_fat11(label));
    }

    let mut disk = file;
    disk.seek(SeekFrom::Start(0)).map_err(|e| format!("{device}: {e}"))?;
    fatfs::format_volume(&mut disk, opts).map_err(|e| format!("{device}: format failed: {e}"))?;

    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("mkfs.fat: {msg}");
            ExitCode::FAILURE
        }
    }
}
