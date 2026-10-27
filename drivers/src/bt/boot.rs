//! Raspberry Pi 5 Bluetooth bring-up at boot (CYW43455 BT half on `uarta`).
//!
//! `uarta: serial@7d50c000` is `brcm,bcm7271-uart`: a 16550A-style MMIO UART
//! (reg-shift 2, 32-bit access, 96 MHz clock, 32-byte FIFOs, MCR.AFE auto
//! RTS/CTS), SPI 0x114. The firmware does NOT mux GPIO 24-27 to it on D0
//! (real Pi 5: all four read fsel 0 = gpio), so with AFE the unconnected CTS
//! held the TX FIFO and HCI Reset failed with `Tx`. We apply the uarta
//! `default` pinctrl state ourselves (`bcm2712_gpio::bt_pins_prepare`: BT_ON
//! low GPIO first, then uart0 + dts pulls on 24-27). BT_ON is gio 29.
//!
//! RX is polled, not IRQ-driven: with auto flow control the UART drops RTS at
//! the RX trigger level, so the controller stops sending while we sleep and no
//! byte is lost (the uart10 RX loss of #101 has no flow control). An IRQ-fed
//! ring is for the HCI core lane that keeps the link up after boot.
//!
//! Sequence (bringup.rs): power-cycle via BT_ON, 115200 8N1 + RTS/CTS, HCI
//! Reset, Read Local Version, Broadcom patchram from
//! `/lib/firmware/brcm/BCM4345C0*.hcd`, 3 Mbaud, Read Local Version + BD_ADDR,
//! BD_ADDR from the live DTB if the controller has its placeholder.
//!
//! Lines: `[bt] uarta ...` at start, then `[bt] hci ver=...` or one
//! `[bt] fail stage=...` line. Inert on failure: BT_ON dropped again, nothing
//! else touched. `bt=0` skips; `bt.baud=N` picks the operational rate
//! (115200 keeps the initial one); `bt.fw=0` skips the patch download.
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use leandros_lib::mmio::{MmioRd, MmioWr};

use super::bringup::{self, Options, Transport};
use super::h4::BdAddr;
use crate::bcm2712_gpio::{self as gpio, Block, Chip, BT_ON};

pub const UARTA_PHYS: u64 = 0x10_7d50_c000;
pub const UART_CLK_HZ: u32 = 96_000_000;
pub const BT_ON_GPIO: u32 = BT_ON;
pub const OPER_BAUD: u32 = 3_000_000;
/// Linux's order: board-specific name first, then the chip name.
pub const HCD_PATHS: [&str; 2] = [
    "/lib/firmware/brcm/BCM4345C0.raspberrypi,5-model-b.hcd",
    "/lib/firmware/brcm/BCM4345C0.hcd",
];
const HCD_MAX: usize = 256 << 10;
const ROOT_WAIT_MS: u64 = 120_000;

pub const STATE_NONE: u32 = 0;
pub const STATE_SKIPPED: u32 = 1;
pub const STATE_RUNNING: u32 = 2;
pub const STATE_READY: u32 = 3;
pub const STATE_FAILED: u32 = 4;
pub static STATE: AtomicU32 = AtomicU32::new(STATE_NONE);

pub type SerrorCheck = fn() -> Option<u64>;

// 16550 registers (index; byte offset = index << 2).
const RBR_THR_DLL: usize = 0;
const IER_DLM: usize = 1;
const FCR: usize = 2;
const LCR: usize = 3;
const MCR: usize = 4;
const LSR: usize = 5;
const MSR: usize = 6;
const LCR_DLAB: u32 = 0x80;
const LCR_8N1: u32 = 0x03;
const FCR_ENABLE: u32 = 0x01;
const FCR_CLR_RX: u32 = 0x02;
const FCR_CLR_TX: u32 = 0x04;
const FCR_RX_TRIG_01: u32 = 0x40; // 8 bytes on the bcm7271 (rxtrig 1/8/16/30)
const MCR_DTR: u32 = 0x01;
const MCR_RTS: u32 = 0x02;
const MCR_AFE: u32 = 0x20;
const LSR_DR: u32 = 0x01;
const LSR_ERR: u32 = 0x1e; // OE | PE | FE | BI
const LSR_THRE: u32 = 0x20;
const LSR_TEMT: u32 = 0x40;
const TX_CHUNK: usize = 16;

extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }
pub(crate) fn log(msg: &str) {
    let mut s = String::with_capacity(msg.len() + 8);
    s.push_str("[bt] ");
    s.push_str(msg);
    s.push('\n');
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}
fn now_us() -> u64 { sched::monotonic_ns() / 1_000 }

struct Uart {
    base: usize,
    serror: SerrorCheck,
    line_errors: u32,
}

unsafe impl Send for Uart {}

impl Uart {
    fn r(&self, reg: usize) -> u32 { unsafe { ((self.base + (reg << 2)) as *const u32).mmio_read() } }
    fn w(&self, reg: usize, v: u32) { unsafe { ((self.base + (reg << 2)) as *mut u32).mmio_write(v) } }

    fn divisor(baud: u32) -> u32 { (UART_CLK_HZ + 8 * baud) / (16 * baud) }

    fn program_divisor(&self, baud: u32) -> bool {
        let d = Self::divisor(baud);
        if d == 0 || d > 0xffff { return false; }
        self.w(LCR, LCR_DLAB | LCR_8N1);
        self.w(RBR_THR_DLL, d & 0xff);
        self.w(IER_DLM, d >> 8);
        self.w(LCR, LCR_8N1);
        true
    }

    /// 8N1 at `baud`, FIFOs on, interrupts off, RTS + auto flow control.
    fn init(&mut self, baud: u32) -> bool {
        self.w(IER_DLM, 0);
        self.w(MCR, 0);
        self.w(FCR, FCR_ENABLE | FCR_CLR_RX | FCR_CLR_TX | FCR_RX_TRIG_01);
        if !self.program_divisor(baud) { return false; }
        self.w(MCR, MCR_DTR | MCR_RTS | MCR_AFE);
        for _ in 0..64 {
            if self.r(LSR) & LSR_DR == 0 { break; }
            let _ = self.r(RBR_THR_DLL);
        }
        true
    }
}

impl Transport for Uart {
    fn send(&mut self, bytes: &[u8]) -> bool {
        for chunk in bytes.chunks(TX_CHUNK) {
            // CTS-gated by AFE: the controller may hold us off; 100 ms per chunk.
            let t0 = now_us();
            while self.r(LSR) & LSR_THRE == 0 {
                if now_us() - t0 > 100_000 { return false; }
                sched::kthread::udelay(10);
            }
            for &b in chunk { self.w(RBR_THR_DLL, b as u32); }
        }
        true
    }

    fn recv(&mut self, timeout_us: u32) -> Option<u8> {
        let t0 = now_us();
        loop {
            let lsr = self.r(LSR);
            if lsr & LSR_ERR != 0 { self.line_errors += 1; }
            if lsr & LSR_DR != 0 { return Some(self.r(RBR_THR_DLL) as u8); }
            let el = now_us() - t0;
            if el >= timeout_us as u64 { return None; }
            // Spin briefly (back-to-back bytes), then sleep: RTS holds the sender off.
            if el < 200 { sched::kthread::udelay(10) } else { sched::kthread::msleep(1) }
        }
    }

    fn set_baud(&mut self, baud: u32) -> bool {
        let t0 = now_us();
        while self.r(LSR) & LSR_TEMT == 0 {
            if now_us() - t0 > 10_000 { break; }
            sched::kthread::udelay(10);
        }
        self.program_divisor(baud)
    }

    fn sleep_ms(&mut self, ms: u32) { sched::kthread::msleep(ms as u64) }

    fn checkpoint(&mut self) -> Option<u64> { (self.serror)() }
}

fn map(phys: u64, len: usize) -> Option<usize> { mm::kva::ioremap(phys, len, mm::kva::CacheMode::Uncached) }

/// Muxes the BT pins (BT_ON low GPIO, 24-27 uart0 + pulls) and describes the
/// result: `D0 mux24-27=0000->4444 pull24-27=0202 gpio29-fsel=0 bt_on=out:0`.
/// `Err` (nothing usable to log as muxed) means the bring-up must not run.
fn pinmux() -> Result<String, String> {
    let st = gpio::stepping();
    let f = |p| gpio::pin_get_fsel(Block::Main, p);
    let (Ok(a), Ok(b), Ok(c), Ok(d)) = (f(24), f(25), f(26), f(27)) else {
        return Err(format!("{:?} mux=unknown-stepping", st));
    };
    let pre = format!("{:x}{:x}{:x}{:x}", a, b, c, d);
    let post = gpio::bt_pins_prepare_hw().map_err(|e| format!("{:?} mux24-27={} mux-err={:?}", st, pre, e))?;
    let pull: String = (24..28).map(|p| match gpio::pin_get_pull(Block::Main, p) {
        Ok(v) => char::from_digit(v, 10).unwrap_or('?'), Err(_) => '?' }).collect();
    let g29 = f(BT_ON_GPIO).map(|v| v as i32).unwrap_or(-1);
    let on = match (gpio::gpio_is_output(Chip::Gio, BT_ON_GPIO), gpio::gpio_get(Chip::Gio, BT_ON_GPIO)) {
        (Ok(o), Ok(v)) => format!("{}:{}", if o { "out" } else { "in" }, v as u8),
        _ => String::from("?"),
    };
    Ok(format!("{:?} mux24-27={}->{:x}{:x}{:x}{:x} pull24-27={} gpio29-fsel={} bt_on={}",
        st, pre, post[0], post[1], post[2], post[3], pull, g29, on))
}

/// Drive BT_ON (gio 29).
fn bt_on(on: bool) { let _ = gpio::gpio_set_output(Chip::Gio, BT_ON_GPIO, on); }

fn opt<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

fn fail(msg: &str) {
    bt_on(false);
    STATE.store(STATE_FAILED, Ordering::Release);
    log(&format!("fail {}", msg));
}

/// Boot entry (rpi5): maps the UART, logs the mux, then runs the bring-up on
/// its own kernel thread once the root (and its firmware) is mounted.
pub fn start(cmdline: &str, serror: SerrorCheck, on_ready: fn(port: u32)) {
    if opt(cmdline, "bt") == Some("0") {
        STATE.store(STATE_SKIPPED, Ordering::Release);
        return log("skip (bt=0)");
    }
    let oper = opt(cmdline, "bt.baud").and_then(|v| v.parse::<u32>().ok()).unwrap_or(OPER_BAUD);
    let patch = opt(cmdline, "bt.fw") != Some("0");
    let base = match (map(UARTA_PHYS, 0x20), gpio::gpio_is_output(Chip::Gio, BT_ON_GPIO)) {
        (Some(u), Ok(_)) => u,
        _ => { STATE.store(STATE_FAILED, Ordering::Release); return log("fail stage=map"); }
    };
    if let Some(syn) = serror() {
        log(&format!("SError pending before the bt probe syn=0x{:016x} (an earlier probe)", syn));
    }
    let mux = match pinmux() {
        Ok(m) => m,
        Err(m) => { bt_on(false); STATE.store(STATE_FAILED, Ordering::Release); return log(&format!("fail stage=pinmux {}", m)); }
    };
    let uart = Uart { base, serror, line_errors: 0 };
    let lsr = uart.r(LSR);
    if let Some(syn) = serror() {
        STATE.store(STATE_FAILED, Ordering::Release);
        return log(&format!("fail stage=mmio serror=0x{:016x}", syn));
    }
    log(&format!("uarta {} lsr={:#x} baud={}", mux, lsr, oper));
    // Read the address now, while the boot DTB is certainly mapped.
    let dt_addr = unsafe { boot::device_tree::recorded_dtb() }.and_then(bringup::fdt_local_bd_address);
    STATE.store(STATE_RUNNING, Ordering::Release);
    if sched::kthread::spawn("bt", move || run(uart, oper, patch, dt_addr, on_ready)).is_none() {
        STATE.store(STATE_FAILED, Ordering::Release);
        log("fail thread-spawn");
    }
}

fn load_hcd() -> Result<(Vec<u8>, &'static str), String> {
    use crate::firmware::FwError as E;
    let mut last = String::new();
    for p in HCD_PATHS {
        match crate::firmware::request_firmware(p, HCD_MAX) {
            Ok(f) => {
                let mut v = Vec::with_capacity(f.len());
                for c in f.chunks() { v.extend_from_slice(c); }
                v.truncate(f.len());
                return Ok((v, p));
            }
            Err(E::Fs(-2)) => last = format!("firmware-missing {}", HCD_PATHS[1]),
            Err(e) => last = format!("firmware-read {} {:?}", p, e),
        }
    }
    Err(last)
}

fn run(mut uart: Uart, oper: u32, patch: bool, dt_addr: Option<BdAddr>, on_ready: fn(port: u32)) {
    let t0 = now_us();
    let hcd = if patch {
        while !crate::firmware::root_ready() {
            if now_us() - t0 > ROOT_WAIT_MS * 1000 { break; }
            sched::kthread::msleep(250);
        }
        match load_hcd() {
            Ok((v, _)) => Some(v),
            // No patch: still identify the ROM controller (inert either way).
            Err(m) => { log(&format!("{} (continuing unpatched)", m)); None }
        }
    } else { None };

    // Power-cycle: the controller may still run (kexec) at another rate.
    bt_on(false);
    sched::kthread::msleep(gpio::BT_OFF_MS as u64);
    bt_on(true);
    sched::kthread::msleep(gpio::BT_ON_SETTLE_MS as u64);
    if !uart.init(bringup::INIT_BAUD) { return fail("stage=uart-init"); }
    if let Some(syn) = (uart.serror)() { return fail(&format!("stage=uart-init serror=0x{:016x}", syn)); }

    let t1 = now_us();
    let o = Options { hcd: hcd.as_deref(), oper_baud: Some(oper), bdaddr: dt_addr, cmd_timeout_us: 0 };
    match bringup::bring_up(&mut uart, &o) {
        Ok(r) => {
            STATE.store(STATE_READY, Ordering::Release);
            let fw = match r.patch {
                Some(s) => format!("patched({} records, {} bytes)", s.records, s.payload_bytes),
                None => String::from("rom"),
            };
            let addr = if r.bdaddr_set { "set-from-dt" } else if r.bdaddr.is_placeholder() { "placeholder" } else { "controller" };
            log(&format!(
                "hci ver={} rev={:#x} lmp_ver={} lmp_subver={:#06x} rom_subver={:#06x} manuf={} fw={} baud={} bdaddr={} ({}) {} ms resets={} vers={} stray={} garbage={} lsr_err={}",
                r.ver.hci_ver, r.ver.hci_rev, r.ver.lmp_ver, r.ver.lmp_subver, r.rom.lmp_subver, r.ver.manufacturer,
                fw, r.baud, r.bdaddr, addr, (now_us() - t1) / 1000, r.reset_tries, r.version_tries, r.stray_events, r.garbage, uart.line_errors));
            // Vendor init is done: publish /dev/hci0 and become its pump.
            match super::hcidev::create_port() {
                Some(port) => {
                    on_ready(port);
                    log("/dev/hci0 ready");
                    super::hcidev::pump(&mut uart);
                }
                None => log("fail stage=hci0-port"),
            }
        }
        // lsr/msr: THRE (0x20) stuck clear with MSR.CTS (0x10) clear = flow control held.
        // baud = host rate in force; rx = bytes the failing command's wait got
        // (`-` = none at all); resets/vers = attempts.
        Err(e) => fail(&format!("stage={} err={:?} baud={} rx={}[{}] resets={} vers={} lsr_err={} lsr={:#x} msr={:#x}",
            e.stage, e.kind, e.diag.baud, e.diag.rx_len, e.diag.rx_hex(), e.diag.reset_tries, e.diag.version_tries,
            uart.line_errors, uart.r(LSR), uart.r(MSR))),
    }
}
