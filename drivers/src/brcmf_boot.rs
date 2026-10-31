//! Raspberry Pi 5 Wi-Fi bring-up at boot: takes the sdio2 host that
//! `brcmf::sdio_host::boot_probe` parked, waits for the real root (the
//! firmware lives in /lib/firmware/brcm), then runs the brcmf chain
//! attach → download → `Brcmf::new` → CLM load → "ver" on its own kernel
//! thread, the way iwlwifi loads its firmware.
//!
//! One `[brcmf]` line per stage (`chip`, `fw`, `clm`, `ver`) or a single
//! `[brcmf] fail ...` line; on any failure the device is dropped and nothing
//! touches the chip again. Every step is bounded: root wait 120 s, file sizes
//! capped, and the chip/protocol layers bound their own waits (clock 1 s,
//! F2 ready 3 s, each control request 2.5 s). `brcmf=0` skips it all.
//!
//! MAC: the DTB's `local-mac-address` on the fmac node is written with
//! `cur_etheraddr` (the nvram's macaddr is a placeholder); `[brcmf] mac ...`.
//!
//! The ready device then drives wifi0 on this same thread
//! (`brcmf::netdev::run`); only when wifi0 was not registered is it parked
//! for [`take_device`].
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spin::Mutex;

use crate::brcmf::bcdc::Brcmf;
use crate::brcmf::chip::Chip;
use crate::brcmf::fw;
use crate::brcmf::fwil::Fwil;
use crate::brcmf::sdio_host::{self, Mmio, SdioHost, SdioTune};
// Runtime retry: see `CTL_PATH` and `/bin/brcmfctl`.

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
    crate::brcmf::netdev::note_log(msg);
    let mut s = String::with_capacity(msg.len() + 9);
    s.push_str("[brcmf] ");
    s.push_str(msg);
    s.push('\n');
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

/// Text of the last `[brcmf] fail ...` line (drives the automatic retry).
static LAST_FAIL: Mutex<String> = Mutex::new(String::new());
/// Set by [`shutdown`]: no more probes or retries, the thread parks.
static SHUT: AtomicBool = AtomicBool::new(false);

/// Reboot/kexec quiesce (kernel `power::quiesce_phase1`, task context, IRQs
/// on): disassociate + WLC_DOWN through the driver thread (bounded wait),
/// then power the chip off and hold WL_REG_ON low across the jump, so the
/// next kernel finds a chip in power-on reset. None when sdio2 was never
/// probed (not a Pi 5, or brcmf=0).
pub fn shutdown() -> Option<String> {
    SHUT.store(true, Ordering::Release);
    let t0 = now_ms();
    let polite = if crate::brcmf::netdev::request_shutdown() {
        while !crate::brcmf::netdev::shutdown_done() && now_ms() - t0 < SHUTDOWN_WAIT_MS {
            sched::kthread::msleep(10);
        }
        if crate::brcmf::netdev::shutdown_done() { "disassoc+down" } else { "driver busy (no answer), forced" }
    } else {
        "driver not running"
    };
    let hw = sdio_host::power_off()?;
    Some(format!("brcmf: {} in {} ms; {}", polite, now_ms() - t0, hw))
}
const SHUTDOWN_WAIT_MS: u64 = 500;

fn fail(msg: &str) {
    *LAST_FAIL.lock() = String::from(msg);
    STATE.store(STATE_FAILED, Ordering::Release);
    crate::brcmf::netdev::set_stage("bring-up failed (brcmfctl diag|retry)");
    crate::brcmf::netdev::set_error(msg);
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
    // Fallback: the board OTP's Ethernet MAC + 1 (bcm2712_otp), as the Pi firmware assigns it.
    let mac = unsafe { boot::device_tree::recorded_dtb() }.and_then(fdt_fmac_mac).map(|m| (m, "dtb"))
        .or_else(|| crate::bcm2712_otp::wifi_mac().map(|m| (m, "otp")))
        // After a kexec the DTB is synthetic (no Wi-Fi node); the Pi firmware's
        // smsc95xx.macaddr= survives on the reused command line.
        .or_else(|| crate::brcmf::cfg::mac_from_cmdline(cmdline));
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

/// Runtime control file, polled once a second while the bring-up is failed
/// (written by `/bin/brcmfctl`). One command per write; a write only runs
/// when its content differs from the last one seen (the tool adds a nonce),
/// and whatever is in the file at boot is ignored.
pub const CTL_PATH: &str = "/etc/brcmf.ctl";
const CTL_MAX: usize = 1024;
const CTL_POLL_MS: u64 = 1000;

/// Everything a retry can vary. Each `retry` starts from the defaults.
#[derive(Clone)]
struct Opts {
    k: fw::Knobs,
    fw: String,
    nvram: Option<String>,
    clm: String,
    wloff_ms: u32,
    wlon_ms: u32,
    lines: usize,
    sd: SdioTune,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            k: fw::Knobs::default(),
            fw: String::from(fw::FW_PATH),
            nvram: None,
            clm: String::from(fw::CLM_PATH),
            wloff_ms: crate::brcmf::power::WL_OFF_BOOT_US / 1000,
            wlon_ms: 150,
            lines: 12,
            sd: SdioTune::default(),
        }
    }
}

impl Opts {
    fn set(&mut self, key: &str, val: &str) -> bool {
        let n = val.parse::<u32>().ok();
        match (key, n) {
            ("fw", _) => self.fw = String::from(val),
            ("nvram", _) => self.nvram = Some(String::from(val)),
            ("clm", _) => self.clm = String::from(val),
            ("wloff_ms", Some(n)) => self.wloff_ms = n,
            ("wlon_ms", Some(n)) => self.wlon_ms = n,
            ("lines", Some(n)) => self.lines = n.clamp(1, 64) as usize,
            ("sdclk" | "hs" | "blk" | "blksz" | "chunk", _) => return self.sd.set(key, val),
            _ => return self.k.set(key, val),
        }
        true
    }
}

enum Outcome {
    Ready(Device),
    /// Failed with the chip still reachable (for `brcmfctl diag`).
    Failed(Option<(SdioHost<Mmio>, Chip)>),
}

fn dump_diag(bus: &mut SdioHost<Mmio>, chip: &mut Chip, lines: usize) {
    let d = fw::diag(bus, chip, lines);
    log(&format!("diag {}", d.line));
    for l in d.console.iter() {
        log(&format!("con: {}", l));
    }
    if d.console.is_empty() {
        log("con: (none)");
    }
}

fn attempt(mut host: SdioHost<Mmio>, dt_mac: Option<([u8; 6], &'static str)>, o: &Opts) -> Outcome {
    // All files before touching the chip, so a missing file leaves it untouched.
    let code = match load(&o.fw, FW_MAX) { Ok(v) => v, Err(m) => { fail(&m); return Outcome::Failed(None) } };
    let nvram = match &o.nvram {
        Some(p) => match load(p, NVRAM_MAX) { Ok(v) => v, Err(m) => { fail(&m); return Outcome::Failed(None) } },
        None => match load(fw::NVRAM_PATH, NVRAM_MAX) {
            Ok(v) => v,
            Err(m) => match load(fw::NVRAM_FALLBACK_PATH, NVRAM_MAX) {
                Ok(v) => v,
                Err(_) => { fail(&m); return Outcome::Failed(None) }
            },
        },
    };
    let clm = if o.k.noclm {
        Vec::new()
    } else {
        match load(&o.clm, CLM_MAX) { Ok(v) => v, Err(m) => { fail(&m); return Outcome::Failed(None) } }
    };
    // Which image this is: the RTE version string sits near the end of the image.
    let tail = &code[code.len().saturating_sub(4096)..];
    // The whole trailer string: "<chip>-roml/<features incl. -sr-> Version: x CRC: y Date: z".
    let fwver = tail.windows(8).rposition(|w| w == b"Version:").map(|i| {
        let start = tail[..i].iter().rposition(|&c| c == 0 || c == b'\n').map_or(0, |p| p + 1);
        let s = &tail[start..];
        let end = s.iter().position(|&c| c == 0 || c == b'\n').unwrap_or(s.len());
        let t = String::from_utf8_lossy(&s[..end]).into_owned();
        let cut = t.len().saturating_sub(320);
        String::from(t.get(cut..).unwrap_or(&t))
    }).unwrap_or_else(|| String::from("Version:?"));
    log(&format!("files fw {} {}B nvram {}B clm {}B rstvec {:#x} {}", o.fw, code.len(), nvram.len(), clm.len(),
                 u32::from_le_bytes([code[0], code[1], code[2], code[3]]), fwver));

    let t = now_ms();
    crate::brcmf::netdev::set_stage("chip attach");
    let mut chip = match Chip::attach(&mut host) {
        Ok(c) => c,
        Err(e) => { fail(&format!("chip {}", e.as_str())); return Outcome::Failed(None) }
    };
    log(&format!("chip {:x} rev {} cores {} ram {:#x}@{:#x} sr {}", chip.chip_id, chip.chip_rev,
                 chip.core_count(), chip.ram_size, chip.ram_base, chip.sr_capable as u8));

    crate::brcmf::netdev::set_stage("firmware download");
    let rep = match fw::download_with(&mut host, &mut chip, &code, &nvram, &o.k) {
        Ok(r) => r,
        Err(e) => {
            let mm = e.mismatch.map(|m| format!(" {}", m.line())).unwrap_or_default();
            fail(&format!("fw {} stage={}{} t={}ms", e.err.as_str(), e.stage, mm, now_ms() - t));
            dump_diag(&mut host, &mut chip, o.lines);
            return Outcome::Failed(Some((host, chip)));
        }
    };
    drop(code);
    log(&format!("fw {} nvram {}@{:#x} rstvec {:#x} sr {} F2 ready t={}ms", rep.code_len, rep.nvram_len,
                 rep.nvram_addr, rep.rstvec, rep.sr_enabled as u8, now_ms() - t));

    let mut dev: Device = Brcmf::new(host, chip);
    // Real-time bound on every credit / control wait (see bcdc::FRAME_COST_US).
    dev.clock = Some(|| sched::monotonic_ns() / 1_000);
    let failed = |mut dev: Device, msg: String, lines: usize| {
        fail(&msg);
        dump_diag(&mut dev.bus, &mut dev.chip, lines);
        let Brcmf { bus, chip, .. } = dev;
        Outcome::Failed(Some((bus, chip)))
    };
    crate::brcmf::netdev::set_stage("clm load");
    if clm.is_empty() {
        log("clm skipped");
    } else {
        match fw::clm_load(&mut dev, &clm) {
            Ok(n) => log(&format!("clm {} chunks", n)),
            Err(e) => return failed(dev, format!("clm {:?}", e), o.lines),
        }
    }

    // The nvram carries a placeholder macaddr; the real one is the DTB's
    // local-mac-address (filled by the Pi firmware). Keep the nvram one if absent.
    crate::brcmf::netdev::set_stage("mac (cur_etheraddr)");
    let src: String = match dt_mac {
        Some((m, from)) => match dev.iovar_set("cur_etheraddr", &m) {
            Ok(()) => String::from(from),
            Err(e) => { log(&format!("mac set failed {:?}, keeping nvram", e)); format!("nvram (set of the {} MAC failed: {:?})", from, e) }
        },
        None => String::from("nvram placeholder (no dtb local-mac-address, otp tag failed, no smsc95xx.macaddr/brcmf.mac)"),
    };
    let mut mbuf = [0u8; 6];
    match dev.iovar_get("cur_etheraddr", &mut mbuf) {
        Ok(_) => {
            let differs = matches!(dt_mac, Some((m, _)) if m != mbuf);
            let src = if differs { format!("{} BUT the firmware still reports another MAC", src) } else { src };
            let line = format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} {}", mbuf[0], mbuf[1], mbuf[2], mbuf[3], mbuf[4], mbuf[5], src);
            crate::brcmf::netdev::set_mac_src(&line);
            log(&format!("mac {}", line));
        }
        Err(e) => return failed(dev, format!("mac {:?}", e), o.lines),
    }
    crate::brcmf::netdev::set_stage("ver");

    let mut buf = [0u8; VER_BUF];
    let ver = match dev.iovar_get("ver", &mut buf) {
        Ok(n) => {
            let b = &buf[..n.min(VER_BUF)];
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            let s: String = String::from_utf8_lossy(&b[..end]).chars()
                .map(|c| if c.is_control() { ' ' } else { c }).collect();
            String::from(s.trim())
        }
        Err(e) => return failed(dev, format!("ver {:?}", e), o.lines),
    };
    log(&format!("ver {}", ver));
    crate::brcmf::netdev::set_fw_version(&ver);
    *VERSION.lock() = ver;
    Outcome::Ready(dev)
}

fn read_ctl() -> Option<Vec<u8>> { load(CTL_PATH, CTL_MAX).ok() }

/// Parses one control write. Returns Some(opts) for `retry`; `diag` and
/// `status` are answered here.
fn ctl_command(text: &str, parked: &mut Option<(SdioHost<Mmio>, Chip)>, tries: u32) -> Option<Opts> {
    let mut words = text.split_ascii_whitespace().filter(|w| !w.starts_with("seq="));
    let cmd = words.next().unwrap_or("");
    match cmd {
        "retry" => {
            let mut o = Opts::default();
            let mut bad = Vec::new();
            for w in words {
                match w.split_once('=') {
                    Some((k, v)) if o.set(k, v) => {}
                    _ => bad.push(w),
                }
            }
            if !bad.is_empty() {
                log(&format!("ctl: unknown knob(s) {:?}; retry not run", bad));
                return None;
            }
            log(&format!("ctl: retry #{} wloff_ms={} wlon_ms={} {:?} {:?}", tries + 1, o.wloff_ms, o.wlon_ms, o.sd, o.k));
            Some(o)
        }
        "diag" => {
            let lines = words.find_map(|w| w.strip_prefix("lines=")).and_then(|v| v.parse().ok()).unwrap_or(32);
            match parked {
                Some((bus, chip)) => dump_diag(bus, chip, lines),
                None => log("ctl: diag: no chip (failed before attach)"),
            }
            None
        }
        "peek" | "poke" => {
            let mut addr = None;
            let mut val = None;
            for w in words {
                let num = |v: &str| match v.strip_prefix("0x") {
                    Some(h) => u32::from_str_radix(h, 16).ok(),
                    None => v.parse::<u32>().ok(),
                };
                if let Some(v) = w.strip_prefix("addr=") { addr = num(v); }
                if let Some(v) = w.strip_prefix("val=") { val = num(v); }
            }
            let Some((bus, chip)) = parked.as_mut() else {
                log("ctl: peek/poke: no chip (failed before attach)");
                return None;
            };
            let Some(a) = addr else { log("ctl: peek/poke needs addr=0x..."); return None; };
            let a = a & !3;
            if cmd == "poke" {
                let Some(v) = val else { log("ctl: poke needs val=0x..."); return None; };
                let r = chip.write32(bus, a, v);
                let back = chip.read32(bus, a);
                log(&format!("ctl: poke {:#010x} <- {:#010x} {:?} readback {:x?}", a, v, r, back));
            } else {
                // Word by word (4-byte CMD53s, the chip-id path) and as one bulk
                // read (the download path): a difference points at the bulk path.
                let mut words = String::new();
                for i in 0..4 {
                    match chip.read32(bus, a + 4 * i) {
                        Ok(v) => words.push_str(&format!("{:08x} ", v)),
                        Err(e) => words.push_str(&format!("{:?} ", e)),
                    }
                }
                let mut b = [0u8; 16];
                let bulk = match chip.bp.ram_read(bus, a, &mut b) {
                    Ok(()) => b.chunks(4).map(|c| format!("{:08x}", u32::from_le_bytes([c[0], c[1], c[2], c[3]])))
                        .collect::<Vec<_>>().join(" "),
                    Err(e) => format!("{:?}", e),
                };
                log(&format!("ctl: peek {:#010x} w32 {}bulk {}", a, words, bulk));
            }
            None
        }
        "status" | "" => {
            log(&format!("ctl: state {} tries {} sdio {} chip {}", state(), tries,
                         sdio_host::PROBE_STATE.load(Ordering::Relaxed), parked.is_some() as u8));
            None
        }
        _ => {
            log(&format!("ctl: unknown command '{}' (retry|diag|status|peek|poke; scan/join need a running chip)", cmd));
            None
        }
    }
}

fn run(host: SdioHost<Mmio>, dt_mac: Option<([u8; 6], &'static str)>) {
    let t0 = now_ms();
    crate::brcmf::netdev::set_stage("waiting for the root filesystem");
    while !crate::firmware::root_ready() {
        if now_ms() - t0 > ROOT_WAIT_MS { return fail("root-not-mounted 120s"); }
        sched::kthread::msleep(250);
    }
    // A command left in the file by an earlier boot is not replayed.
    let mut seen = read_ctl();
    let mut opts = Opts::default();
    let mut next = Some(host);
    let mut tries = 0u32;
    let mut auto = 0u32;
    let mut parked: Option<(SdioHost<Mmio>, Chip)> = None;
    // A `retry` received while wifi0 was running.
    let mut pending: Option<String> = None;
    loop {
        if let Some(h) = next.take() {
            STATE.store(STATE_RUNNING, Ordering::Release);
            parked = None;
            match attempt(h, dt_mac, &opts) {
                Outcome::Ready(dev) => match hand_off(dev) {
                    None => return,
                    Some(_) if SHUT.load(Ordering::Acquire) => { log("shutdown: driver stopped, chip parked"); return; }
                    Some(text) => { seen = read_ctl(); pending = Some(text); }
                },
                Outcome::Failed(p) => {
                    parked = p;
                    STATE.store(STATE_FAILED, Ordering::Release);
                    if SHUT.load(Ordering::Acquire) { return; }
                    let why = LAST_FAIL.lock().clone();
                    if let Some(off) = crate::brcmf::power::auto_retry_off_us(&why, auto) {
                        auto += 1;
                        log(&format!("auto-retry #{}: '{}' looks like stale chip state; power-cycling (WL_REG_ON low {} ms)",
                                     auto, why, off / 1000));
                        parked = None;
                        sdio_host::probe_with(off, opts.wlon_ms.saturating_mul(1000), opts.sd);
                        next = sdio_host::take_host();
                        if next.is_none() {
                            fail("no-sdio-host (sdio probe failed; see the [sdio] line)");
                        } else {
                            continue;
                        }
                    }
                    log(&format!("ctl: waiting for commands in {} (brcmfctl retry|diag|status)", CTL_PATH));
                }
            }
        }
        let text = match pending.take() {
            Some(t) => t,
            None => {
                sched::kthread::msleep(CTL_POLL_MS);
                let cur = read_ctl();
                if cur.is_none() || cur == seen {
                    continue;
                }
                seen = cur.clone();
                String::from_utf8_lossy(cur.as_deref().unwrap_or(&[])).into_owned()
            }
        };
        if SHUT.load(Ordering::Acquire) { return; }
        if let Some(o) = ctl_command(text.trim(), &mut parked, tries) {
            tries += 1;
            opts = o;
            parked = None;
            sdio_host::probe_with(opts.wloff_ms.saturating_mul(1000), opts.wlon_ms.saturating_mul(1000), opts.sd);
            next = sdio_host::take_host();
            if next.is_none() {
                fail("no-sdio-host (sdio probe failed; see the [sdio] line)");
            }
        }
    }
}

/// The ctl file and bus diagnostics for the running driver (netdev::run_ctl).
struct BootCtl { seen: Option<Vec<u8>> }

impl crate::brcmf::netdev::Ctl<Device> for BootCtl {
    fn poll(&mut self) -> Option<String> {
        let cur = read_ctl();
        if cur.is_none() || cur == self.seen { return None; }
        self.seen = cur.clone();
        Some(String::from_utf8_lossy(cur.as_deref().unwrap_or(&[])).into_owned())
    }
    fn diag(&mut self, dev: &mut Device, lines: usize) { dump_diag(&mut dev.bus, &mut dev.chip, lines); }
}

/// Runs wifi0 on this thread. Returns the `retry ...` text when the running
/// driver was asked for a full re-probe (the device is dropped here).
fn hand_off(dev: Device) -> Option<String> {
    STATE.store(STATE_READY, Ordering::Release);
    // Hand-off to wifi0. `netdev::init` ran at kernel init, long before this
    // thread could get here (the root is mounted by userspace init and we
    // waited for it above), so `present()` is already final. This thread
    // becomes the wifi0 driver thread: BCDC init, then the connection
    // manager (autoconnect to /etc/leandros/wifi.conf or netctl profiles).
    if crate::brcmf::netdev::present() {
        let ctl = alloc::boxed::Box::new(BootCtl { seen: read_ctl() });
        crate::brcmf::netdev::run_ctl(dev, Some(ctl)).map(|(dev, text)| { drop(dev); text })
    } else {
        log("wifi0 not registered (brcmf.disable=1); device parked");
        *DEVICE.lock() = Some(dev);
        None
    }
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
