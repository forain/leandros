//! Raspberry Pi 5 Wi-Fi bring-up at boot: takes the sdio2 host that
//! `brcmf::sdio_host::boot_probe` parked, waits for the real root (the
//! firmware lives in /lib/firmware/brcm), then runs the brcmf chain
//! attach → download → `Brcmf::new` → CLM load → "ver" on its own kernel
//! thread, the way iwlwifi loads its firmware.
//!
//! One `[brcmf]` line per stage (`chip`, `fw`, `clm`, `ver`) or a single
//! `[brcmf] fail ...` line; on any failure the device is dropped and nothing
//! touches the chip again. Every step is bounded: root wait 120 s, file sizes
//! capped, and the chip/protocol layers bound their own waits (clock 15 ms,
//! F2 ready 3 s, each control request 2.5 s). `brcmf=0` skips it all.
//!
//! MAC: the DTB's `local-mac-address` on the fmac node is written with
//! `cur_etheraddr` (the nvram's macaddr is a placeholder); `[brcmf] mac ...`.
//!
//! The ready device is parked for the netdev layer: [`take_device`].
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

use crate::brcmf::bcdc::Brcmf;
use crate::brcmf::chip::Chip;
use crate::brcmf::fw;
use crate::brcmf::fwil::Fwil;
use crate::brcmf::sdio_host::{self, Mmio, SdioHost};

/// The firmware-ready CYW43455: BCDC/SDPCM over pisdio's sdio2 host.
pub type Device = Brcmf<SdioHost<Mmio>>;

pub const STATE_NONE: u32 = 0;
pub const STATE_SKIPPED: u32 = 1;
pub const STATE_RUNNING: u32 = 2;
pub const STATE_READY: u32 = 3;
pub const STATE_FAILED: u32 = 4;

/// Where the bring-up is: one of the `STATE_*` values.
pub static STATE: AtomicU32 = AtomicU32::new(STATE_NONE);

static DEVICE: Mutex<Option<Device>> = Mutex::new(None);
static VERSION: Mutex<String> = Mutex::new(String::new());

const ROOT_WAIT_MS: u64 = 120_000;
const FW_MAX: usize = 2 << 20;
const NVRAM_MAX: usize = 64 << 10;
const CLM_MAX: usize = 256 << 10;
const VER_BUF: usize = 256;

/// Hand the firmware-ready device to the netdev layer (once). `None` until
/// [`state`] is `STATE_READY`, and after it has been taken.
pub fn take_device() -> Option<Device> { DEVICE.lock().take() }

/// Current bring-up state (`STATE_*`).
pub fn state() -> u32 { STATE.load(Ordering::Acquire) }

/// The firmware's "ver" string once ready (empty before).
pub fn version() -> String { VERSION.lock().clone() }

extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }

fn log(msg: &str) {
    let mut s = String::with_capacity(msg.len() + 9);
    s.push_str("[brcmf] ");
    s.push_str(msg);
    s.push('\n');
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

fn fail(msg: &str) {
    STATE.store(STATE_FAILED, Ordering::Release);
    log(&format!("fail {}", msg));
}

fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }

/// Kernel init, rpi5 only. Returns at once; the work runs on a kthread.
pub fn start(cmdline: &str) {
    if cmdline.split_ascii_whitespace().any(|t| t == "brcmf=0") {
        STATE.store(STATE_SKIPPED, Ordering::Release);
        log("skip (brcmf=0)");
        return;
    }
    let Some(host) = sdio_host::take_host() else {
        return fail("no-sdio-host");
    };
    STATE.store(STATE_RUNNING, Ordering::Release);
    // Read the MAC now, while the boot DTB is certainly mapped.
    let mac = unsafe { boot::device_tree::recorded_dtb() }.and_then(fdt_fmac_mac);
    if sched::kthread::spawn("brcmf", move || run(host, mac)).is_none() {
        fail("thread-spawn");
    }
}

/// Whole file as a Vec. `Err` carries the fail-line text.
fn load(path: &str, max: usize) -> Result<Vec<u8>, String> {
    use crate::firmware::FwError as E;
    match crate::firmware::request_firmware(path, max) {
        Ok(f) => {
            let mut v = Vec::with_capacity(f.len());
            for c in f.chunks() { v.extend_from_slice(c); }
            v.truncate(f.len());
            Ok(v)
        }
        Err(E::Fs(-2)) => Err(format!("firmware-missing {}", path)),
        Err(e) => Err(format!("firmware-read {} {:?}", path, e)),
    }
}

fn run(mut host: SdioHost<Mmio>, dt_mac: Option<[u8; 6]>) {
    let t0 = now_ms();
    while !crate::firmware::root_ready() {
        if now_ms() - t0 > ROOT_WAIT_MS { return fail("root-not-mounted 120s"); }
        sched::kthread::msleep(250);
    }
    // All three files before touching the chip, so a missing file leaves it untouched.
    let code = match load(fw::FW_PATH, FW_MAX) { Ok(v) => v, Err(m) => return fail(&m) };
    let nvram = match load(fw::NVRAM_PATH, NVRAM_MAX) {
        Ok(v) => v,
        Err(m) => match load(fw::NVRAM_FALLBACK_PATH, NVRAM_MAX) {
            Ok(v) => v,
            Err(_) => return fail(&m),
        },
    };
    let clm = match load(fw::CLM_PATH, CLM_MAX) { Ok(v) => v, Err(m) => return fail(&m) };

    let mut chip = match Chip::attach(&mut host) {
        Ok(c) => c,
        Err(e) => return fail(&format!("chip {}", e.as_str())),
    };
    log(&format!("chip {:x} rev {} cores {} ram {:#x}@{:#x} sr {}", chip.chip_id, chip.chip_rev,
                 chip.core_count(), chip.ram_size, chip.ram_base, chip.sr_capable as u8));

    let rep = match fw::download(&mut host, &mut chip, &code, &nvram) {
        Ok(r) => r,
        Err(e) => return fail(&format!("fw {}", e.as_str())),
    };
    drop(code);
    log(&format!("fw {} nvram {} rstvec {:#x} F2 ready", rep.code_len, rep.nvram_len, rep.rstvec));

    let mut dev: Device = Brcmf::new(host, chip);
    match fw::clm_load(&mut dev, &clm) {
        Ok(n) => log(&format!("clm {} chunks", n)),
        Err(e) => return fail(&format!("clm {:?}", e)),
    }

    // The nvram carries a placeholder macaddr; the real one is the DTB's
    // local-mac-address (filled by the Pi firmware). Keep the nvram one if absent.
    let src = match dt_mac {
        Some(m) => match dev.iovar_set("cur_etheraddr", &m) {
            Ok(()) => "dtb",
            Err(e) => { log(&format!("mac set failed {:?}, keeping nvram", e)); "nvram" }
        },
        None => "nvram (no dtb local-mac-address)",
    };
    let mut mbuf = [0u8; 6];
    match dev.iovar_get("cur_etheraddr", &mut mbuf) {
        Ok(_) => log(&format!("mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} {}",
                              mbuf[0], mbuf[1], mbuf[2], mbuf[3], mbuf[4], mbuf[5], src)),
        Err(e) => return fail(&format!("mac {:?}", e)),
    }

    let mut buf = [0u8; VER_BUF];
    let ver = match dev.iovar_get("ver", &mut buf) {
        Ok(n) => {
            let b = &buf[..n.min(VER_BUF)];
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            let s: String = String::from_utf8_lossy(&b[..end]).chars()
                .map(|c| if c.is_control() { ' ' } else { c }).collect();
            String::from(s.trim())
        }
        Err(e) => return fail(&format!("ver {:?}", e)),
    };
    log(&format!("ver {}", ver));
    *VERSION.lock() = ver;
    *DEVICE.lock() = Some(dev);
    STATE.store(STATE_READY, Ordering::Release);
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Walks a flattened device tree for the Wi-Fi function node (compatible
/// `brcm,bcm4329-fmac`, the sdio2 `wifi@1` child) and returns its
/// `local-mac-address` if the firmware filled in a valid unicast one.
pub fn fdt_fmac_mac(dtb: &[u8]) -> Option<[u8; 6]> {
    if be32(dtb, 0)? != 0xd00d_feed { return None; }
    let st = be32(dtb, 8)? as usize;
    let strs = be32(dtb, 12)? as usize;
    let st_len = be32(dtb, 36).unwrap_or(0) as usize;
    let end = (st + st_len).min(dtb.len());
    // Per open node: (is_fmac, mac). Properties precede child nodes.
    let mut stack: [(bool, Option<[u8; 6]>); 32] = [(false, None); 32];
    let mut depth = 0usize;
    let mut o = st;
    while o + 4 <= end {
        let tok = be32(dtb, o)?;
        o += 4;
        match tok {
            1 => {
                let n = dtb.get(o..end)?.iter().position(|&c| c == 0)?;
                o = (o + n + 1 + 3) & !3;
                depth += 1;
                if depth >= stack.len() { return None; }
                stack[depth] = (false, None);
            }
            2 => {
                if let (true, Some(m)) = stack[depth] { return Some(m); }
                depth = depth.checked_sub(1)?;
            }
            3 => {
                let len = be32(dtb, o)? as usize;
                let noff = be32(dtb, o + 4)? as usize;
                let val = dtb.get(o + 8..o + 8 + len)?;
                o = (o + 8 + len + 3) & !3;
                let name_start = strs + noff;
                let nlen = dtb.get(name_start..)?.iter().position(|&c| c == 0)?;
                let name = &dtb[name_start..name_start + nlen];
                if name == b"compatible" {
                    if val.split(|&c| c == 0).any(|c| c == b"brcm,bcm4329-fmac") { stack[depth].0 = true; }
                } else if name == b"local-mac-address" && len == 6 {
                    let m = [val[0], val[1], val[2], val[3], val[4], val[5]];
                    if m != [0; 6] && m[0] & 1 == 0 { stack[depth].1 = Some(m); }
                }
            }
            4 => {}
            _ => break,
        }
    }
    None
}
