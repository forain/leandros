//! `nvprobe` — read-only first-contact probe for an NVIDIA GPU (G1).
//!
//! Runs once at boot, and only if the PCI bus holds a display-class function
//! whose vendor is 0x10de (or the vendor named by `nvprobe.vendor=`, which
//! exists so every code path here can be exercised against QEMU's VGA device).
//! It only *reads*. The state it ever changes is restored before returning:
//!
//!   * PCI COMMAND is cleared of memory/IO decode around BAR sizing (as Linux
//!     does) and put back bit-for-bit; memory decode is switched ON only if
//!     firmware left it off, because without it BAR0 reads return all-ones.
//!   * The ROM-shadow bit (config dword 0x50 bit 0, NV_PBUS_PCI_NV_20) is
//!     cleared for the duration of the PROM read, exactly as nouveau's
//!     `nvkm_pci_rom_shadow(pci, false)` does in shadowrom.c, then the
//!     original dword is written back.
//!   * The expansion-ROM BAR enable bit is toggled for the fallback read and
//!     restored.
//!
//! Output goes through the normal kernel console/log (`[NVPROBE]` lines).
//!
//! Kernel command line (Limine `cmdline:`), all optional:
//!   `nvprobe.off=1`        skip the probe entirely
//!   `nvprobe.vendor=0x..`  probe this vendor instead of 0x10de (generic mode:
//!                          BAR0 registers and the PROM window are NOT touched,
//!                          only config space and the expansion ROM)
//!   `nvprobe.dumpvbios=1`  also emit the raw VBIOS as base64 between
//!                          `-----BEGIN VBIOS-----` / `-----END VBIOS-----`
//!                          into the persistent klog ring only (not the console),
//!                          and allow a longer read budget
//!   `nvprobe.romaddr=0x..` (testing) temporarily place the expansion ROM BAR
//!                          here when firmware left it unassigned

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::pci::{self, PciDevice};

const NV_VENDOR: u16 = 0x10de;
/// Whole probe is bounded: the register + VBIOS read stops at this.
const BUDGET_NS: u64 = 600_000_000;
const BUDGET_DUMP_NS: u64 = 5_000_000_000;
/// PROM window: BAR0 + 0x300000, 1 MiB.
const PROM_OFF: usize = 0x30_0000;
const PROM_MAX: usize = 0x10_0000;
const BAR0_MIN: u64 = 16 << 20;

// ── command line ─────────────────────────────────────────────────────────────

static mut CMDLINE: [u8; 512] = [0; 512];
static mut CMDLINE_LEN: usize = 0;

/// Record the kernel command line (called once, before anything else runs).
pub fn set_cmdline(s: &str) {
    let n = s.len().min(511);
    unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(CMDLINE);
        buf[..n].copy_from_slice(&s.as_bytes()[..n]);
        CMDLINE_LEN = n;
    }
}

fn cmdline() -> &'static str {
    unsafe {
        let buf = &*core::ptr::addr_of!(CMDLINE);
        core::str::from_utf8(&buf[..CMDLINE_LEN]).unwrap_or("")
    }
}

/// `key=value` from the kernel command line (decimal or 0x-hex), for other
/// kernel modules' switches (`drm.syncfile=1`).
pub fn cmdline_opt(key: &str) -> Option<u64> {
    opt(key)
}

fn opt(key: &str) -> Option<u64> {
    for tok in cmdline().split_ascii_whitespace() {
        if let Some(v) = tok.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return if let Some(h) = v.strip_prefix("0x") {
                u64::from_str_radix(h, 16).ok()
            } else {
                v.parse().ok()
            };
        }
    }
    None
}

// ── logging / timing ─────────────────────────────────────────────────────────

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
    fn klog_write_only(s: *const u8, len: usize);
    fn arch_monotonic_ns() -> u64;
}

/// Append to the persistent kernel-log ring only (no UART, no framebuffer).
fn emit_logonly(s: &str) {
    unsafe { klog_write_only(s.as_ptr(), s.len()); }
}

fn emit(s: &str) {
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

fn log(msg: &str) {
    emit("[NVPROBE] ");
    emit(msg);
    emit("\n");
}

macro_rules! nvlog { ($($a:tt)*) => { log(&format!($($a)*)) }; }

fn now() -> u64 { unsafe { arch_monotonic_ns() } }

/// Bounded busy-wait; falls back to an iteration cap if the clock is stuck.
fn delay_us(us: u64) {
    let start = now();
    for _ in 0..2_000_000u32 {
        if now().wrapping_sub(start) >= us * 1000 { return; }
        core::hint::spin_loop();
    }
}

fn fmt_size(n: u64) -> String {
    if n >= 1 << 30 && n % (1 << 30) == 0 { format!("{} GiB", n >> 30) }
    else if n >= 1 << 20 && n % (1 << 20) == 0 { format!("{} MiB", n >> 20) }
    else if n >= 1 << 10 && n % (1 << 10) == 0 { format!("{} KiB", n >> 10) }
    else { format!("{} B", n) }
}

// ── config space (legacy for < 0x100, ECAM for the extended part) ─────────────

struct Dev {
    bus: u8,
    dev: u8,
    func: u8,
    /// Virtual address of this function's 4 KiB ECAM page, or 0.
    ext: usize,
}

impl Dev {
    unsafe fn rd32(&self, off: u16) -> u32 {
        if off < 0x100 {
            pci::pci_read_config_32(self.bus, self.dev, self.func, off as u8)
        } else if self.ext != 0 && off < 0x1000 {
            core::ptr::read_volatile((self.ext + (off as usize & !3)) as *const u32)
        } else {
            0xFFFF_FFFF
        }
    }
    unsafe fn rd16(&self, off: u16) -> u16 { (self.rd32(off & !3) >> ((off & 2) * 8)) as u16 }
    unsafe fn rd8(&self, off: u16) -> u8 { (self.rd32(off & !3) >> ((off & 3) * 8)) as u8 }
    unsafe fn wr32(&self, off: u8, v: u32) { pci::pci_write_config_32(self.bus, self.dev, self.func, off, v) }
    unsafe fn wr16(&self, off: u8, v: u16) { pci::pci_write_config_16(self.bus, self.dev, self.func, off, v) }
}

/// Map this function's ECAM page. `None` if there is no ECAM or it cannot be
/// mapped; the caller then logs "extended config unavailable".
unsafe fn map_ecam(d: &PciDevice, rsdp: u64, hhdm: u64) -> Option<usize> {
    let off = ((d.bus as usize) << 20) | ((d.dev as usize) << 15) | ((d.func as usize) << 12);
    #[cfg(target_arch = "x86_64")]
    {
        let base = boot::acpi::find_ecam_base(rsdp, hhdm) as usize;
        if base == 0 { return None; }
        mm::paging::map_kernel_device(
            base + off, 4096,
            mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO,
        )
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (rsdp, hhdm);
        let base = pci::ecam_va();
        if base == 0 { return None; }
        Some(base + off)
    }
}

// ── BARs ─────────────────────────────────────────────────────────────────────

struct Bar { idx: usize, io: bool, is64: bool, pf: bool, base: u64, size: u64 }

/// Size every BAR and the expansion ROM with decode disabled, restoring each
/// register and then COMMAND exactly (`pci::PciFunction::size_bars`, the
/// sizing code that used to live here). Returns (bars, rom_orig, rom_size).
unsafe fn size_bars(d: &Dev) -> (Vec<Bar>, u32, u64) {
    let s = pci::PciFunction::new(d.bus, d.dev, d.func).size_bars();
    let bars = s.bars.iter().map(|b| Bar {
        idx: b.index, io: b.io, is64: b.is64, pf: b.prefetchable, base: b.base, size: b.size,
    }).collect();
    (bars, s.rom_orig, s.rom_size)
}

// ── capabilities ─────────────────────────────────────────────────────────────

fn gts(code: u16) -> &'static str {
    match code { 1 => "2.5GT/s", 2 => "5GT/s", 3 => "8GT/s", 4 => "16GT/s", 5 => "32GT/s", 6 => "64GT/s", _ => "?" }
}

unsafe fn log_caps(d: &Dev, have_ext: bool) {
    if d.rd16(0x06) & (1 << 4) == 0 {
        log("no capability list");
    } else {
        let mut p = d.rd8(0x34) as u16 & !3;
        let mut guard = 0;
        while p >= 0x40 && guard < 48 {
            guard += 1;
            let id = d.rd8(p);
            match id {
                0x01 => nvlog!("cap 0x01 power-management @0x{:02x} PMCSR=0x{:04x}", p, d.rd16(p + 4)),
                0x05 => {
                    let c = d.rd16(p + 2);
                    nvlog!("cap 0x05 MSI @0x{:02x} ctrl=0x{:04x} (64bit={} vectors=2^{} enabled={})",
                        p, c, (c & 0x80 != 0) as u8, (c >> 1) & 7, c & 1);
                }
                0x11 => {
                    let c = d.rd16(p + 2);
                    nvlog!("cap 0x11 MSI-X @0x{:02x} ctrl=0x{:04x} table-size={} table=0x{:08x} pba=0x{:08x}",
                        p, c, (c & 0x7FF) + 1, d.rd32(p + 4), d.rd32(p + 8));
                }
                0x10 => {
                    let devcap = d.rd32(p + 4);
                    let lcap = d.rd32(p + 0x0C);
                    let lsta = d.rd16(p + 0x12);
                    nvlog!("cap 0x10 PCIe @0x{:02x} devcap=0x{:08x} type={}", p, devcap, (d.rd16(p + 2) >> 4) & 0xF);
                    nvlog!("PCIe link: current {} x{}, max {} x{} (GPUs downtrain at idle; max is what the slot+GPU can do)",
                        gts(lsta & 0xF), (lsta >> 4) & 0x3F, gts((lcap & 0xF) as u16), (lcap >> 4) & 0x3F);
                }
                _ => nvlog!("cap 0x{:02x} @0x{:02x}", id, p),
            }
            p = d.rd8(p + 1) as u16 & !3;
        }
    }
    if !have_ext {
        log("extended config unavailable (no ECAM): Resizable BAR / AER not readable");
        return;
    }
    let mut p: u16 = 0x100;
    let mut guard = 0;
    let mut seen_rebar = false;
    while p >= 0x100 && p < 0x1000 && guard < 64 {
        guard += 1;
        let h = d.rd32(p);
        if h == 0 || h == 0xFFFF_FFFF { break; }
        let id = (h & 0xFFFF) as u16;
        nvlog!("ext cap 0x{:04x} v{} @0x{:03x}", id, (h >> 16) & 0xF, p);
        if id == 0x0015 {
            seen_rebar = true;
            let nbars = ((d.rd32(p + 8) >> 5) & 7) as u16;
            for n in 0..nbars.min(6) {
                let cap = d.rd32(p + 4 + 8 * n);
                let ctl = d.rd32(p + 8 + 8 * n);
                let cur = (ctl >> 8) & 0x3F;
                let mut sup = String::new();
                for b in 4..32u32 {
                    if cap & (1 << b) != 0 {
                        if !sup.is_empty() { sup.push(' '); }
                        sup.push_str(&fmt_size(1u64 << (b + 16)));
                    }
                }
                nvlog!("ReBAR entry {}: BAR{} current={} supported=[{}] (cap=0x{:08x} ctl=0x{:08x})",
                    n, ctl & 7, fmt_size(1u64 << (cur + 20)), sup, cap, ctl);
            }
        }
        p = ((h >> 20) & 0xFFF) as u16 & !3;
    }
    if !seen_rebar { log("no Resizable BAR extended capability"); }
}

// ── NVIDIA chip identification (nouveau nvkm/engine/device/base.c) ───────────

fn chip_name(chipset: u32) -> &'static str {
    match chipset {
        0x117 => "GM107", 0x120 => "GM200", 0x124 => "GM204", 0x126 => "GM206", 0x12b => "GM20B",
        0x130 => "GP100", 0x132 => "GP102", 0x134 => "GP104", 0x136 => "GP106", 0x137 => "GP107",
        0x138 => "GP108", 0x140 => "GV100",
        0x162 => "TU102", 0x164 => "TU104", 0x166 => "TU106", 0x167 => "TU117", 0x168 => "TU116",
        0x170 => "GA100", 0x172 => "GA102", 0x173 => "GA103", 0x174 => "GA104", 0x176 => "GA106",
        0x177 => "GA107",
        0x192 => "AD102", 0x193 => "AD103", 0x194 => "AD104", 0x196 => "AD106", 0x197 => "AD107",
        0x1b2 => "GB202", 0x1b3 => "GB203", 0x1b5 => "GB205", 0x1b6 => "GB206", 0x1b7 => "GB207",
        _ => "unknown",
    }
}

fn arch_name(chipset: u32) -> &'static str {
    match chipset >> 4 {
        0x11 | 0x12 => "Maxwell", 0x13 => "Pascal", 0x14 => "Volta", 0x16 => "Turing",
        0x17 => "Ampere", 0x19 => "Ada", 0x1b => "Blackwell", _ => "unknown",
    }
}

fn decode_boot(label: &str, v: u32) {
    if v == 0xFFFF_FFFF || v == 0 {
        nvlog!("{} = 0x{:08x} (not a valid chip id)", label, v);
        return;
    }
    let chipset = (v & 0x1FF0_0000) >> 20;
    nvlog!("{} = 0x{:08x} -> arch=0x{:02x} impl=0x{:x} chipset=0x{:03x} {} ({}) revision=0x{:02x}",
        label, v, chipset >> 4, chipset & 0xF, chipset, chip_name(chipset), arch_name(chipset), v & 0xFF);
}

// ── BAR0 registers + VBIOS ───────────────────────────────────────────────────

unsafe fn rd(bar0: usize, off: usize) -> u32 { core::ptr::read_volatile((bar0 + off) as *const u32) }

unsafe fn probe_regs(bar0: usize) -> bool {
    let boot0 = rd(bar0, 0x000000);
    if boot0 == 0xFFFF_FFFF {
        log("PMC_BOOT_0 reads 0xffffffff: BAR0 not decoding (device off the bus / D3 / memory decode off)");
        return false;
    }
    decode_boot("PMC_BOOT_0 ", boot0);
    decode_boot("PMC_BOOT_42", rd(bar0, 0x000A00));
    nvlog!("PMC_ENABLE = 0x{:08x}", rd(bar0, 0x000200));
    let t0 = rd(bar0, 0x009400);
    let h0 = rd(bar0, 0x009410);
    delay_us(100);
    let t1 = rd(bar0, 0x009400);
    let h1 = rd(bar0, 0x009410);
    nvlog!("PTIMER TIME_0/TIME_1: 0x{:08x}/0x{:08x} then 0x{:08x}/0x{:08x} (ticks={})",
        t0, h0, t1, h1, if t0 != t1 || h0 != h1 { "yes" } else { "NO" });
    // Best effort, UNVERIFIED against the register spec: NV_PFB_PRI_MMU_LOCAL_
    // MEMORY_RANGE (0x100ce0) is a per-FBPA amount (mag = bits 5:0, scale =
    // bits 9:6, size = mag << (scale + 20)); nouveau's gp100/ga102 RAM probe
    // multiplies per-FBPA sizes over the enabled FBPAs (count at 0x022438,
    // floorswept mask at 0x022554). On an 8 GiB RTX 3070 the register reads
    // 0x208 = 2 GiB per FBPA with 4 FBPAs, hence the multiplication.
    let r = rd(bar0, 0x100CE0);
    let per = ((r & 0x3F) as u64) << (((r >> 6) & 0xF) + 20);
    let n = rd(bar0, 0x022438);
    let mask = rd(bar0, 0x022554);
    let live = if (1..=32).contains(&n) {
        let all = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
        n - (mask & all).count_ones()
    } else { 0 };
    nvlog!("VRAM (best effort, UNVERIFIED): 0x100ce0=0x{:08x} -> {} per FBPA; FBPA count 0x022438={} mask 0x022554=0x{:08x} -> {} enabled; estimated total {} ecc={}",
        r, fmt_size(per), n, mask, live, if live != 0 { fmt_size(per * live as u64) } else { String::from("unknown") },
        (r >> 30) & 1);
    true
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 { c = (c >> 1) ^ (0xEDB8_8320 & (0u32.wrapping_sub(c & 1))); }
    }
    !c
}

fn b64(data: &[u8]) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut line = String::new();
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        line.push(T[(n >> 18) as usize & 63] as char);
        line.push(T[(n >> 12) as usize & 63] as char);
        line.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        line.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
        if line.len() >= 76 { line.push('\n'); emit_logonly(&line); line.clear(); }
    }
    if !line.is_empty() { line.push('\n'); emit_logonly(&line); }
}

fn le16(b: &[u8], o: usize) -> usize { b[o] as usize | (b[o + 1] as usize) << 8 }

fn code_type(t: u8) -> &'static str {
    match t {
        0x00 => "x86 legacy BIOS", 0x01 => "Open Firmware", 0x02 => "HP PA-RISC", 0x03 => "EFI (GOP)",
        0xE0 => "NVIDIA-specific (0xE0)", 0x70 => "NVIDIA-specific (0x70)", _ => "other",
    }
}

/// Validate and walk the ROM: 0x55AA, PCI data structures, BIT table.
fn analyze_vbios(rom: &[u8], dump: bool, truncated: bool) {
    if rom.len() < 0x1C || rom[0] != 0x55 || rom[1] != 0xAA {
        nvlog!("VBIOS: no 0x55AA signature (first bytes {:02x} {:02x}), {} bytes read",
            rom.first().copied().unwrap_or(0), rom.get(1).copied().unwrap_or(0), rom.len());
        return;
    }
    let mut off = 0usize;
    let mut total = 0usize;
    let mut n = 0;
    loop {
        if off + 0x1C > rom.len() || rom[off] != 0x55 || rom[off + 1] != 0xAA {
            nvlog!("VBIOS image {} @0x{:x}: not a 0x55AA header; walk stops", n, off);
            break;
        }
        let pcir = off + le16(rom, off + 0x18);
        if pcir + 0x18 > rom.len() { nvlog!("VBIOS image {} @0x{:x}: PCIR beyond data read", n, off); break; }
        let sig = &rom[pcir..pcir + 4];
        let vendor = le16(rom, pcir + 4);
        let device = le16(rom, pcir + 6);
        let len = le16(rom, pcir + 0x10) * 512;
        let ctype = rom[pcir + 0x14];
        let last = rom[pcir + 0x15] & 0x80 != 0;
        nvlog!("VBIOS image {} @0x{:06x}: {} vendor=0x{:04x} device=0x{:04x} len=0x{:x} ({}) code-rev=0x{:04x} type=0x{:02x} {} last={}",
            n, off, core::str::from_utf8(sig).unwrap_or("????"), vendor, device, len, fmt_size(len as u64),
            le16(rom, pcir + 0x12), ctype, code_type(ctype), last as u8);
        if len == 0 { log("VBIOS: zero-length image, walk stops"); break; }
        total = off + len;
        n += 1;
        if last { break; }
        off += len;
        if n > 8 { break; }
    }
    nvlog!("VBIOS: {} image(s), declared total {} ({} bytes), read {} bytes{}",
        n, fmt_size(total as u64), total, rom.len(), if truncated { " (TRUNCATED by time budget)" } else { "" });
    let crc_end = if total != 0 { total.min(rom.len()) } else { rom.len() };
    nvlog!("VBIOS: CRC32 over first {} bytes = 0x{:08x}", crc_end, crc32(&rom[..crc_end]));

    // BIT table: 0xB8FF 'B' 'I' 'T' 0.
    let sig = [0xFFu8, 0xB8, b'B', b'I', b'T', 0];
    if let Some(p) = rom.windows(6).position(|w| w == sig) {
        if p + 12 <= rom.len() {
            let ver = le16(rom, p + 6);
            let hsz = rom[p + 8] as usize;
            let tsz = rom[p + 9] as usize;
            let cnt = rom[p + 10] as usize;
            nvlog!("BIT table @0x{:x}: version=0x{:04x} header={} token-size={} tokens={}", p, ver, hsz, tsz, cnt);
            for t in 0..cnt {
                let o = p + hsz + t * tsz;
                if tsz < 6 || o + 6 > rom.len() { break; }
                let id = rom[o];
                let c = if id.is_ascii_graphic() { id as char } else { '?' };
                nvlog!("  BIT token '{}' (0x{:02x}) ver={} size={} data@0x{:04x}", c, id, rom[o + 1], le16(rom, o + 2), le16(rom, o + 4));
            }
        }
    } else {
        log("BIT table: signature not found");
    }

    for row in 0..16.min((rom.len() + 15) / 16) {
        let mut s = format!("VBIOS {:04x}:", row * 16);
        for b in &rom[row * 16..(row * 16 + 16).min(rom.len())] { s.push_str(&format!(" {:02x}", b)); }
        log(&s);
    }
    if dump && total != 0 {
        nvlog!("raw VBIOS follows ({} bytes)", crc_end);
        // Persistent log ring only: pushing ~1.4 MB through the framebuffer
        // console took ~20 s and delayed boot.
        emit_logonly("-----BEGIN VBIOS-----\n");
        b64(&rom[..crc_end]);
        emit_logonly("-----END VBIOS-----\n");
    }
}

/// Read the VBIOS through the PROM window with ROM shadow disabled, as
/// nouveau's shadowrom.c does; restore the shadow dword afterwards.
unsafe fn read_prom(d: &Dev, bar0: usize, deadline: u64) -> (Vec<u8>, bool) {
    let orig = d.rd32(0x50);
    d.wr32(0x50, orig & !1);
    let mut v: Vec<u8> = Vec::new();
    let mut truncated = false;
    let first = rd(bar0, PROM_OFF);
    if first & 0xFFFF == 0xAA55 {
        v.reserve(PROM_MAX);
        let mut o = 0;
        while o < PROM_MAX {
            let w = rd(bar0, PROM_OFF + o);
            v.extend_from_slice(&w.to_le_bytes());
            o += 4;
            if o & 0xFFF == 0 && now() >= deadline { truncated = o < PROM_MAX; break; }
        }
    }
    d.wr32(0x50, orig);
    nvlog!("PROM: config 0x50 was 0x{:08x}, forced to 0x{:08x} for the read, restored (now 0x{:08x})",
        orig, orig & !1, d.rd32(0x50));
    (v, truncated)
}

/// Fallback: the PCI expansion ROM BAR (enable bit toggled, then restored).
unsafe fn read_rom_bar(d: &Dev, rom_orig: u32, rom_size: u64, deadline: u64) -> (Vec<u8>, bool) {
    let mut addr = (rom_orig & 0xFFFF_F800) as u64;
    let mut placed = false;
    if rom_size == 0 { log("expansion ROM BAR: not implemented"); return (Vec::new(), false); }
    // A value equal to the sizing read-back (top of 4 GiB, firmware flash) was
    // never programmed with a real address: some firmware leaves it like that.
    if addr == (0x1_0000_0000u64 - rom_size) { addr = 0; }
    if addr == 0 {
        match opt("nvprobe.romaddr") {
            Some(a) if a & (rom_size - 1) == 0 && a < (1 << 32) => { addr = a; placed = true; }
            _ => { log("expansion ROM BAR: unassigned by firmware, skipping"); return (Vec::new(), false); }
        }
    }
    let cmd = d.rd16(0x04);
    d.wr16(0x04, cmd | 0x2);
    d.wr32(0x30, addr as u32 | 1);
    let mut v = Vec::new();
    let mut truncated = false;
    let len = rom_size.min(PROM_MAX as u64) as usize;
    let flags = mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::MMIO;
    match mm::paging::map_kernel_device(addr as usize, len, flags) {
        Some(va) => {
            let first = core::ptr::read_volatile(va as *const u32);
            if first & 0xFFFF == 0xAA55 {
                v.reserve(len);
                let mut o = 0;
                while o < len {
                    let w = core::ptr::read_volatile((va + o) as *const u32);
                    v.extend_from_slice(&w.to_le_bytes());
                    o += 4;
                    if o & 0xFFF == 0 && now() >= deadline { truncated = o < len; break; }
                }
            }
        }
        None => log("expansion ROM BAR: cannot map (inside a huge-page HHDM mapping?)"),
    }
    d.wr32(0x30, rom_orig);
    d.wr16(0x04, cmd);
    nvlog!("expansion ROM BAR at 0x{:x} ({}){}, restored to 0x{:08x}", addr, fmt_size(rom_size),
        if placed { " temporarily placed by nvprobe.romaddr" } else { "" }, d.rd32(0x30));
    (v, truncated)
}

// ── entry point ──────────────────────────────────────────────────────────────

/// Run the probe. `rsdp`/`hhdm` locate the ACPI MCFG table on x86_64;
/// `fb_phys`/`fb_w`/`fb_h`/`fb_pitch` describe the bootloader framebuffer.
pub fn run(rsdp: u64, hhdm: u64, fb_phys: u64, fb_w: u32, fb_h: u32, fb_pitch: u32) {
    if opt("nvprobe.off") == Some(1) { return; }
    let vendor = opt("nvprobe.vendor").map(|v| v as u16).unwrap_or(NV_VENDOR);
    let generic = vendor != NV_VENDOR;
    let devs: Vec<PciDevice> = pci::scan().into_iter()
        .filter(|d| d.vendor_id == vendor && d.class == 0x03)
        .collect();
    if devs.is_empty() { return; }
    let t_start = now();
    let dump = opt("nvprobe.dumpvbios") == Some(1);
    let deadline = t_start + if dump { BUDGET_DUMP_NS } else { BUDGET_NS };
    nvlog!("=== nvprobe start: {} display function(s) with vendor 0x{:04x}{} ===",
        devs.len(), vendor, if generic { " (generic mode: no BAR0 register/PROM access)" } else { "" });
    for pd in devs.iter().take(2) {
        unsafe { probe_one(pd, rsdp, hhdm, generic, dump, deadline, fb_phys, fb_w, fb_h, fb_pitch); }
    }
    nvlog!("=== nvprobe done in {} ms ===", now().wrapping_sub(t_start) / 1_000_000);
}

unsafe fn probe_one(pd: &PciDevice, rsdp: u64, hhdm: u64, generic: bool, dump: bool, deadline: u64,
                    fb_phys: u64, fb_w: u32, fb_h: u32, fb_pitch: u32) {
    let ext = map_ecam(pd, rsdp, hhdm).unwrap_or(0);
    let mut d = Dev { bus: pd.bus, dev: pd.dev, func: pd.func, ext };
    // Sanity: the ECAM page must show the same vendor/device as legacy config.
    if d.ext != 0 {
        let e = core::ptr::read_volatile(d.ext as *const u32);
        if e != d.rd32(0) { log("ECAM page does not match legacy config (wrong base?); ignoring ECAM"); d.ext = 0; }
    }
    let ids = d.rd32(0);
    let sub = d.rd32(0x2C);
    let cr = d.rd32(0x08);
    nvlog!("device {:02x}:{:02x}.{} vendor=0x{:04x} device=0x{:04x} subsys=0x{:04x}:0x{:04x} rev=0x{:02x} class={:02x}{:02x}{:02x} hdr=0x{:02x}",
        d.bus, d.dev, d.func, ids & 0xFFFF, ids >> 16, sub & 0xFFFF, sub >> 16, cr & 0xFF,
        cr >> 24, (cr >> 16) & 0xFF, (cr >> 8) & 0xFF, d.rd8(0x0E));
    let cmd0 = d.rd16(0x04);
    nvlog!("COMMAND=0x{:04x} (io={} mem={} busmaster={}) STATUS=0x{:04x}", cmd0, cmd0 & 1, (cmd0 >> 1) & 1, (cmd0 >> 2) & 1, d.rd16(0x06));
    if !generic {
        match ids >> 16 {
            0x2484 | 0x2488 => log("device id matches GeForce RTX 3070 (GA104 / GA104 LHR)"),
            0x228b => log("this is the HDMI audio function id, not the GPU"),
            _ => log("NVIDIA device id is not the expected RTX 3070 (0x2484/0x2488); continuing"),
        }
    }

    let (bars, rom_orig, rom_size) = size_bars(&d);
    for b in &bars {
        if b.io {
            nvlog!("BAR{}: io base=0x{:x} size={}", b.idx, b.base, b.size);
        } else {
            let st = if b.base == 0 { " UNASSIGNED" } else { "" };
            nvlog!("BAR{}: mem{} {} base=0x{:x} size={} (0x{:x}){}", b.idx, if b.is64 { "64" } else { "32" },
                if b.pf { "prefetchable" } else { "non-prefetchable" }, b.base, fmt_size(b.size), b.size, st);
        }
    }
    nvlog!("expansion ROM BAR: raw=0x{:08x} size={}", rom_orig, fmt_size(rom_size));
    let cmd1 = d.rd16(0x04);
    if cmd1 != cmd0 { nvlog!("WARNING: COMMAND changed across BAR sizing (0x{:04x} -> 0x{:04x})", cmd0, cmd1); }
    else { nvlog!("COMMAND restored after BAR sizing (0x{:04x})", cmd1); }

    log_caps(&d, d.ext != 0);

    // Framebuffer vs BARs (works in both modes).
    let mut fb_in_bar = false;
    for b in bars.iter().filter(|b| !b.io && b.base != 0) {
        if fb_phys >= b.base && fb_phys < b.base + b.size {
            nvlog!("GOP framebuffer at BAR{}+0x{:x} (fb phys 0x{:x}, {}x{} pitch {}, {} bytes; BAR{} base 0x{:x} size {}); {}",
                b.idx, fb_phys - b.base, fb_phys, fb_w, fb_h, fb_pitch, fb_pitch as u64 * fb_h as u64, b.idx, b.base,
                fmt_size(b.size), if b.size <= 256 << 20 { "BAR is 256 MiB or smaller (ReBAR off)" } else { "BAR is large (ReBAR on?)" });
            fb_in_bar = true;
        }
    }
    if !fb_in_bar {
        nvlog!("framebuffer phys 0x{:x} is not inside any BAR of this function", fb_phys);
    }

    if generic {
        // Expansion ROM only; no BAR0 pokes on a foreign device.
        let (rom, trunc) = read_rom_bar(&d, rom_orig, rom_size, deadline);
        if rom.is_empty() { log("VBIOS: nothing read (generic mode)"); } else { analyze_vbios(&rom, dump, trunc); }
        return;
    }

    // BAR0 registers.
    let b0 = match bars.iter().find(|b| b.idx == 0 && !b.io) {
        Some(b) if b.base != 0 && b.size >= BAR0_MIN => b,
        Some(b) => { nvlog!("BAR0 unusable (base=0x{:x} size={}); skipping register and PROM access", b.base, fmt_size(b.size)); return; }
        None => { log("no BAR0; skipping register and PROM access"); return; }
    };
    if d.rd16(0x04) & 2 == 0 {
        log("memory decode was off; enabling it (left on)");
        d.wr16(0x04, d.rd16(0x04) | 2);
    }
    let flags = mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO;
    let bar0 = match mm::paging::map_kernel_device(b0.base as usize, BAR0_MIN as usize, flags) {
        Some(v) => v,
        None => { log("cannot map BAR0 (page-table conflict); skipping register and PROM access"); return; }
    };
    if !probe_regs(bar0) { return; }

    let (mut rom, mut trunc) = read_prom(&d, bar0, deadline);
    if rom.is_empty() {
        log("PROM window did not show 0x55AA; falling back to the expansion ROM BAR");
        let r = read_rom_bar(&d, rom_orig, rom_size, deadline);
        rom = r.0;
        trunc = r.1;
    }
    if rom.is_empty() { log("VBIOS: could not be read by either method"); } else { analyze_vbios(&rom, dump, trunc); }
}
