//! Power device discovery, evaluation and event polling on top of `aml`.

extern crate alloc;
use super::aml::{self, Host, Ns, V};
use super::{ec, BatteryInfo, ST_CHARGING, ST_DISCHARGING, ST_FULL, ST_NOT_CHARGING, ST_UNKNOWN};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

static SINK: AtomicUsize = AtomicUsize::new(0);
static SEQ: AtomicU64 = AtomicU64::new(0);
static STATE: Mutex<Option<Wrap>> = Mutex::new(None);
/// SystemMemory pages mapped for operation regions: (phys page, kernel va).
/// Raw firmware tables for `/sys/firmware/acpi/tables` (Linux names: DSDT, SSDT1..).
static RAW: Mutex<Vec<(String, &'static [u8])>> = Mutex::new(Vec::new());

/// The `idx`-th raw ACPI table: (file name, bytes).
pub fn raw_table(idx: usize) -> Option<(String, &'static [u8])> {
    RAW.lock().get(idx).cloned()
}

fn raw_add(t: &'static [u8]) {
    let sig = String::from_utf8_lossy(&t[..4]).into_owned();
    let mut raw = RAW.lock();
    let name = if sig == "SSDT" {
        format!("SSDT{}", 1 + raw.iter().filter(|(n, _)| n.starts_with("SSDT")).count())
    } else { sig };
    if !raw.iter().any(|(n, _)| *n == name) { raw.push((name, t)); }
}

static MAPS: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());

const EV_KEY: u16 = 1;
const EV_SW: u16 = 5;
const KEY_POWER: u16 = 116;
const KEY_SLEEP: u16 = 142;
const KEY_BRIGHTNESSDOWN: u16 = 224;
const KEY_BRIGHTNESSUP: u16 = 225;
const SW_LID: u16 = 0;

struct Bat {
    dev: usize,
    info: Option<Vec<V>>,
    bix: bool,
    info_at: u64,
}

struct State {
    ns: Ns,
    bats: Vec<Bat>,
    ac: Option<usize>,
    lid: Option<usize>,
    lid_open: Option<bool>,
    pwrbtn: Option<usize>,
    slpbtn: Option<usize>,
    video: Option<usize>,
    levels: Vec<u64>,
    cur_level: Option<u64>,
    ec_dev: Option<usize>,
    ec_gpe: Option<u32>,
    pm1a_evt: u16,
    pm1b_evt: u16,
    gpe0: u16,
    gpe0_len: u16,
    fixed_pwr: bool,
    fixed_slp: bool,
    gpe_methods: bool,
    gpe_storm: [u16; 128],
    last_slow: u64,
    ac_last: Option<bool>,
}

/// The interpreter holds `Rc`s; every access is serialized by STATE's lock.
struct Wrap(State);
unsafe impl Send for Wrap {}

fn log(s: &str) { crate::i915::emit_logonly(s); }
/// Console + klog (the one-time discovery summary).
static EC_FAIL_LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
fn say(s: &str) { crate::i915::emit(s); }
fn now_ns() -> u64 { crate::i915::now_ns() }

#[cfg(target_arch = "x86_64")]
mod pio {
    pub fn inb(p: u16) -> u8 { let v: u8; unsafe { core::arch::asm!("in al, dx", out("al") v, in("dx") p, options(nomem, nostack)); } v }
    pub fn inw(p: u16) -> u16 { let v: u16; unsafe { core::arch::asm!("in ax, dx", out("ax") v, in("dx") p, options(nomem, nostack)); } v }
    pub fn inl(p: u16) -> u32 { let v: u32; unsafe { core::arch::asm!("in eax, dx", out("eax") v, in("dx") p, options(nomem, nostack)); } v }
    pub fn outb(p: u16, v: u8) { unsafe { core::arch::asm!("out dx, al", in("dx") p, in("al") v, options(nomem, nostack)); } }
    pub fn outw(p: u16, v: u16) { unsafe { core::arch::asm!("out dx, ax", in("dx") p, in("ax") v, options(nomem, nostack)); } }
    pub fn outl(p: u16, v: u32) { unsafe { core::arch::asm!("out dx, eax", in("dx") p, in("eax") v, options(nomem, nostack)); } }
}
#[cfg(not(target_arch = "x86_64"))]
mod pio {
    pub fn inb(_p: u16) -> u8 { 0xFF }
    pub fn inw(_p: u16) -> u16 { 0xFFFF }
    pub fn inl(_p: u16) -> u32 { !0 }
    pub fn outb(_p: u16, _v: u8) {}
    pub fn outw(_p: u16, _v: u16) {}
    pub fn outl(_p: u16, _v: u32) {}
}

fn mem_va(phys: u64) -> Option<usize> {
    let page = phys & !0xFFF;
    let mut m = MAPS.lock();
    if let Some(&(_, va)) = m.iter().find(|(p, _)| *p == page) { return Some(va + (phys & 0xFFF) as usize); }
    if m.len() >= 64 { return None; }
    let va = mm::kva::ioremap(page, 0x2000, mm::kva::CacheMode::Uncached)?;
    m.push((page, va));
    Some(va + (phys & 0xFFF) as usize)
}


// ---- sleep-method support (lane s3aml) ----
/// Per-space OpRegion accounting: [0..16) reads, [16..32) writes, [32..48) dropped accesses.
static CNT: [core::sync::atomic::AtomicU32; 48] = [const { core::sync::atomic::AtomicU32::new(0) }; 48];
static PAUSED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Inside _TTS/_PTS/_WAK: Sleep() honours up to 2000 ms, writes may be traced.
static SLEEP_WIN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static TRACE_LEFT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static FACS_PHYS: AtomicU64 = AtomicU64::new(0);
static PM1A_CNT: AtomicUsize = AtomicUsize::new(0);
static GL_FAILS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static GL_PENDING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn cnt(i: usize) { CNT[i & 47].fetch_add(1, Ordering::Relaxed); }
fn space_name(s: usize) -> &'static str {
    match s { 0 => "Mem", 1 => "IO", 2 => "PCI", 3 => "EC", 4 => "SMBus", 5 => "CMOS", 6 => "PCIBar", 7 => "IPMI", 8 => "GPIO", 9 => "GSBus", 10 => "PCC", _ => "?" }
}
fn cmos_ports(addr: u64) -> (u16, u16, u8) { if addr >= 0x80 { (0x72, 0x73, (addr - 0x80) as u8) } else { (0x70, 0x71, addr as u8) } }
fn trace_write(space: u8, addr: u64, w: u8, val: u64) {
    if !SLEEP_WIN.load(Ordering::Relaxed) || crate::nvidia_probe::cmdline_word("pm.amltrace") != Some("1") { return; }
    if TRACE_LEFT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1)).is_ok() {
        log(&format!("[S3] aml: write {} {:#x} w{} = {:#x}\n", space_name(space as usize), addr, w, val));
    }
}

/// FACS global lock (ACPI 5.2.10.1): cmpxchg on FACS+16; bit0 pending, bit1 owned.
static GL_DEPTH: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
fn gl_acquire() {
    if GL_DEPTH.fetch_add(1, Ordering::Relaxed) > 0 { return; }
    let phys = FACS_PHYS.load(Ordering::Relaxed);
    if phys == 0 { return; }
    let Some(va) = mem_va(phys + 16) else { return };
    let a = unsafe { &*(va as *const core::sync::atomic::AtomicU32) };
    for _ in 0..100_000u32 {
        let old = a.load(Ordering::Acquire);
        let new = (old & !3) | 2 | if old & 2 != 0 { 1 } else { 0 };
        if a.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            if new & 1 == 0 { GL_PENDING.store(false, Ordering::Relaxed); return; }
            // Owned by firmware: pending set, wait for its release.
            GL_PENDING.store(true, Ordering::Relaxed);
        }
        core::hint::spin_loop();
        crate::i915::delay_us(1);
    }
    GL_FAILS.fetch_add(1, Ordering::Relaxed);
}
fn gl_release() {
    if GL_DEPTH.fetch_sub(1, Ordering::Relaxed) > 1 { return; }
    let phys = FACS_PHYS.load(Ordering::Relaxed);
    if phys == 0 { return; }
    let Some(va) = mem_va(phys + 16) else { return };
    let a = unsafe { &*(va as *const core::sync::atomic::AtomicU32) };
    let old = a.fetch_and(!3, Ordering::AcqRel);
    if old & 1 != 0 {
        let p = PM1A_CNT.load(Ordering::Relaxed) as u16;
        if p != 0 { pio::outw(p, pio::inw(p) | (1 << 2)); } // GBL_RLS
    }
}

struct KHost {
    notes: Vec<(usize, u64)>,
}

fn pci_fn(addr: u64) -> crate::pci::PciFunction {
    crate::pci::PciFunction::new((addr >> 32) as u8, ((addr >> 24) & 0x1F) as u8, ((addr >> 16) & 7) as u8)
}

impl Host for KHost {
    fn read(&mut self, space: u8, addr: u64, width: u8) -> u64 {
        cnt(space as usize);
        match space {
            aml::SPACE_MEM => {
                let Some(va) = mem_va(addr) else { return 0 };
                unsafe {
                    match width {
                        8 => core::ptr::read_volatile(va as *const u8) as u64,
                        16 => core::ptr::read_volatile(va as *const u16) as u64,
                        32 => core::ptr::read_volatile(va as *const u32) as u64,
                        _ => core::ptr::read_volatile(va as *const u64),
                    }
                }
            }
            aml::SPACE_IO => {
                let p = addr as u16;
                match width { 8 => pio::inb(p) as u64, 16 => pio::inw(p) as u64, 32 => pio::inl(p) as u64, _ => pio::inl(p) as u64 | (pio::inl(p + 4) as u64) << 32 }
            }
            aml::SPACE_PCI => {
                let off = (addr & 0xFFF) as u16;
                let f = pci_fn(addr);
                match width { 8 => f.read8(off) as u64, 16 => f.read16(off) as u64, _ => f.read32(off) as u64 }
            }
            aml::SPACE_EC => {
                let mut v = 0u64;
                for i in 0..(width / 8) as u64 {
                    let b = match ec::read((addr + i) as u8) { Some(b) => b, None => { if !EC_FAIL_LOGGED.swap(true, Ordering::Relaxed) { say(&format!("acpi: EC read of {:#04x} FAILED (reads as 0; further failures counted in timeouts)\n", addr + i)); } 0 } };
                    v |= (b as u64) << (8 * i);
                }
                v
            }
            aml::SPACE_CMOS => {
                let mut v = 0u64;
                for i in 0..(width / 8) as u64 {
                    let (ip, dp, idx) = cmos_ports(addr + i);
                    pio::outb(ip, idx & 0x7F);
                    v |= (pio::inb(dp) as u64) << (8 * i);
                }
                v
            }
            _ => { cnt(32 + space as usize); 0 }
        }
    }

    fn write(&mut self, space: u8, addr: u64, width: u8, val: u64) {
        cnt(16 + space as usize);
        trace_write(space, addr, width, val);
        match space {
            aml::SPACE_MEM => {
                let Some(va) = mem_va(addr) else { return };
                unsafe {
                    match width {
                        8 => core::ptr::write_volatile(va as *mut u8, val as u8),
                        16 => core::ptr::write_volatile(va as *mut u16, val as u16),
                        32 => core::ptr::write_volatile(va as *mut u32, val as u32),
                        _ => core::ptr::write_volatile(va as *mut u64, val),
                    }
                }
            }
            aml::SPACE_IO => {
                let p = addr as u16;
                match width { 8 => pio::outb(p, val as u8), 16 => pio::outw(p, val as u16), _ => pio::outl(p, val as u32) }
            }
            aml::SPACE_PCI => {
                let off = (addr & 0xFFF) as u16;
                let f = pci_fn(addr);
                // Narrow writes as read-modify-write of the dword.
                let sh = ((off & 3) * 8) as u32;
                let m: u32 = match width { 8 => 0xFF, 16 => 0xFFFF, _ => !0 };
                let old = if width >= 32 { 0 } else { f.read32(off & !3) };
                f.write32(off & !3, (old & !(m << sh)) | (((val as u32) & m) << sh));
            }
            aml::SPACE_EC => {
                for i in 0..(width / 8) as u64 { ec::write((addr + i) as u8, (val >> (8 * i)) as u8); }
            }
            aml::SPACE_CMOS if crate::nvidia_probe::cmdline_word("pm.aml_cmos_write") == Some("1") => {
                for i in 0..(width / 8) as u64 {
                    let (ip, dp, idx) = cmos_ports(addr + i);
                    pio::outb(ip, idx & 0x7F);
                    pio::outb(dp, (val >> (8 * i)) as u8);
                }
            }
            _ => cnt(32 + space as usize), // CMOS writes (unless allowed), SMBus, GPIO, GSBus, PCC ...
        }
    }

    fn sleep_ms(&mut self, ms: u64) {
        let cap = if SLEEP_WIN.load(Ordering::Relaxed) { 2000 } else { 100 };
        crate::i915::delay_us(ms.min(cap) * 1000);
    }
    fn global_lock(&mut self, acquire: bool) { if acquire { gl_acquire() } else { gl_release() } }
    fn stall_us(&mut self, us: u64) { crate::i915::delay_us(us.min(100)); }
    fn timer(&mut self) -> u64 { now_ns() / 100 }
    fn notify(&mut self, node: usize, code: u64) { if self.notes.len() < 64 { self.notes.push((node, code)); } }
    fn log(&mut self, msg: &str) { log(msg); }
}

fn rd16(t: &[u8], o: usize) -> u16 { if o + 2 <= t.len() { u16::from_le_bytes([t[o], t[o + 1]]) } else { 0 } }
fn rd32(t: &[u8], o: usize) -> u32 { if o + 4 <= t.len() { u32::from_le_bytes([t[o], t[o + 1], t[o + 2], t[o + 3]]) } else { 0 } }
fn rd64(t: &[u8], o: usize) -> u64 { if o + 8 <= t.len() { u64::from_le_bytes(t[o..o + 8].try_into().unwrap()) } else { 0 } }

unsafe fn table(rsdp: u64, hhdm: u64, sig: &[u8; 4]) -> Option<&'static [u8]> {
    let t = boot::acpi::find_table(rsdp, hhdm, sig)?;
    let len = core::ptr::read_unaligned(t.add(4) as *const u32) as usize;
    if !(36..(4 << 20)).contains(&len) { return None; }
    Some(core::slice::from_raw_parts(t, len))
}

fn hid_matches(ns: &mut Ns, h: &mut KHost, dev: usize, ids: &[&str]) -> bool {
    for key in ["_HID", "_CID"] {
        if ns.child(dev, key).is_none() { continue; }
        let Some(v) = ns.eval_child(h, dev, key, &[]) else { continue };
        let one = |v: &V| -> bool {
            match v {
                V::Int(i) => ids.iter().any(|s| aml::eisa_id(s) == *i),
                o => o.as_bytes().map_or(false, |b| ids.iter().any(|s| s.as_bytes() == &b[..])),
            }
        };
        let hit = match &v { V::Pkg(p) => p.borrow().iter().any(|e| one(e)), o => one(o) };
        if hit { return true; }
    }
    false
}

pub fn init(rsdp: u64, hhdm: u64) {
    if rsdp == 0 { return; }
    let t0 = now_ns();
    let mut h = KHost { notes: Vec::new() };
    let mut ns = Ns::new();
    let fadt = unsafe { table(rsdp, hhdm, b"FACP") };
    let (mut pm1a_evt, mut pm1b_evt, mut gpe0, mut gpe0_len, mut flags) = (0u16, 0u16, 0u16, 0u16, 0u32);
    if let Some(f) = fadt {
        raw_add(f);
        pm1a_evt = rd32(f, 56) as u16;
        pm1b_evt = rd32(f, 60) as u16;
        let pm1a_cnt = rd32(f, 64) as u16;
        PM1A_CNT.store(pm1a_cnt as usize, Ordering::Relaxed);
        let facs = if f.len() >= 140 && rd64(f, 132) != 0 { rd64(f, 132) } else { rd32(f, 36) as u64 };
        FACS_PHYS.store(facs, Ordering::Relaxed);
        gpe0 = rd32(f, 80) as u16;
        gpe0_len = f.get(92).copied().unwrap_or(0) as u16;
        flags = rd32(f, 112);
        let smi_cmd = rd32(f, 48) as u16;
        let enable = f.get(52).copied().unwrap_or(0);
        if pm1a_cnt != 0 && pio::inw(pm1a_cnt) & 1 == 0 && smi_cmd != 0 && enable != 0 {
            pio::outb(smi_cmd, enable);
            for _ in 0..300 { if pio::inw(pm1a_cnt) & 1 != 0 { break; } crate::i915::delay_us(1000); }
            log(&format!("acpi: SCI_EN was clear, ACPI enable -> PM1a_CNT={:#x}\n", pio::inw(pm1a_cnt)));
        }
    }
    // The EC first (ECDT): code run while loading may touch EC fields.
    let mut ec_gpe = None;
    if let Some(e) = unsafe { table(rsdp, hhdm, b"ECDT") } {
        raw_add(e);
        let (cmd, data) = (rd64(e, 40) as u16, rd64(e, 52) as u16);
        if cmd != 0 && data != 0 { ec::set_ports(data, cmd); ec_gpe = Some(e.get(64).copied().unwrap_or(0) as u32); }
    }
    unsafe {
        boot::acpi::for_each_aml_table(rsdp, hhdm, &mut |t: &[u8]| {
            let t: &'static [u8] = core::slice::from_raw_parts(t.as_ptr(), t.len());
            if t.len() >= 36 { raw_add(t); }
            ns.load_table(&mut h, t);
        });
    }
    let load_errs = ns.errors;
    let mut st = State {
        ns: Ns::new(), bats: Vec::new(), ac: None, lid: None, lid_open: None, pwrbtn: None, slpbtn: None,
        video: None, levels: Vec::new(), cur_level: None, ec_dev: None, ec_gpe, pm1a_evt, pm1b_evt, gpe0, gpe0_len,
        fixed_pwr: flags & (1 << 4) == 0, fixed_slp: flags & (1 << 5) == 0,
        gpe_methods: crate::nvidia_probe::cmdline_word("acpi_power.gpe").map_or(false, |w| w == "1"),
        gpe_storm: [0; 128], last_slow: 0, ac_last: None,
    };
    for n in 1..ns.len() {
        if !ns.is_real_device(n) { continue; }
        if st.video.is_none() && ns.child(n, "_BCL").is_some() && ns.child(n, "_BCM").is_some() { st.video = Some(n); }
        if ns.child(n, "_HID").is_none() && ns.child(n, "_CID").is_none() { continue; }
        if hid_matches(&mut ns, &mut h, n, &["PNP0C0A"]) { st.bats.push(Bat { dev: n, info: None, bix: false, info_at: 0 }); }
        else if hid_matches(&mut ns, &mut h, n, &["ACPI0003"]) { st.ac.get_or_insert(n); }
        else if hid_matches(&mut ns, &mut h, n, &["PNP0C0D"]) { st.lid.get_or_insert(n); }
        else if hid_matches(&mut ns, &mut h, n, &["PNP0C0C"]) { st.pwrbtn.get_or_insert(n); }
        else if hid_matches(&mut ns, &mut h, n, &["PNP0C0E"]) { st.slpbtn.get_or_insert(n); }
        else if hid_matches(&mut ns, &mut h, n, &["PNP0C09"]) { st.ec_dev.get_or_insert(n); }
    }
    if let Some(e) = st.ec_dev {
        if !ec::present() {
            // _CRS: the first two I/O port descriptors are data, command.
            if let Some(V::Buf(b)) = ns.eval_child(&mut h, e, "_CRS", &[]) {
                let b = b.borrow();
                let mut ports = Vec::new();
                let mut i = 0;
                while i < b.len() {
                    let tag = b[i];
                    if tag & 0x80 != 0 { i += 3 + rd16(&b, i + 1) as usize; continue; }
                    let len = (tag & 7) as usize;
                    match tag >> 3 { 0x08 => ports.push(rd16(&b, i + 2)), 0x09 => ports.push(rd16(&b, i + 1)), 0x0F => break, _ => {} }
                    i += 1 + len;
                }
                if ports.len() >= 2 { ec::set_ports(ports[0], ports[1]); }
            }
        }
        if let Some(g) = ns.eval_child(&mut h, e, "_GPE", &[]).and_then(|v| v.as_int()) { st.ec_gpe = Some(g as u32); }
        ec::flush();
    }
    if crate::nvidia_probe::cmdline_word("acpi_power.trace").map_or(false, |w| w == "1") { ec::TRACE.store(true, Ordering::Relaxed); }
    if ec::present() && crate::nvidia_probe::cmdline_word("acpi_power.ecdump").map_or(false, |w| w == "1") { ec::dump(); }
    // Initialization, as an OS would: \_SB._INI, EC _REG, \_SB.PCI0._INI.
    if let Some(sb) = ns.find("\\_SB_") { if ns.child(sb, "_INI").is_some() { ns.eval_child(&mut h, sb, "_INI", &[]); } }
    if ec::present() {
        let mut regs = Vec::new();
        for n in 1..ns.len() {
            if ns.region_space(n) == Some(aml::SPACE_EC) {
                let p = ns.parent(n);
                if ns.child(p, "_REG").is_some() && !regs.contains(&p) { regs.push(p); }
            }
        }
        if let Some(e) = st.ec_dev { if ns.child(e, "_REG").is_some() && !regs.contains(&e) { regs.push(e); } }
        for p in regs { ns.eval_child(&mut h, p, "_REG", &[V::Int(3), V::Int(1)]); }
    }
    let mut inits: Vec<usize> = Vec::new();
    if let Some(p) = ns.find("\\_SB_.PCI0") { inits.push(p); }
    inits.extend(st.ec_dev);
    inits.extend(st.bats.iter().map(|b| b.dev));
    inits.extend(st.ac);
    for d in inits { if ns.child(d, "_INI").is_some() { ns.eval_child(&mut h, d, "_INI", &[]); } }
    if let Some(v) = st.video {
        if let Some(V::Pkg(p)) = ns.eval_child(&mut h, v, "_BCL", &[]) {
            let els = p.borrow().clone();
            let mut lv: Vec<u64> = els.iter().skip(2).filter_map(|e| ns.int_of(&mut h, e)).collect();
            lv.sort_unstable();
            lv.dedup();
            st.levels = lv;
        }
        if st.levels.len() < 2 { st.video = None; }
    }
    if let Some(l) = st.lid { st.lid_open = ns.eval_child(&mut h, l, "_LID", &[]).and_then(|v| v.as_int()).map(|v| v != 0); }
    let path = |o: Option<usize>, ns: &Ns| o.map(|a| ns.path(a)).unwrap_or_else(|| String::from("-"));
    say(&format!(
        "acpi: namespace {} nodes, {} load errors, {} init errors, {} ms; EC {} (status {:#x}, GPE {:?}, timeouts {}); batteries [{}]; AC {}; lid {} (open {:?}); power button {}; sleep button {}; video {} ({} levels); PM1a_EVT {:#x} GPE0 {:#x}/{}\n",
        ns.len(), load_errs, ns.errors - load_errs, (now_ns() - t0) / 1_000_000, ec::present(), ec::status(), st.ec_gpe,
        ec::TIMEOUTS.load(Ordering::Relaxed),
        st.bats.iter().map(|b| ns.path(b.dev)).collect::<Vec<_>>().join(","),
        path(st.ac, &ns), path(st.lid, &ns), st.lid_open,
        if st.fixed_pwr { String::from("fixed") } else { path(st.pwrbtn, &ns) },
        if st.fixed_slp { String::from("fixed") } else { path(st.slpbtn, &ns) },
        path(st.video, &ns), st.levels.len(), pm1a_evt, gpe0, gpe0_len));
    st.ns = ns;
    *STATE.lock() = Some(Wrap(st));
    if let Some(b) = battery(0) {
        say(&format!("acpi: BAT0 present={} status={} capacity={}% now={} full={} design={} rate={} mV={} model={:?} rechargeable={}\n",
            b.present, b.status, b.capacity, b.now, b.full, b.full_design, b.rate, b.voltage_now / 1000, b.model, b.rechargeable));
    }
    if ac_present() { say(&format!("acpi: AC online={:?}\n", ac_online())); }
}

fn with<T>(f: impl FnOnce(&mut State, &mut KHost) -> T) -> Option<T> {
    let mut g = STATE.lock();
    let st = &mut g.as_mut()?.0;
    let mut h = KHost { notes: Vec::new() };
    let r = f(st, &mut h);
    let notes = core::mem::take(&mut h.notes);
    handle_notes(st, &mut h, notes);
    Some(r)
}

/// Run `f` on the loaded namespace (for other drivers' _CRS/_DSM/_STA/_PSx).
pub fn with_ns<T>(f: impl FnOnce(&mut Ns, &mut dyn Host) -> T) -> Option<T> {
    with(|s, h| f(&mut s.ns, h))
}

pub fn battery_count() -> usize { with(|s, _| s.bats.len()).unwrap_or(0) }
pub fn ac_present() -> bool { with(|s, _| s.ac.is_some()).unwrap_or(false) }

pub fn ac_online() -> Option<bool> {
    with(|s, h| {
        let a = s.ac?;
        s.ns.eval_child(h, a, "_PSR", &[]).and_then(|v| v.as_int()).map(|v| v != 0)
    }).flatten()
}

pub fn lid_open() -> Option<bool> {
    with(|s, h| {
        let l = s.lid?;
        s.ns.eval_child(h, l, "_LID", &[]).and_then(|v| v.as_int()).map(|v| v != 0)
    }).flatten()
}

fn int_at(ns: &mut Ns, h: &mut KHost, p: &[V], i: usize) -> u64 {
    p.get(i).and_then(|e| ns.int_of(h, e)).unwrap_or(0xFFFF_FFFF)
}
fn str_at(ns: &mut Ns, h: &mut KHost, p: &[V], i: usize) -> String {
    let Some(e) = p.get(i) else { return String::new() };
    match ns.resolve(h, e) {
        V::Int(x) => format!("{}", x),
        o => o.as_bytes().map(|b| String::from_utf8_lossy(&b).trim().into()).unwrap_or_default(),
    }
}

pub fn battery(idx: usize) -> Option<BatteryInfo> {
    with(|s, h| {
        let b = s.bats.get_mut(idx)?;
        let dev = b.dev;
        let sta = if s.ns.child(dev, "_STA").is_some() { s.ns.eval_child(h, dev, "_STA", &[]).and_then(|v| v.as_int()).unwrap_or(0x1F) } else { 0x1F };
        let mut out = BatteryInfo { present: sta & 0x10 != 0, ..Default::default() };
        if !out.present { b.info = None; return Some(out); }
        if b.info.is_none() || now_ns().wrapping_sub(b.info_at) > 60_000_000_000 {
            let bix = if s.ns.child(dev, "_BIX").is_some() { s.ns.eval_child(h, dev, "_BIX", &[]).and_then(|v| v.elements()) } else { None };
            let (info, is_bix) = match bix {
                Some(p) if p.len() >= 20 => (Some(p), true),
                _ => (s.ns.eval_child(h, dev, "_BIF", &[]).and_then(|v| v.elements()), false),
            };
            b.info = info;
            b.bix = is_bix;
            b.info_at = now_ns();
        }
        let info = b.info.clone()?;
        let o = if b.bix { 1 } else { 0 };
        let norm = |x: u64| if x >= 0xFFFF_FFFF { 0 } else { x * 1000 };
        out.energy_units = int_at(&mut s.ns, h, &info, o) == 0;
        out.full_design = norm(int_at(&mut s.ns, h, &info, o + 1));
        out.full = norm(int_at(&mut s.ns, h, &info, o + 2));
        out.rechargeable = int_at(&mut s.ns, h, &info, o + 3) == 1;
        out.voltage_min_design = norm(int_at(&mut s.ns, h, &info, o + 4));
        let (mi, si, ti, oi) = if b.bix { (16, 17, 18, 19) } else { (9, 10, 11, 12) };
        if b.bix { let c = int_at(&mut s.ns, h, &info, 8); out.cycle_count = if c >= 0xFFFF_FFFF { 0 } else { c }; }
        out.model = str_at(&mut s.ns, h, &info, mi);
        out.serial = str_at(&mut s.ns, h, &info, si);
        out.kind = str_at(&mut s.ns, h, &info, ti);
        out.manufacturer = str_at(&mut s.ns, h, &info, oi);
        let bst = s.ns.eval_child(h, dev, "_BST", &[]).and_then(|v| v.elements()).unwrap_or_default();
        if bst.len() >= 4 {
            let state = int_at(&mut s.ns, h, &bst, 0);
            let rate = int_at(&mut s.ns, h, &bst, 1);
            // Some firmware reports a negative (16-bit two's complement) rate.
            let rate = if rate & 0x8000 != 0 && rate < 0x10000 { 0x10000 - rate } else { rate };
            out.rate = norm(rate);
            out.now = norm(int_at(&mut s.ns, h, &bst, 2));
            out.voltage_now = norm(int_at(&mut s.ns, h, &bst, 3));
            if out.full > 0 { out.capacity = ((out.now * 100 + out.full / 2) / out.full).min(100) as u32; }
            out.status = if state & 1 != 0 { ST_DISCHARGING }
                else if state & 2 != 0 { ST_CHARGING }
                else if out.full > 0 && out.capacity >= 95 { ST_FULL }
                else if out.full > 0 { ST_NOT_CHARGING }
                else { ST_UNKNOWN };
        }
        Some(out)
    }).flatten()
}

pub fn acpi_backlight() -> Option<(u32, u32)> {
    with(|s, h| {
        let v = s.video?;
        let cur = if s.ns.child(v, "_BQC").is_some() { s.ns.eval_child(h, v, "_BQC", &[]).and_then(|x| x.as_int()) } else { None };
        let cur = cur.or(s.cur_level).unwrap_or(*s.levels.last()?);
        let idx = s.levels.iter().position(|&l| l >= cur).unwrap_or(s.levels.len() - 1);
        Some((idx as u32, (s.levels.len() - 1) as u32))
    }).flatten()
}

pub fn acpi_backlight_set(idx: u32) -> bool {
    with(|s, h| {
        let Some(v) = s.video else { return false };
        let Some(&lvl) = s.levels.get(idx as usize) else { return false };
        let ok = s.ns.eval_child(h, v, "_BCM", &[V::Int(lvl)]).is_some();
        if ok { s.cur_level = Some(lvl); }
        ok
    }).unwrap_or(false)
}

pub fn set_input_sink(f: fn(u16, u16, i32)) { SINK.store(f as usize, Ordering::Release) }
pub fn change_seq() -> u64 { SEQ.load(Ordering::Relaxed) }

fn emit(ty: u16, code: u16, val: i32) {
    let f = SINK.load(Ordering::Acquire);
    if f == 0 { return; }
    let f: fn(u16, u16, i32) = unsafe { core::mem::transmute(f) };
    f(ty, code, val);
}
fn key(code: u16) { emit(EV_KEY, code, 1); emit(EV_KEY, code, 0); }

fn check_lid(s: &mut State, h: &mut KHost) {
    let Some(l) = s.lid else { return };
    let open = s.ns.eval_child(h, l, "_LID", &[]).and_then(|v| v.as_int()).map(|v| v != 0);
    if open.is_some() && open != s.lid_open {
        s.lid_open = open;
        emit(EV_SW, SW_LID, if open == Some(true) { 0 } else { 1 });
        log(&format!("acpi: lid {}\n", if open == Some(true) { "open" } else { "closed" }));
    }
}

fn handle_notes(s: &mut State, h: &mut KHost, mut notes: Vec<(usize, u64)>) {
    let mut rounds = 0;
    while !notes.is_empty() && rounds < 4 {
        rounds += 1;
        for (n, code) in core::mem::take(&mut notes) {
            if Some(n) == s.lid { check_lid(s, h); }
            else if Some(n) == s.pwrbtn && code == 0x80 { key(KEY_POWER); }
            else if Some(n) == s.slpbtn && code == 0x80 { key(KEY_SLEEP); }
            else if Some(n) == s.ac || s.bats.iter().any(|b| b.dev == n) {
                if code == 0x81 { for b in s.bats.iter_mut() { if b.dev == n { b.info = None; } } }
                SEQ.fetch_add(1, Ordering::Relaxed);
            } else if code == 0x86 { key(KEY_BRIGHTNESSUP); }
            else if code == 0x87 { key(KEY_BRIGHTNESSDOWN); }
            else if s.ns.notifies < 200 { log(&format!("acpi: Notify({}, {:#x})\n", s.ns.path(n), code)); }
        }
        notes.append(&mut h.notes);
    }
}

pub fn poll() {
    if PAUSED.load(Ordering::Relaxed) { return; }
    let _ = with(|s, h| {
        // Fixed-feature buttons (PM1 status, write-1-to-clear).
        for blk in [s.pm1a_evt, s.pm1b_evt] {
            if blk == 0 { continue; }
            let sts = pio::inw(blk);
            if s.fixed_pwr && sts & (1 << 8) != 0 { pio::outw(blk, 1 << 8); key(KEY_POWER); log("acpi: power button\n"); }
            if s.fixed_slp && sts & (1 << 9) != 0 { pio::outw(blk, 1 << 9); key(KEY_SLEEP); }
        }
        // EC queries -> _Qxx.
        if let Some(e) = s.ec_dev {
            let mut n = 0;
            while ec::present() && ec::status() & ec::SCI_EVT != 0 && n < 8 {
                n += 1;
                let q = ec::query();
                if q == 0 { break; }
                let m = format!("_Q{:02X}", q);
                if s.ns.child(e, &m).is_some() { s.ns.eval_child(h, e, &m, &[]); }
            }
        }
        // GPE0 status: the EC's bit is cleared (serviced above); other GPE
        // methods run only with acpi_power.gpe=1.
        if s.gpe0 != 0 && s.gpe0_len >= 2 {
            let half = (s.gpe0_len / 2).min(16);
            for i in 0..half {
                let sts = pio::inb(s.gpe0 + i);
                if sts == 0 { continue; }
                for b in 0..8u16 {
                    if sts & (1 << b) == 0 { continue; }
                    let g = (i * 8 + b) as u32;
                    if Some(g) == s.ec_gpe { pio::outb(s.gpe0 + i, 1 << b); continue; }
                    if !s.gpe_methods || g >= 128 || s.gpe_storm[g as usize] > 50 { continue; }
                    s.gpe_storm[g as usize] += 1;
                    let Some(gs) = s.ns.find("\\_GPE") else { continue };
                    let (l, e) = (format!("_L{:02X}", g), format!("_E{:02X}", g));
                    if s.ns.child(gs, &e).is_some() { pio::outb(s.gpe0 + i, 1 << b); s.ns.eval_child(h, gs, &e, &[]); }
                    else if s.ns.child(gs, &l).is_some() { s.ns.eval_child(h, gs, &l, &[]); pio::outb(s.gpe0 + i, 1 << b); }
                }
            }
        }
        // Once a second: lid and AC by polling, whatever the event path is.
        let now = now_ns();
        if now.wrapping_sub(s.last_slow) > 1_000_000_000 {
            s.last_slow = now;
            for g in s.gpe_storm.iter_mut() { *g = g.saturating_sub(1); }
            check_lid(s, h);
            if let Some(a) = s.ac {
                let on = s.ns.eval_child(h, a, "_PSR", &[]).and_then(|v| v.as_int()).map(|v| v != 0);
                if on != s.ac_last {
                    if s.ac_last.is_some() { SEQ.fetch_add(1, Ordering::Relaxed); log(&format!("acpi: AC online {:?}\n", on)); }
                    s.ac_last = on;
                }
            }
        }
    });
}

// ---- sleep methods (lane s3aml): _TTS / _PTS / _SST / _WAK / _Sx_ ----

/// Result of one sleep-method group.
#[derive(Clone, Debug, Default)]
pub struct SleepAml {
    /// No method failed.
    pub ok: bool,
    /// `_WAK` Package{status, PSS} (sleep_wake only).
    pub wak: Option<(u64, u64)>,
    pub ms: u64,
    /// Accesses to unsupported spaces that were dropped.
    pub dropped: u32,
}

pub fn pause_poll(on: bool) { PAUSED.store(on, Ordering::Relaxed); }

fn snap() -> [u32; 48] { let mut a = [0u32; 48]; for (i, c) in CNT.iter().enumerate() { a[i] = c.load(Ordering::Relaxed); } a }

fn diff_summary(a: &[u32; 48], b: &[u32; 48]) -> (String, u32) {
    let mut s = String::new();
    let mut dropped = 0;
    for sp in 0..16 {
        let (r, w, d) = (b[sp] - a[sp], b[16 + sp] - a[16 + sp], b[32 + sp] - a[32 + sp]);
        if r + w > 0 {
            if !s.is_empty() { s.push_str(", "); }
            s.push_str(&format!("{} r{} w{}", space_name(sp), r, w));
        }
        if d > 0 { dropped += d; s.push_str(&format!(" dropped {} {}", space_name(sp), d)); }
    }
    if s.is_empty() { s.push_str("no region access"); }
    (s, dropped)
}

/// Evaluate `path` (absolute) with `args` if it exists. Ok(None) = absent.
fn run_method(ns: &mut Ns, h: &mut KHost, path: &str, args: &[V]) -> Result<Option<V>, ()> {
    let Some(n) = ns.find(path) else { return Ok(None) };
    if !ns.is_method(n) { return Ok(None); }
    match ns.eval(h, n, args) {
        Ok(v) => Ok(Some(v)),
        Err(e) => { log(&format!("[S3] aml: {} failed: {:?}\n", path, e)); Err(()) }
    }
}

fn run_group(label: &str, steps: &[(&str, u64)], want_wak: bool) -> SleepAml {
    let mut out = SleepAml { ok: true, ..Default::default() };
    let t0 = now_ns();
    TRACE_LEFT.store(200, Ordering::Relaxed);
    SLEEP_WIN.store(true, Ordering::Relaxed);
    let before = snap();
    let r = with_ns_locked(|ns, h| {
        for &(path, arg) in steps {
            let t = now_ns();
            match run_method(ns, h, path, &[V::Int(arg)]) {
                Ok(None) => log(&format!("[S3] aml: {}({}) absent\n", path, arg)),
                Ok(Some(v)) => {
                    let mut extra = String::new();
                    if want_wak && path == "\\_WAK" {
                        let e = v.elements();
                        let (st, pss) = e.as_ref().map(|e| (e.get(0).and_then(|x| x.as_int()).unwrap_or(0), e.get(1).and_then(|x| x.as_int()).unwrap_or(0)))
                            .unwrap_or_else(|| (v.as_int().unwrap_or(0), 0));
                        out.wak = Some((st, pss));
                        extra = format!(" -> ({}, {}){}{}", st, pss, if st & 1 != 0 { " [wake failed: no power]" } else { "" }, if st & 2 != 0 { " [thermal]" } else { "" });
                    }
                    log(&format!("[S3] aml: {}({}) ok in {} ms{}\n", path, arg, (now_ns() - t) / 1_000_000, extra));
                }
                Err(()) => out.ok = false,
            }
        }
    });
    SLEEP_WIN.store(false, Ordering::Relaxed);
    if r.is_none() { out.ok = false; log("[S3] aml: interpreter not loaded\n"); }
    let (sum, dropped) = diff_summary(&before, &snap());
    out.dropped = dropped;
    out.ms = (now_ns() - t0) / 1_000_000;
    log(&format!("[S3] aml: {} {} in {} ms ({}; global lock fails {})\n", label, if out.ok { "ok" } else { "FAILED" }, out.ms, sum, GL_FAILS.load(Ordering::Relaxed)));
    out
}

fn with_ns_locked<T>(f: impl FnOnce(&mut Ns, &mut KHost) -> T) -> Option<T> { with(|s, h| f(&mut s.ns, h)) }

/// `_TTS(state)`, `_PTS(state)`, `\_SI._SST(3)` (kacpid paused by the caller via [`pause_poll`]).
pub fn sleep_prepare(state: u8) -> SleepAml {
    let st = state as u64;
    run_group("prepare", &[("\\_TTS", st), ("\\_PTS", st), ("\\_SI._SST", 3)], false)
}

/// `\_SI._SST(2)`, `_WAK(state)` -> (status, pss), `\_SI._SST(1)`.
pub fn sleep_wake(state: u8) -> SleepAml {
    let st = state as u64;
    run_group("wake", &[("\\_SI._SST", 2), ("\\_WAK", st), ("\\_SI._SST", 1)], true)
}

/// `_TTS(state)` alone (S0 at the very end of a resume).
pub fn tts(state: u8) { let _ = with_ns_locked(|ns, h| run_method(ns, h, "\\_TTS", &[V::Int(state as u64)])); }

/// Evaluate `\_S0_` / `\_S3_` / `\_S5_` -> (SLP_TYPa, SLP_TYPb).
pub fn sx_typ(name: &str) -> Option<(u8, u8)> {
    let path = format!("\\{}", name);
    with_ns_locked(|ns, h| {
        let n = ns.find(&path)?;
        let v = ns.eval(h, n, &[]).ok()?;
        let e = v.elements()?;
        let a = ns.int_of(h, e.get(0)?)? as u8;
        let b = ns.int_of(h, e.get(1)?)? as u8;
        Some((a, b))
    }).flatten()
}

/// Dry run: `_S3_`, then prepare + wake back-to-back. Never writes SLP_EN.
pub fn s3_dryrun() {
    log("[S3] dryrun: start (no sleep state will be entered)\n");
    for sx in ["_S0_", "_S3_", "_S5_"] {
        match sx_typ(sx) {
            Some((a, b)) => log(&format!("[S3] dryrun: \\{} SLP_TYP = {:#x}/{:#x}\n", sx, a, b)),
            None => log(&format!("[S3] dryrun: \\{} absent\n", sx)),
        }
    }
    pause_poll(true);
    let p = sleep_prepare(3);
    let w = sleep_wake(3);
    tts(0);
    pause_poll(false);
    log(&format!("[S3] dryrun: done prepare_ok={} wake_ok={} wak={:?} dropped={} gl_fails={}\n", p.ok, w.ok, w.wak, p.dropped + w.dropped, GL_FAILS.load(Ordering::Relaxed)));
}
