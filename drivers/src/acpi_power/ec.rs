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

/// Wait until `(status & mask) == want`, up to ~5 ms.
fn wait(cmd: u16, mask: u8, want: u8) -> bool {
    let t0 = crate::i915::now_ns();
    loop {
        if inb(cmd) & mask == want { return true; }
        if crate::i915::now_ns().wrapping_sub(t0) > 5_000_000 {
            TIMEOUTS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Status register (bit 5 = SCI_EVT).
pub fn status() -> u8 { ports().map_or(0, |(_, c)| inb(c)) }

pub fn read(addr: u8) -> Option<u8> {
    let (d, c) = ports()?;
    if !wait(c, IBF, 0) { return None; }
    outb(c, RD_EC);
    if !wait(c, IBF, 0) { return None; }
    outb(d, addr);
    if !wait(c, OBF, OBF) { return None; }
    Some(inb(d))
}

pub fn write(addr: u8, v: u8) -> bool {
    let Some((d, c)) = ports() else { return false };
    if !wait(c, IBF, 0) { return false; }
    outb(c, WR_EC);
    if !wait(c, IBF, 0) { return false; }
    outb(d, addr);
    if !wait(c, IBF, 0) { return false; }
    outb(d, v);
    wait(c, IBF, 0)
}

/// QR_EC: the pending query number (`_Qxx`), 0 when none.
pub fn query() -> u8 {
    let Some((d, c)) = ports() else { return 0 };
    if !wait(c, IBF, 0) { return 0; }
    outb(c, QR_EC);
    if !wait(c, OBF, OBF) { return 0; }
    inb(d)
}

/// Drain a stale output byte (OBF set with nobody waiting), as Linux does.
pub fn flush() {
    if let Some((d, c)) = ports() {
        for _ in 0..16 { if inb(c) & OBF == 0 { break; } let _ = inb(d); }
    }
}
