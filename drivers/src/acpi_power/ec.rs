//! ACPI embedded controller (ACPI spec 12.2): byte reads/writes of the EC
//! address space and SCI event queries, over the two I/O ports the ECDT or
//! the EC device's `_CRS` names (0x62 data / 0x66 command+status on almost
//! every PC). Polled; no interrupt is used.

use core::sync::atomic::{AtomicU32, Ordering};

const OBF: u8 = 0x01;
const IBF: u8 = 0x02;
/// An SCI event is pending: issue QR_EC to fetch its query number.
pub const SCI_EVT: u8 = 0x20;
const RD_EC: u8 = 0x80;
const WR_EC: u8 = 0x81;
const QR_EC: u8 = 0x84;

/// Data port in the low 16 bits, command/status port in the high 16; 0 = no EC.
static PORTS: AtomicU32 = AtomicU32::new(0);
/// Timeouts seen (each one aborts one transaction).
pub static TIMEOUTS: AtomicU32 = AtomicU32::new(0);

pub fn set_ports(data: u16, cmd: u16) { PORTS.store(data as u32 | (cmd as u32) << 16, Ordering::Release); }
pub fn present() -> bool { PORTS.load(Ordering::Acquire) != 0 }
fn ports() -> Option<(u16, u16)> {
    let p = PORTS.load(Ordering::Acquire);
    if p == 0 { None } else { Some((p as u16, (p >> 16) as u16)) }
}

#[cfg(target_arch = "x86_64")]
fn inb(port: u16) -> u8 { let v: u8; unsafe { core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack)); } v }
#[cfg(target_arch = "x86_64")]
fn outb(port: u16, v: u8) { unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)); } }
#[cfg(not(target_arch = "x86_64"))]
fn inb(_port: u16) -> u8 { 0xFF }
#[cfg(not(target_arch = "x86_64"))]
fn outb(_port: u16, _v: u8) {}

/// Serializes every EC transaction (AML field path and the `_Qxx` poller).
static LOCK: spin::Mutex<()> = spin::Mutex::new(());
/// `acpi_power.trace=1`: log every EC access.
pub static TRACE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Time before which a backed-off EC is not touched again.
static BACKOFF_UNTIL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Consecutive timeouts since the last success.
static CONSEC: AtomicU32 = AtomicU32::new(0);

fn tr(args: core::fmt::Arguments) {
    if TRACE.load(Ordering::Relaxed) { crate::i915::emit(&alloc::format!("acpi: ec {}\n", args)); }
}

/// Wait until `(status & mask) == want`, up to ~50 ms.
fn wait(cmd: u16, mask: u8, want: u8) -> bool {
    let t0 = crate::i915::now_ns();
    loop {
        if inb(cmd) & mask == want { return true; }
        let now = crate::i915::now_ns();
        if now.wrapping_sub(t0) > 50_000_000 {
            let n = TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 16 { crate::i915::emit(&alloc::format!("acpi: ec TIMEOUT #{} status={:#x} mask={:#x} want={:#x}\n", n, inb(cmd), mask, want)); }
            // A dead EC must not cost 50 ms per access: back off for 1 s after 4 in a row.
            if CONSEC.fetch_add(1, Ordering::Relaxed) + 1 >= 4 {
                BACKOFF_UNTIL.store(now + 1_000_000_000, Ordering::Relaxed);
                CONSEC.store(0, Ordering::Relaxed);
            }
            return false;
        }
        core::hint::spin_loop();
    }
}

fn ok() { CONSEC.store(0, Ordering::Relaxed); }

fn backed_off() -> bool { crate::i915::now_ns() < BACKOFF_UNTIL.load(Ordering::Relaxed) }

/// Drop a stale output byte before a command (bounded).
fn drain(d: u16, c: u16) { for _ in 0..16 { if inb(c) & OBF == 0 { break; } let _ = inb(d); } }

/// Status register (bit 5 = SCI_EVT).
pub fn status() -> u8 { ports().map_or(0, |(_, c)| inb(c)) }

pub fn read(addr: u8) -> Option<u8> {
    let (d, c) = ports()?;
    if backed_off() { return None; }
    let _g = LOCK.lock();
    drain(d, c);
    let r = (|| {
        if !wait(c, IBF, 0) { return None; }
        outb(c, RD_EC);
        if !wait(c, IBF, 0) { return None; }
        outb(d, addr);
        if !wait(c, OBF, OBF) { return None; }
        Some(inb(d))
    })();
    if r.is_some() { ok(); }
    tr(format_args!("rd off={:#04x} -> {:?}", addr, r));
    r
}

pub fn write(addr: u8, v: u8) -> bool {
    let Some((d, c)) = ports() else { return false };
    if backed_off() { return false; }
    let _g = LOCK.lock();
    drain(d, c);
    let r = (|| {
        if !wait(c, IBF, 0) { return false; }
        outb(c, WR_EC);
        if !wait(c, IBF, 0) { return false; }
        outb(d, addr);
        if !wait(c, IBF, 0) { return false; }
        outb(d, v);
        wait(c, IBF, 0)
    })();
    if r { ok(); }
    tr(format_args!("wr off={:#04x} <- {:#04x} ok={}", addr, v, r));
    r
}

/// QR_EC: the pending query number (`_Qxx`), 0 when none.
pub fn query() -> u8 {
    let Some((d, c)) = ports() else { return 0 };
    if backed_off() { return 0; }
    let _g = LOCK.lock();
    if !wait(c, IBF, 0) { return 0; }
    outb(c, QR_EC);
    if !wait(c, OBF, OBF) { return 0; }
    let q = inb(d);
    ok();
    tr(format_args!("query -> {:#04x}", q));
    q
}

/// Log the whole 256-byte EC space (`acpi_power.ecdump=1`).
pub fn dump() {
    for row in 0..16u16 {
        let mut s = alloc::string::String::new();
        for col in 0..16u16 {
            match read((row * 16 + col) as u8) { Some(v) => s.push_str(&alloc::format!("{:02x} ", v)), None => s.push_str("?? ") }
        }
        crate::i915::emit(&alloc::format!("acpi: ecdump {:02x}: {}\n", row * 16, s));
    }
}

/// Drain a stale output byte (OBF set with nobody waiting), as Linux does.
pub fn flush() {
    if let Some((d, c)) = ports() {
        let _g = LOCK.lock();
        for _ in 0..16 { if inb(c) & OBF == 0 { break; } let _ = inb(d); }
    }
}
