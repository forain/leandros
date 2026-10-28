//! Raspberry Pi 5 board identity from the OTP, read through the firmware
//! mailbox: the board serial (GET_BOARD_SERIAL) and the factory Ethernet MAC
//! (GET_BOARD_MAC_ADDRESS). The Wi-Fi MAC is the Ethernet MAC + 1, as the Pi
//! firmware assigns them.
//!
//! Consumers: `macb` (RP1 GEM) when the DTB has no `local-mac-address`, and
//! `brcmf_boot` (CYW43455) when the fmac node has none. The values are kept
//! in atomics so every build can call [`eth_mac`] / [`wifi_mac`]; only the
//! `rpi5` build ever fills them ([`boot`]).

use core::sync::atomic::{AtomicU64, Ordering};

pub const TAG_GET_BOARD_MAC_ADDRESS: u32 = 0x0001_0003;
pub const TAG_GET_BOARD_SERIAL: u32 = 0x0001_0004;

/// Bit 63 = valid; MAC in bits 0..48, byte 0 most significant.
static MAC: AtomicU64 = AtomicU64::new(0);
static SERIAL: AtomicU64 = AtomicU64::new(0);
static SERIAL_OK: AtomicU64 = AtomicU64::new(0);

const VALID: u64 = 1 << 63;

/// A usable unicast station address: not multicast, not all-zero, not all-ones.
pub fn mac_usable(m: &[u8; 6]) -> bool {
    m[0] & 1 == 0 && m.iter().any(|&b| b != 0) && m.iter().any(|&b| b != 0xFF)
}

/// MAC from the tag's value words (6 bytes, in order, little-endian words).
pub fn mac_from_words(w: &[u32]) -> Option<[u8; 6]> {
    if w.len() < 2 { return None; }
    let (a, b) = (w[0].to_le_bytes(), w[1].to_le_bytes());
    let m = [a[0], a[1], a[2], a[3], b[0], b[1]];
    mac_usable(&m).then_some(m)
}

/// Serial from the tag's value words (u64, low word first). 0 = none.
pub fn serial_from_words(w: &[u32]) -> Option<u64> {
    if w.len() < 2 { return None; }
    let s = (w[0] as u64) | ((w[1] as u64) << 32);
    (s != 0 && s != u64::MAX).then_some(s)
}

/// Wi-Fi MAC: the Ethernet MAC + 1 in the NIC-specific (low 24) bits; the OUI is kept.
pub fn wifi_from_eth(e: &[u8; 6]) -> [u8; 6] {
    let nic = (((e[3] as u32) << 16) | ((e[4] as u32) << 8) | e[5] as u32).wrapping_add(1) & 0xFF_FFFF;
    [e[0], e[1], e[2], (nic >> 16) as u8, (nic >> 8) as u8, nic as u8]
}

/// Bluetooth MAC: the Ethernet MAC + 2 in the low 24 bits (Pi convention).
pub fn bt_from_eth(e: &[u8; 6]) -> [u8; 6] {
    let nic = (((e[3] as u32) << 16) | ((e[4] as u32) << 8) | e[5] as u32).wrapping_add(2) & 0xFF_FFFF;
    [e[0], e[1], e[2], (nic >> 16) as u8, (nic >> 8) as u8, nic as u8]
}

pub fn fmt_mac(m: &[u8; 6]) -> [u8; 17] {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut o = [b':'; 17];
    for (i, b) in m.iter().enumerate() {
        o[i * 3] = H[(b >> 4) as usize];
        o[i * 3 + 1] = H[(b & 0xF) as usize];
    }
    o
}

fn pack(m: &[u8; 6]) -> u64 { m.iter().fold(0u64, |a, &b| (a << 8) | b as u64) | VALID }
fn unpack(v: u64) -> Option<[u8; 6]> {
    if v & VALID == 0 { return None; }
    let b = v.to_be_bytes();
    Some([b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Record what the OTP said (also the seam for tests).
pub fn set(serial: Option<u64>, mac: Option<[u8; 6]>) {
    if let Some(s) = serial { SERIAL.store(s, Ordering::Relaxed); SERIAL_OK.store(1, Ordering::Release); }
    if let Some(m) = mac { MAC.store(pack(&m), Ordering::Release); }
}

/// Factory Ethernet MAC, if the OTP read worked.
pub fn eth_mac() -> Option<[u8; 6]> { unpack(MAC.load(Ordering::Acquire)) }
/// Wi-Fi MAC (= Ethernet MAC + 1).
pub fn wifi_mac() -> Option<[u8; 6]> { eth_mac().map(|m| wifi_from_eth(&m)) }
/// Bluetooth MAC (= Ethernet MAC + 2), MSB first.
pub fn bt_mac() -> Option<[u8; 6]> { eth_mac().map(|m| bt_from_eth(&m)) }
pub fn serial() -> Option<u64> {
    (SERIAL_OK.load(Ordering::Acquire) != 0).then(|| SERIAL.load(Ordering::Relaxed))
}

/// Read both tags once (after `rpi_misc::boot` mapped the mailbox) and print
/// `[otp] serial=<16 hex> mac=<eth>`. Inert on failure: one line, nothing stored.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub fn boot() {
    extern crate alloc;
    let s = crate::rpi_misc::fw_tag(TAG_GET_BOARD_SERIAL, &[]).and_then(|w| serial_from_words(&w));
    let m = crate::rpi_misc::fw_tag(TAG_GET_BOARD_MAC_ADDRESS, &[]).and_then(|w| mac_from_words(&w));
    set(s, m);
    let ser = match s { Some(v) => alloc::format!("{:016x}", v), None => alloc::string::String::from("none") };
    let mac = match m { Some(v) => alloc::string::String::from(core::str::from_utf8(&fmt_mac(&v)).unwrap_or("?")), None => alloc::string::String::from("none") };
    let wifi = match m { Some(v) => alloc::string::String::from(core::str::from_utf8(&fmt_mac(&wifi_from_eth(&v))).unwrap_or("?")), None => alloc::string::String::from("none") };
    let line = alloc::format!("[otp] serial={} mac={} wifi={}{}\n", ser, mac, wifi,
                              if s.is_none() || m.is_none() { " (firmware tag failed)" } else { "" });
    extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }
    unsafe { serial_print_str_raw(line.as_ptr(), line.len()) }
}
