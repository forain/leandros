//! GSP firmware: nv_get_firmware / nv_put_firmware.
//!
//! Files live on the real root, `/lib/firmware/nvidia/570.86.16/` (staged by
//! scripts/mkfs-f2fs-populated.py from $OPENRM_CACHE). The bring-up thread
//! preloads the image RM will ask for (`preload`) before `rm_init_adapter`,
//! so a missing or truncated file is reported before the GPU is touched and
//! RM's own request is served from memory.

use alloc::boxed::Box;
use core::ffi::c_void;

use drivers::firmware::{self, Firmware, FwError};

use crate::nvlog;

pub const FW_DIR: &str = "/lib/firmware/nvidia/570.86.16";

/// nv_firmware_type_t / nv_firmware_chip_family_t (nv-firmware.h).
const TYPE_GSP: u32 = 0;
const TYPE_GSP_LOG: u32 = 1;

pub(crate) fn file_for(fw_type: u32, family: u32) -> Option<&'static str> {
    // GA10X 4, AD10X 5, GH100 6, GB10X 8, GB20X 9 -> ga10x; TU10X 1, TU11X 2, GA100 3 -> tu10x.
    let ga = matches!(family, 4 | 5 | 6 | 8 | 9);
    let tu = matches!(family, 1..=3);
    match (fw_type, ga, tu) {
        (TYPE_GSP, true, _) => Some("gsp_ga10x.bin"),
        (TYPE_GSP, _, true) => Some("gsp_tu10x.bin"),
        (TYPE_GSP_LOG, true, _) => Some("gsp_log_ga10x.bin"),
        (TYPE_GSP_LOG, _, true) => Some("gsp_log_tu10x.bin"),
        _ => None,
    }
}

struct Loaded {
    name: &'static str,
    fw: Firmware,
}

static mut PRELOADED: Option<Loaded> = None;

fn fw_err(e: &FwError) -> alloc::string::String {
    match e {
        FwError::NoRoot => "root filesystem not mounted yet".into(),
        FwError::Fs(-2) => "file not found (ENOENT): firmware not staged on the stick".into(),
        FwError::Fs(n) => alloc::format!("filesystem error {}", n),
        FwError::NoMemory => "out of memory".into(),
        FwError::TooBig => "too big".into(),
    }
}

/// CRC-32 (IEEE, zlib's) of the contents now. Logged so the copy on the
/// stick can be checked against the host:
/// `python3 -c "import zlib;print(hex(zlib.crc32(open(p,'rb').read())))"`.
pub(crate) fn crc32(fw: &Firmware) -> u32 {
    fw.compute_crc32()
}

fn load(name: &'static str) -> Result<Firmware, FwError> {
    let path = alloc::format!("{}/{}", FW_DIR, name);
    let t0 = sched::monotonic_ns();
    let mut fw = firmware::request_firmware(&path, 256 << 20)?;
    let ms = (sched::monotonic_ns() - t0) / 1_000_000;
    let va = fw.map();
    let mut head = [0u8; 4];
    fw.read_at(0, &mut head);
    nvlog!("[nvrm] +{}ms firmware {}: {} bytes in {} ms, mapped at {:#x}, magic {:02x} {:02x} {:02x} {:02x}{}\n",
           crate::ms(), path, fw.len(), ms, va.unwrap_or(0), head[0], head[1], head[2], head[3],
           if &head == b"\x7fELF" { " (ELF)" } else { " (NOT an ELF image!)" });
    if va.is_none() {
        return Err(FwError::NoMemory);
    }
    Ok(fw)
}

/// Load `name` now (bring-up thread, after the root is up) and keep it for
/// RM's request. Logs size, timing, magic and CRC-32.
pub(crate) fn preload(name: &'static str) -> bool {
    match load(name) {
        Ok(fw) => {
            // Computed by request_firmware over the loaded pages.
            let crc = fw.crc32();
            nvlog!("[nvrm] +{}ms firmware {} crc32={:#010x} (over the loaded pages){}\n", crate::ms(), name, crc,
                   if name == "gsp_ga10x.bin" && fw.len() != 63_330_032 {
                       " SIZE MISMATCH: expected 63330032 for 570.86.16"
                   } else { "" });
            unsafe { PRELOADED = Some(Loaded { name, fw }) };
            true
        }
        Err(e) => {
            nvlog!("[nvrm] +{}ms firmware {}/{} FAILED: {}\n", crate::ms(), FW_DIR, name, fw_err(&e));
            false
        }
    }
}

/// C: `const void *nv_get_firmware(nv_state_t *, nv_firmware_type_t,
/// nv_firmware_chip_family_t, const void **fw_buf, NvU32 *fw_size)`.
/// Returns a handle for nv_put_firmware, NULL if unavailable.
#[no_mangle]
pub unsafe extern "C" fn nv_get_firmware(_nv: *mut c_void, fw_type: u32, family: u32, buf: *mut *const c_void, size: *mut u32) -> *const c_void {
    cb!("nv_get_firmware");
    let name = match file_for(fw_type, family) {
        Some(n) => n,
        None => {
            nvlog!("[nvrm] +{}ms nv_get_firmware: no file for type {} chip family {}\n", crate::ms(), fw_type, family);
            return core::ptr::null();
        }
    };
    nvlog!("[nvrm] +{}ms nv_get_firmware: type {} chip family {} -> {}\n", crate::ms(), fw_type, family, name);
    let pre = (*core::ptr::addr_of_mut!(PRELOADED)).take();
    let mut fw = match pre {
        Some(l) if l.name == name => l.fw,
        other => {
            PRELOADED = other;
            if !sched::irqctx::can_sleep() {
                nvlog!("[nvrm] nv_get_firmware from atomic context; refusing\n");
                return core::ptr::null();
            }
            match load(name) {
                Ok(f) => f,
                Err(e) => {
                    nvlog!("[nvrm] +{}ms nv_get_firmware {}: {}{}\n", crate::ms(), name, fw_err(&e),
                           if fw_type == TYPE_GSP_LOG { " (log decoder image; optional)" } else { "" });
                    return core::ptr::null();
                }
            }
        }
    };
    let va = match fw.map() {
        Some(v) => v,
        None => return core::ptr::null(),
    };
    if !buf.is_null() {
        *buf = va as *const c_void;
    }
    if !size.is_null() {
        *size = fw.len() as u32;
    }
    Box::into_raw(Box::new(fw)) as *const c_void
}

#[no_mangle]
pub unsafe extern "C" fn nv_put_firmware(handle: *const c_void) {
    cb!("nv_put_firmware");
    if !handle.is_null() {
        let fw = Box::from_raw(handle as *mut Firmware);
        // Integrity re-check (lane fwintegrity): RM has consumed the image
        // by now; a CRC that moved since the load means something wrote into
        // the pages in between (stray DMA, a use-after-free).
        if sched::irqctx::can_sleep() {
            let t0 = sched::monotonic_ns();
            let now = crc32(&fw);
            nvlog!("[nvrm] +{}ms nv_put_firmware: {} bytes, crc32 {:#010x} at load, {:#010x} now{} ({} ms)\n",
                   crate::ms(), fw.len(), fw.crc32(), now,
                   if now == fw.crc32() { " (unchanged)" } else { " CHANGED SINCE LOAD: the image RM used was corrupted in memory" },
                   (sched::monotonic_ns() - t0) / 1_000_000);
        }
        drop(fw);
    }
}
