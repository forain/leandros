//! Laptop device inventory and small read-only probes (lane lapinv).
//!
//! At boot on x86_64 this logs, with the `[LAPINV]` prefix:
//! - every ACPI table signature, and every `Device` in the DSDT/SSDTs that
//!   declares `_HID`/`_CID` (a byte-level AML walk: Device/Scope package
//!   lengths and NameStrings, no interpreter, so `_STA` is not evaluated and
//!   a device may be declared but disabled);
//! - every PCI function with its BARs;
//! - the TPM: the TPM2 table's start method and the locality-0 registers at
//!   0xFED40000 (TIS DID_VID / CRB INTF_ID), read-only;
//! - the Thunderbolt 3 NHI: PCI power state, firmware status and ICM mode,
//!   read-only;
//! - the PCH SMBus host: probes the ST accelerometer addresses (HP 3D
//!   DriveGuard, lis3lv02d family) by reading WHO_AM_I (0x0F); a known chip
//!   is powered on and one X/Y/Z sample is logged.
//!
//! Knobs: `lapinv=0` turns all of it off, `lapinv.smbus=0` skips the SMBus
//! probe, `lapinv.acpi=1` also prints every AML device line on the UART
//! (they always go to the kernel log).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::pci;

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
    fn klog_write_only(s: *const u8, len: usize);
    fn arch_monotonic_ns() -> u64;
}

fn log(msg: &str) {
    let s = format!("[LAPINV] {}\n", msg);
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

fn log_quiet(msg: &str, loud: bool) {
    if loud { return log(msg); }
    let s = format!("[LAPINV] {}\n", msg);
    unsafe { klog_write_only(s.as_ptr(), s.len()); }
}

fn knob(cl: &str, key: &str) -> Option<&'static str> {
    for t in cl.split_ascii_whitespace() {
        if let Some(v) = t.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return Some(match v { "0" => "0", "1" => "1", _ => "?" });
        }
    }
    None
}

/// Run the whole inventory. `cmdline` is the kernel command line.
pub fn run(rsdp_phys: u64, hhdm: u64, cmdline: &str) {
    if knob(cmdline, "lapinv") == Some("0") { return; }
    let loud = knob(cmdline, "lapinv.acpi") == Some("1");
    unsafe { acpi_inventory(rsdp_phys, hhdm, loud); }
    pci_inventory();
    unsafe { tpm_probe(rsdp_phys, hhdm); }
    tb_probe();
    if knob(cmdline, "lapinv.smbus") != Some("0") {
        smbus_probe();
    }
}

// ── ACPI ─────────────────────────────────────────────────────────────────────

/// AML PkgLength at `i`: (value, bytes used).
pub fn aml_pkglen(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let lead = *b.get(i)? as usize;
    let n = lead >> 6;
    if n == 0 { return Some((lead & 0x3F, 1)); }
    let mut v = lead & 0x0F;
    for k in 0..n {
        v |= (*b.get(i + 1 + k)? as usize) << (4 + 8 * k);
    }
    Some((v, n + 1))
}

fn seg_ok(s: &[u8]) -> bool {
    s.len() == 4
        && (s[0].is_ascii_uppercase() || s[0] == b'_')
        && s[1..].iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// AML NameString at `i`: (text, absolute?, bytes used).
pub fn aml_namestring(b: &[u8], mut i: usize) -> Option<(String, bool, usize)> {
    let start = i;
    let mut s = String::new();
    let mut abs = false;
    if *b.get(i)? == b'\\' { abs = true; s.push('\\'); i += 1; }
    while *b.get(i)? == b'^' { s.push('^'); i += 1; }
    let segs = match *b.get(i)? {
        0x00 => { i += 1; 0 }
        0x2E => { i += 1; 2 }
        0x2F => { let c = *b.get(i + 1)? as usize; i += 2; c }
        _ => 1,
    };
    for k in 0..segs {
        let seg = b.get(i..i + 4)?;
        if !seg_ok(seg) { return None; }
        if k > 0 { s.push('.'); }
        for &c in seg { s.push(c as char); }
        i += 4;
    }
    Some((s, abs, i - start))
}

/// Decode a compressed EISA id (`EISAID("PNP0C0A")` → "PNP0C0A").
pub fn eisaid(v: u32) -> String {
    let id = v.swap_bytes();
    let mut s = String::new();
    for sh in [26u32, 21, 16] { s.push((((id >> sh) & 0x1F) as u8 + 0x40) as char); }
    s.push_str(&format!("{:04X}", id & 0xFFFF));
    s
}

/// Walk one DSDT/SSDT body, returning (device path, `_HID`/`_CID`, id).
pub fn aml_scan(t: &[u8]) -> Vec<(String, &'static str, String)> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut i = 36;
    while i + 6 < t.len() {
        while let Some((end, _)) = stack.last() { if i >= *end { stack.pop(); } else { break; } }
        let parent_end = stack.last().map(|e| e.0).unwrap_or(t.len());
        let parent = stack.last().map(|e| e.1.clone()).unwrap_or_default();
        let (is_dev, at) = if t[i] == 0x5B && t[i + 1] == 0x82 { (true, i + 2) } else if t[i] == 0x10 { (false, i + 1) } else { (false, 0) };
        if at != 0 {
            if let Some((len, n)) = aml_pkglen(t, at) {
                let end = at + len;
                if let Some((name, abs, used)) = aml_namestring(t, at + n) {
                    let scope_ok = is_dev || abs || name.starts_with('_') || name.starts_with('^');
                    if end > i && end <= parent_end && !name.is_empty() && scope_ok {
                        let path = if abs { String::from(name.trim_start_matches('\\')) }
                                   else if parent.is_empty() { name } else { format!("{}.{}", parent, name.trim_start_matches('^')) };
                        stack.push((end, path));
                        i = at + n + used;
                        continue;
                    }
                }
            }
        }
        if t[i] == 0x08 && (&t[i + 1..i + 5] == b"_HID" || &t[i + 1..i + 5] == b"_CID") {
            let which = if t[i + 2] == b'H' { "_HID" } else { "_CID" };
            let path = stack.last().map(|e| e.1.clone()).unwrap_or_else(|| String::from("?"));
            let id = match t[i + 5] {
                0x0C if i + 10 <= t.len() => eisaid(u32::from_le_bytes([t[i + 6], t[i + 7], t[i + 8], t[i + 9]])),
                0x0D => {
                    let s = &t[i + 6..];
                    let n = s.iter().take(64).position(|&c| c == 0).unwrap_or(0);
                    String::from(core::str::from_utf8(&s[..n]).unwrap_or("?"))
                }
                0x12 => String::from("(package)"),
                op => format!("(op {:#x})", op),
            };
            out.push((path, which, id));
            i += 5;
            continue;
        }
        i += 1;
    }
    out
}

unsafe fn acpi_inventory(rsdp: u64, hhdm: u64, loud: bool) {
    let mut sigs = String::new();
    let mut ndev = 0usize;
    let mut tpm2: Option<(u64, u32)> = None;
    boot::acpi::for_each_table(rsdp, hhdm, &mut |sig, p, len| {
        sigs.push_str(core::str::from_utf8(sig).unwrap_or("????"));
        sigs.push(' ');
        let t = core::slice::from_raw_parts(p, len);
        if sig == b"TPM2" && len >= 52 {
            let ca = u64::from_le_bytes(t[40..48].try_into().unwrap());
            let sm = u32::from_le_bytes(t[48..52].try_into().unwrap());
            tpm2 = Some((ca, sm));
        }
        if sig == b"DSDT" || sig == b"SSDT" {
            let oem = core::str::from_utf8(&t[16..24]).unwrap_or("?");
            for (path, which, id) in aml_scan(t) {
                ndev += 1;
                log_quiet(&format!("acpi {} {} {} {}={}", core::str::from_utf8(sig).unwrap_or("?"), oem.trim_end(), path, which, id), loud);
            }
        }
    });
    log(&format!("acpi tables: {}", sigs.trim_end()));
    log(&format!("acpi ids: {} _HID/_CID entries (full list in the kernel log)", ndev));
    match tpm2 {
        Some((ca, sm)) => log(&format!("acpi TPM2: start method {} ({}) control area {:#x}", sm, match sm {
            2 => "ACPI start", 6 => "TIS/FIFO", 7 => "CRB", 8 => "CRB+ACPI start", 11 => "CRB+SMC", _ => "other" }, ca)),
        None => log("acpi TPM2: no table"),
    }
}

// ── PCI ──────────────────────────────────────────────────────────────────────

fn pci_inventory() {
    for d in pci::scan().iter() {
        let mut s = format!("pci {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x}{:02x}",
            d.bus, d.dev, d.func, d.vendor_id, d.device_id, d.class, d.subclass, d.prog_if);
        let hdr = d.function().read8(0x0E) & 0x7F;
        let nbars = if hdr == 0 { 6 } else if hdr == 1 { 2 } else { 0 };
        let mut i = 0;
        while i < nbars {
            if let Some(b) = d.function().bar(i) {
                if b.base != 0 {
                    s.push_str(&format!(" bar{}={}{:#x}", i, if b.io { "io:" } else { "" }, b.base));
                }
                if b.is64 { i += 1; }
            }
            i += 1;
        }
        let ssid = d.function().read32(0x2C);
        if hdr == 0 { s.push_str(&format!(" ssid {:04x}:{:04x}", ssid & 0xFFFF, ssid >> 16)); }
        log_quiet(&s, false);
    }
}

// ── TPM ──────────────────────────────────────────────────────────────────────

unsafe fn tpm_probe(_rsdp: u64, _hhdm: u64) {
    let flags = mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::MMIO;
    let va = match mm::paging::map_kernel_device(0xFED4_0000, 0x1000, flags) {
        Some(v) => v,
        None => return log("tpm: cannot map 0xfed40000"),
    };
    let r32 = |o: usize| core::ptr::read_volatile((va + o) as *const u32);
    let access = core::ptr::read_volatile(va as *const u8);
    let intf_id = r32(0x30);
    let intf_hi = r32(0x34);
    let didvid = r32(0xF00);
    let cap = r32(0x14);
    if (didvid == 0xFFFF_FFFF || didvid == 0) && (intf_id == 0xFFFF_FFFF || intf_id == 0) {
        return log("tpm: nothing decodes 0xfed40000 (absent or disabled in setup)");
    }
    let itype = intf_id & 0xF;
    log(&format!("tpm: access {:#04x} intf_id {:#010x}:{:#010x} (type {} = {}) TIS did_vid {:#010x} intf_cap {:#010x}",
        access, intf_hi, intf_id, itype,
        match itype { 0 => "FIFO (PTP)", 1 => "CRB", 0xF => "TIS 1.2 FIFO", _ => "?" }, didvid, cap));
    let (vid, did) = if itype == 1 { (intf_hi & 0xFFFF, intf_hi >> 16) } else { (didvid & 0xFFFF, didvid >> 16) };
    let ver = if itype == 0xF { "1.2 or 2.0 (TIS)" } else { "2.0" };
    log(&format!("tpm: vendor {:#06x} ({}) device {:#06x}, TPM {}", vid,
        match vid { 0x15D1 => "Infineon", 0x8086 => "Intel PTT", 0x104A => "ST", 0x1050 => "Nuvoton", 0x1114 => "Atmel", _ => "?" }, did, ver));
}

// ── Thunderbolt 3 NHI ────────────────────────────────────────────────────────

const NHI_IDS: &[u16] = &[0x1575, 0x1577, 0x15BF, 0x15D2, 0x15D9, 0x15DC, 0x15DD, 0x15DE, 0x15E8, 0x15EB];

fn tb_probe() {
    for d in pci::scan().iter().filter(|d| d.vendor_id == 0x8086 && NHI_IDS.contains(&d.device_id)) {
        let f = d.function();
        let cmd = f.read16(0x04);
        let pm = f.find_capability(pci::cap::PM).map(|c| f.read16(c + 4) & 3);
        let mut s = format!("thunderbolt NHI {:02x}:{:02x}.{} {:04x} cmd {:#06x} D{}",
            d.bus, d.dev, d.func, d.device_id, cmd, pm.map(|p| p as i32).unwrap_or(-1));
        let bar0 = d.function().bar(0).map(|b| b.base).unwrap_or(0);
        if bar0 != 0 && cmd & pci::CMD_MEMORY != 0 && pm == Some(0) {
            let flags = mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::MMIO;
            let fw = unsafe { mm::paging::map_kernel_device(bar0 as usize + 0x39000, 0x1000, flags) }
                .map(|va| unsafe { core::ptr::read_volatile((va + 0x944) as *const u32) });
            let mail = unsafe { mm::paging::map_kernel_device(bar0 as usize + 0x39000, 0x1000, flags) }
                .map(|va| unsafe { core::ptr::read_volatile((va + 0x90C) as *const u32) });
            if let Some(v) = fw {
                s.push_str(&format!(" FW_STS {:#010x} (ICM_EN={} nvm_auth_done={})", v, v & 1, v >> 31));
            }
            if let Some(v) = mail {
                let mode = (v >> 8) & 0xF;
                s.push_str(&format!(" OUTMAIL {:#010x} mode {} ({})", v, mode,
                    match mode { 0 => "safe", 1 => "auth", 2 => "endpoint", 3 => "connection manager", _ => "?" }));
            }
        } else {
            s.push_str(" (not in D0 with memory decode: registers not read)");
        }
        let vs = f.read32(0xC8);
        s.push_str(&format!(" VS_CAP_9 {:#010x}", vs));
        log(&s);
    }
}

// ── SMBus (Intel i801) + ST accelerometer ────────────────────────────────────

unsafe fn inb(p: u16) -> u8 { let v: u8; core::arch::asm!("in al, dx", out("al") v, in("dx") p, options(nomem, nostack)); v }
unsafe fn outb(p: u16, v: u8) { core::arch::asm!("out dx, al", in("dx") p, in("al") v, options(nomem, nostack)); }

const HST_STS: u16 = 0;
const HST_CNT: u16 = 2;
const HST_CMD: u16 = 3;
const XMIT_SLVA: u16 = 4;
const HST_D0: u16 = 5;
const STS_BUSY: u8 = 1 << 0;
const STS_INTR: u8 = 1 << 1;
const STS_ERR: u8 = 0x1C; // DEV_ERR | BUS_ERR | FAILED
const STS_INUSE: u8 = 1 << 6;

struct Smbus { base: u16 }

impl Smbus {
    unsafe fn wait(&self) -> Result<u8, u8> {
        let t0 = arch_monotonic_ns();
        loop {
            let s = inb(self.base + HST_STS);
            if s & STS_ERR != 0 { return Err(s); }
            if s & STS_INTR != 0 && s & STS_BUSY == 0 { return Ok(s); }
            if arch_monotonic_ns() - t0 > 25_000_000 { return Err(0xFF); }
            core::hint::spin_loop();
        }
    }
    /// SMBus "byte data" transfer. `write`: Some(value) writes.
    unsafe fn xfer(&self, addr: u8, reg: u8, write: Option<u8>) -> Result<u8, u8> {
        outb(self.base + HST_STS, 0x1E | STS_INTR); // clear stale status (not INUSE)
        outb(self.base + XMIT_SLVA, (addr << 1) | (write.is_none() as u8));
        outb(self.base + HST_CMD, reg);
        if let Some(v) = write { outb(self.base + HST_D0, v); }
        outb(self.base + HST_CNT, 0x40 | 0x08); // START | BYTE_DATA
        let r = self.wait();
        let d = inb(self.base + HST_D0);
        outb(self.base + HST_STS, 0x1E | STS_INTR);
        r.map(|_| d)
    }
}

fn st_chip(wai: u8) -> Option<&'static str> {
    Some(match wai {
        0x3A => "LIS3LV02DL (12-bit)",
        0x3B => "LIS302DL/LIS331DL (8-bit)",
        0x52 => "LIS331DLF (6-bit)",
        0x33 => "LIS3DH/HP3DC (3DC)",
        0x32 => "LIS331DLH",
        _ => return None,
    })
}

fn smbus_probe() {
    let host = pci::scan().into_iter().find(|d| d.vendor_id == 0x8086 && d.class == 0x0C && d.subclass == 0x05);
    let d = match host { Some(d) => d, None => return log("smbus: no Intel SMBus host") };
    let f = d.function();
    let base = (f.read32(0x20) & 0xFFE0) as u16;
    let hostc = f.read8(0x40);
    let cmd = f.read16(0x04);
    if base != 0 && hostc & 1 != 0 && cmd & pci::CMD_IO == 0 {
        // Firmware left I/O decode off (the laptop does); turn it on as
        // Linux's i801 does through pci_enable_device().
        f.write16(0x04, cmd | pci::CMD_IO);
        log(&format!("smbus {:04x}: enabled I/O decode (cmd {:#x} -> {:#x})", d.device_id, cmd, f.read16(0x04)));
    }
    let cmd = f.read16(0x04);
    if base == 0 || hostc & 1 == 0 || cmd & pci::CMD_IO == 0 {
        return log(&format!("smbus {:04x}: host disabled (base {:#x} HOSTC {:#x} cmd {:#x}), not probing", d.device_id, base, hostc, cmd));
    }
    let bus = Smbus { base };
    unsafe {
        let st = inb(base + HST_STS);
        if st & STS_INUSE != 0 || st & STS_BUSY != 0 {
            return log(&format!("smbus {:04x}: in use by firmware (HST_STS {:#x}), not probing", d.device_id, st));
        }
        let mut found = Vec::new();
        for addr in [0x1Du8, 0x1C, 0x19, 0x18, 0x29] {
            match bus.xfer(addr, 0x0F, None) {
                Ok(wai) => {
                    let name = st_chip(wai);
                    log(&format!("smbus {:04x} @{:#x}: addr {:#04x} WHO_AM_I {:#04x} {}", d.device_id, base, addr, wai, name.unwrap_or("(not an ST accelerometer)")));
                    if name.is_some() { found.push((addr, wai)); }
                }
                Err(_) => log_quiet(&format!("smbus {:04x}: addr {:#04x} no ack", d.device_id, addr), false),
            }
        }
        for (addr, wai) in found {
            // CTRL_REG1: power on, X/Y/Z enabled (0x47: 50 Hz on 3DC, PD|XYZ on the 8-bit parts).
            let _ = bus.xfer(addr, 0x20, Some(0x47));
            let t0 = arch_monotonic_ns();
            while arch_monotonic_ns() - t0 < 60_000_000 { core::hint::spin_loop(); }
            let rd = |r: u8| bus.xfer(addr, r, None).unwrap_or(0) as i8;
            // 8-bit parts: OUTX/Y/Z at 0x29/0x2B/0x2D; 12-bit and 3DC: high bytes there too.
            let (x, y, z) = (rd(0x29), rd(0x2B), rd(0x2D));
            log(&format!("accel {:#04x} (wai {:#04x}): sample x={} y={} z={} (raw high bytes)", addr, wai, x, y, z));
            accel_set(addr, wai, base);
        }
        outb(base + HST_STS, STS_INUSE); // release the INUSE semaphore
    }
}

static ACCEL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn accel_set(addr: u8, wai: u8, base: u16) {
    ACCEL.store((base as u32) << 16 | (wai as u32) << 8 | addr as u32, core::sync::atomic::Ordering::Relaxed);
}

/// One accelerometer reading (x, y, z raw high bytes), if a chip was found.
/// Used by /proc/accel.
#[allow(dead_code)]
pub fn accel_read() -> Option<String> {
    let v = ACCEL.load(core::sync::atomic::Ordering::Relaxed);
    if v == 0 { return None; }
    let bus = Smbus { base: (v >> 16) as u16 };
    let addr = v as u8;
    unsafe {
        if inb(bus.base + HST_STS) & (STS_INUSE | STS_BUSY) != 0 { return Some(String::from("busy\n")); }
        let rd = |r: u8| bus.xfer(addr, r, None).map(|b| b as i8);
        let r = match (rd(0x29), rd(0x2B), rd(0x2D)) {
            (Ok(x), Ok(y), Ok(z)) => format!("{} {} {}\n", x, y, z),
            _ => String::from("error\n"),
        };
        outb(bus.base + HST_STS, STS_INUSE);
        Some(r)
    }
}
