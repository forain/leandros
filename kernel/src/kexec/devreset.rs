//! kexec: reset every PCI function before the jump (Linux has no
//! equivalent; it trusts each driver's `shutdown` and clears bus mastering,
//! which leaves real NICs/GPUs/xHCIs running behind the next kernel's back).
//!
//! Two halves:
//! - [`plan`], task context, after the drivers' own shutdown hooks and the
//!   filesystem sync (power.rs `kernel_kexec`): pick a reset method per
//!   function and record it in a static table, so the second half needs no
//!   allocation and no lock.
//! - [`execute`], from the arch code once every other CPU is stopped and the
//!   interrupt controllers are masked (exec_x86_64 `final_bsp`, exec_aarch64
//!   `final_cpu0`): save the header, start every reset at once, one shared
//!   settle wait, then poll each function back and restore its header (BARs,
//!   ROM, cache line/INTx line, PCIe DevCtl/LnkCtl/DevCtl2), memory/I/O
//!   decode on, bus mastering off.
//!
//! Method, first that applies (Linux `pci_reset_function` order):
//!   FLR    PCIe DevCap.FLR: wait for DevSta.TransactionsPending (100 ms),
//!          DevCtl.BCR_FLR.
//!   AF-FLR Advanced Features capability with TP + FLR (integrated Intel
//!          functions without a PCIe capability: HDA, xHCI, IGD, I219).
//!   PM     D3hot -> D0 when PMCSR.No_Soft_Reset is clear (10 ms each way).
//!   SBR    secondary bus reset of the parent bridge, only when every function
//!          on that bus is a reset-class endpoint, none of them the boot
//!          framebuffer, and no bridge sits below it (`kexec.sbr=0` disables).
//! All of them share one 100 ms settle, then up to 1 s for config reads to
//! return (CRS / all-ones).
//!
//! Never reset: bridges (host, PCI-PCI, ISA/LPC: class 06 or header type 1),
//! functions outside the reset classes below (MEI, SMBus, thermal, ... stay as
//! firmware left them, bus mastering off), virtio functions (already reset
//! through their status register), and the function whose BAR holds the
//! framebuffer handed to the next kernel (MB2 tag 8, x86_64 only; aarch64
//! passes none), unless `kexec.fbreset=1`, which also stops the hand-over.
//! That function keeps bus mastering only when it is an Intel IGD (its
//! scanout of stolen memory; its GT is reset by the i915 hook), else it is
//! cleared too (NVIDIA: GSP queues live in system memory).
//!
//! IOMMU: this kernel never enables VT-d or an SMMU (no DMAR/IORT parsing),
//! so nothing is turned off here; translation is whatever the firmware left
//! (off on the machines in use, pre-boot DMA protection hands over disabled).

use core::fmt::Write as _;
use drivers::pci::{cap, PciFunction};

const MAX: usize = 96;

#[derive(Clone, Copy, PartialEq, Eq)]
enum How { Keep, Flr, AfFlr, Pm, Sbr }

#[derive(Clone, Copy)]
struct Ent {
    bus: u8, dev: u8, func: u8,
    vendor: u16, device: u16, class: u16,
    ecam: usize,
    pcie: u16, pm: u16, af: u16,
    /// Extended capabilities whose state a reset clears and the firmware
    /// programmed at cold boot (Linux pci_save_ltr_state /
    /// pci_save_aspm_l1ss_state / AER): LTR (0x18), L1 PM Substates (0x1E),
    /// AER (0x01). 0 = absent or no ECAM.
    ltr_cap: u16, l1ss_cap: u16, aer_cap: u16,
    how: How,
    why: &'static str,
    /// Reset-class function. These and virtio functions get INTx enabled
    /// again at the end (the next kernel's aarch64 drivers use INTx and never
    /// clear the disable bit); every other function keeps it disabled.
    resettable: bool,
    keep_bm: bool,
    /// SBR: the parent bridge (bus, dev, func) and its ECAM page.
    br: (u8, u8, u8), br_ecam: usize,
    hdr: [u32; 16],
    devctl: u16, lnkctl: u16, devctl2: u16,
    /// LTR max snoop/no-snoop latency (one dword), L1SS ctl1/ctl2,
    /// AER uncorrectable mask/severity, correctable mask, cap/control.
    ltr: u32, l1ss: [u32; 2], aer: [u32; 4],
    /// What the restore had to rewrite (bit 0 LTR, 1 L1SS, 2 AER).
    fixed: u8,
    ms: u32,
    back: bool,
}

const EMPTY: Ent = Ent {
    bus: 0, dev: 0, func: 0, vendor: 0, device: 0, class: 0, ecam: 0,
    pcie: 0, pm: 0, af: 0, ltr_cap: 0, l1ss_cap: 0, aer_cap: 0, how: How::Keep, why: "", resettable: false, keep_bm: false,
    br: (0, 0, 0), br_ecam: 0, hdr: [0; 16], devctl: 0, lnkctl: 0, devctl2: 0,
    ltr: 0, l1ss: [0; 2], aer: [0; 4], fixed: 0, ms: 0, back: false,
};

static mut PLAN: [Ent; MAX] = [EMPTY; MAX];
static mut COUNT: usize = 0;

/// Classes (base, sub; 0xFF = any sub) whose functions are reset.
const RESET_CLASSES: &[(u8, u8)] = &[
    (0x01, 0xFF), // storage: NVMe, AHCI
    (0x02, 0xFF), // network: e1000e, r8169, iwlwifi (0280)
    (0x03, 0xFF), // display: i915, NVIDIA
    (0x04, 0xFF), // multimedia: HDA (0403)
    (0x0C, 0x03), // USB: xHCI
    (0x08, 0x05), // SD host
    (0x0D, 0xFF), // wireless
];

/// Functions whose FLR is broken (Linux `quirk_no_flr`).
const NO_FLR: &[(u16, u16)] = &[
    (0x1022, 0x149c), (0x1022, 0x7901), (0x8086, 0x1502), (0x8086, 0x1503),
];

fn opt(key: &str) -> Option<&'static str> {
    crate::boot_cmdline()?.split_ascii_whitespace()
        .filter_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .last()
}
fn on(key: &str) -> bool { matches!(opt(key), Some("1") | Some("on") | Some("yes")) }
fn off(key: &str) -> bool { matches!(opt(key), Some("0") | Some("off") | Some("no")) }

/// `kexec.fbreset=1`: reset the framebuffer's device too and hand the next
/// kernel no framebuffer.
pub fn fb_reset() -> bool { on("kexec.fbreset") }

/// The display function whose memory BAR holds `fb` (x86_64 boot
/// framebuffer), found without sizing BARs (that would stop the live GPU's
/// decode): the memory BAR with the largest base <= `fb` across all
/// endpoints, provided `fb` is within that base's natural alignment (a BAR
/// of size S is S-aligned) and the function is class 03.
pub fn fb_owner(fb: u64) -> Option<PciFunction> {
    if fb == 0 { return None; }
    let mut best: Option<(u64, PciFunction, u8)> = None;
    for d in drivers::pci::scan().iter() {
        let f = d.function();
        if f.read8(0x0E) & 0x7F != 0 { continue; }
        for i in 0..6 {
            let Some(b) = f.bar(i) else { continue };
            if b.io || b.base == 0 || b.base > fb { continue; }
            if best.map_or(true, |(bb, _, _)| b.base > bb) { best = Some((b.base, f, d.class)); }
        }
    }
    let (base, f, class) = best?;
    let span = 1u64.checked_shl(base.trailing_zeros()).unwrap_or(u64::MAX);
    (class == 0x03 && fb - base < span).then_some(f)
}

/// Physical address of the framebuffer handed to the next kernel, if any:
/// x86_64 only, a PCI BAR (a RAM-backed GOP buffer gives a black console),
/// and not with `kexec.fbreset=1`. info.rs (MB2 tag 8) and the reset plan
/// both decide through this.
pub fn handover_fb(bi: &boot::BootInfo, ram_top: u64) -> Option<u64> {
    if cfg!(not(target_arch = "x86_64")) || fb_reset() { return None; }
    let fb = bi.framebuffer_base;
    if fb == 0 { return None; }
    (fb >= ram_top || fb_owner(fb).is_some()).then_some(fb)
}

fn reset_class(class: u8, sub: u8) -> bool {
    RESET_CLASSES.iter().any(|&(c, s)| c == class && (s == 0xFF || s == sub))
}

// ── raw config access: no lock, no allocation (the final context) ───────────

#[cfg(target_arch = "x86_64")]
fn legacy(b: (u8, u8, u8), off: u16, size: u8, w: Option<u32>) -> u32 {
    let addr = 0x8000_0000u32 | ((b.0 as u32) << 16) | ((b.1 as u32) << 11) | ((b.2 as u32) << 8) | (off as u32 & 0xFC);
    let port = 0xCFCu16 + (off & 3);
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") 0xCF8u16, in("eax") addr, options(nomem, nostack));
        match (size, w) {
            (1, None) => { let v: u8; core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack)); v as u32 }
            (2, None) => { let v: u16; core::arch::asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack)); v as u32 }
            (_, None) => { let v: u32; core::arch::asm!("in eax, dx", out("eax") v, in("dx") 0xCFCu16, options(nomem, nostack)); v }
            (1, Some(x)) => { core::arch::asm!("out dx, al", in("dx") port, in("al") x as u8, options(nomem, nostack)); 0 }
            (2, Some(x)) => { core::arch::asm!("out dx, ax", in("dx") port, in("ax") x as u16, options(nomem, nostack)); 0 }
            (_, Some(x)) => { core::arch::asm!("out dx, eax", in("dx") 0xCFCu16, in("eax") x, options(nomem, nostack)); 0 }
        }
    }
}

fn cfg(b: (u8, u8, u8), ecam: usize, off: u16, size: u8, w: Option<u32>) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if off < 0x100 || ecam == 0 {
        if off >= 0x100 { return u32::MAX; }
        return legacy(b, off, size, w);
    }
    let _ = b;
    if ecam == 0 { return u32::MAX; }
    let a = ecam + off as usize;
    unsafe {
        match (size, w) {
            (1, None) => leandros_lib::mmio::read(a as *const u8) as u32,
            (2, None) => leandros_lib::mmio::read(a as *const u16) as u32,
            (_, None) => leandros_lib::mmio::read(a as *const u32),
            (1, Some(x)) => { leandros_lib::mmio::write(a as *mut u8, x as u8); 0 }
            (2, Some(x)) => { leandros_lib::mmio::write(a as *mut u16, x as u16); 0 }
            (_, Some(x)) => { leandros_lib::mmio::write(a as *mut u32, x); 0 }
        }
    }
}

impl Ent {
    fn b(&self) -> (u8, u8, u8) { (self.bus, self.dev, self.func) }
    fn r8(&self, o: u16) -> u8 { cfg(self.b(), self.ecam, o, 1, None) as u8 }
    fn r16(&self, o: u16) -> u16 { cfg(self.b(), self.ecam, o, 2, None) as u16 }
    fn r32(&self, o: u16) -> u32 { cfg(self.b(), self.ecam, o, 4, None) }
    fn w8(&self, o: u16, v: u8) { cfg(self.b(), self.ecam, o, 1, Some(v as u32)); }
    fn w16(&self, o: u16, v: u16) { cfg(self.b(), self.ecam, o, 2, Some(v as u32)); }
    fn w32(&self, o: u16, v: u32) { cfg(self.b(), self.ecam, o, 4, Some(v)); }
    fn br16(&self, o: u16) -> u16 { cfg(self.br, self.br_ecam, o, 2, None) as u16 }
    fn bw16(&self, o: u16, v: u16) { cfg(self.br, self.br_ecam, o, 2, Some(v as u32)); }
}

// PCI register bits.
const CMD_IO_MEM: u16 = 0x3;
const CMD_PERR_SERR: u16 = (1 << 6) | (1 << 8);
const CMD_BM: u16 = 1 << 2;
const CMD_INTX_OFF: u16 = 1 << 10;
const EXP_DEVCAP_FLR: u32 = 1 << 28;
const EXP_DEVCTL_BCR_FLR: u16 = 1 << 15;
const EXP_DEVSTA_TRPND: u16 = 1 << 5;
const AF_CAP_ID: u8 = 0x13;
const PM_NO_SOFT_RESET: u16 = 1 << 3;
const BRIDGE_CTL: u16 = 0x3E;
const BRIDGE_CTL_SBR: u16 = 1 << 6;

/// Does the plan keep bus mastering on this function (the Intel IGD
/// scanning out the handed-over framebuffer)? power.rs `quiesce_all_pci`
/// asks before clearing it.
pub fn keeps_bm(bus: u8, dev: u8, func: u8) -> bool {
    let n = unsafe { COUNT };
    let plan = unsafe { &(&*core::ptr::addr_of!(PLAN))[..n] };
    plan.iter().any(|e| e.keep_bm && e.bus == bus && e.dev == dev && e.func == func)
}

/// Task context, interrupts may be off: build the reset table. Runs after
/// the drivers' shutdown hooks and before power.rs `quiesce_all_pci`
/// (which clears bus mastering and MSI/MSI-X everywhere but `keeps_bm`).
pub fn plan(fb: Option<u64>) {
    let enabled = !off("kexec.devreset");
    let sbr_ok = !off("kexec.sbr");
    let owner = fb.and_then(fb_owner);
    let devs = drivers::pci::scan();
    let mut n = 0usize;
    let plan = unsafe { &mut *core::ptr::addr_of_mut!(PLAN) };
    for d in devs.iter() {
        let f = d.function();
        if d.class == 0x06 || f.read8(0x0E) & 0x7F != 0 { continue; }
        if n == MAX { break; }
        let mut e = EMPTY;
        e.bus = d.bus; e.dev = d.dev; e.func = d.func;
        e.vendor = d.vendor_id; e.device = d.device_id;
        e.class = ((d.class as u16) << 8) | d.subclass as u16;
        e.ecam = f.ecam_page();
        e.pcie = f.find_capability(cap::PCIE).unwrap_or(0);
        if e.pcie != 0 && e.ecam != 0 {
            e.ltr_cap = f.find_ext_capability(0x18).unwrap_or(0);
            e.l1ss_cap = f.find_ext_capability(0x1E).unwrap_or(0);
            e.aer_cap = f.find_ext_capability(0x01).unwrap_or(0);
        }
        e.pm = f.find_capability(cap::PM).unwrap_or(0);
        e.af = f.find_capability(AF_CAP_ID).unwrap_or(0);
        e.resettable = reset_class(d.class, d.subclass);
        let is_owner = owner.map_or(false, |o| o.bus == f.bus && o.dev == f.dev && o.func == f.func);
        e.keep_bm = is_owner && d.vendor_id == 0x8086 && !off("kexec.fbbm")
            && f.command() & drivers::pci::CMD_BUS_MASTER != 0;
        let no_flr = NO_FLR.iter().any(|&(v, i)| v == d.vendor_id && i == d.device_id);
        (e.how, e.why) = if !enabled { (How::Keep, "kexec.devreset=0") }
            else if !e.resettable { (How::Keep, "class not reset") }
            else if is_owner { (How::Keep, "boot framebuffer for the next kernel") }
            else if d.vendor_id == 0x1AF4 { (How::Keep, "virtio: status reset") }
            else if !no_flr && e.pcie != 0 && f.read32(e.pcie + 4) & EXP_DEVCAP_FLR != 0 { (How::Flr, "FLR") }
            else if !no_flr && e.af != 0 && f.read8(e.af + 3) & 3 == 3 { (How::AfFlr, "AF FLR") }
            else if e.pm != 0 && f.read16(e.pm + 4) & PM_NO_SOFT_RESET == 0 && f.read16(e.pm + 4) & 3 == 0 { (How::Pm, "D3hot->D0") }
            else { (How::Sbr, "") };
        plan[n] = e;
        n += 1;
    }
    // Secondary bus reset: only for a bus behind a bridge whose every
    // function is a planned endpoint that is itself being reset (so each one
    // gets its header restored afterwards): no bridge below, no framebuffer,
    // no virtio or firmware-owned function on it.
    for i in 0..n {
        if plan[i].how != How::Sbr { continue; }
        let bus = plan[i].bus;
        let parent = devs.iter().find(|d| {
            let f = d.function();
            f.read8(0x0E) & 0x7F == 1 && f.read8(0x19) == bus && bus != 0
        });
        let all_ok = devs.iter().filter(|d| d.bus == bus).all(|d| {
            plan[..n].iter().any(|p| p.bus == d.bus && p.dev == d.dev && p.func == d.func && p.how != How::Keep)
        });
        match parent {
            Some(p) if sbr_ok && all_ok => {
                let pf = p.function();
                plan[i].br = (pf.bus, pf.dev, pf.func);
                plan[i].br_ecam = pf.ecam_page();
                plan[i].why = "secondary bus reset";
            }
            _ => {
                plan[i].how = How::Keep;
                plan[i].why = if parent.is_none() { "no FLR/PM reset, no parent bridge (driver hook only)" }
                    else if !sbr_ok { "kexec.sbr=0" } else { "no FLR/PM reset, bus shared (driver hook only)" };
            }
        }
    }
    unsafe { COUNT = n; }
}

fn now_ns() -> u64 {
    #[cfg(target_arch = "x86_64")]
    { arch_x86_64::timer::monotonic_ns() }
    #[cfg(target_arch = "aarch64")]
    { arch_aarch64::timer::monotonic_ns() }
}

fn spin_ms(ms: u64) {
    let t = now_ns();
    while now_ns().wrapping_sub(t) < ms * 1_000_000 { core::hint::spin_loop(); }
}

struct Line { b: [u8; 200], n: usize }
impl core::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

/// Final context: every other CPU stopped, interrupts masked. Runs the plan
/// and prints one line per function through `log`. No lock, no allocation.
/// Put back the firmware's PCIe power-management and error-reporting state
/// that the reset cleared, before DevCtl/LnkCtl/DevCtl2 are restored (so LTR
/// values are in place before LTR is enabled, and L1SS is reprogrammed with
/// ASPM L1 off). Mirrors Linux pci_restore_ltr_state /
/// pci_restore_aspm_l1ss_state / AER. The bridges above are never reset, so
/// their side of L1SS is still what the firmware set.
fn restore_pm(e: &mut Ent) {
    if e.ltr_cap != 0 && e.r32(e.ltr_cap + 4) != e.ltr {
        e.w32(e.ltr_cap + 4, e.ltr);
        e.fixed |= 1;
    }
    if e.l1ss_cap != 0 && (e.r32(e.l1ss_cap + 8) != e.l1ss[0] || e.r32(e.l1ss_cap + 0xC) != e.l1ss[1]) {
        let lc = e.r16(e.pcie + 0x10);
        e.w16(e.pcie + 0x10, lc & !3);
        e.w32(e.l1ss_cap + 0xC, e.l1ss[1]);
        e.w32(e.l1ss_cap + 8, e.l1ss[0] & !0xF);
        e.w32(e.l1ss_cap + 8, e.l1ss[0]);
        e.fixed |= 2;
    }
    if e.aer_cap != 0 {
        let offs = [8u16, 0xC, 0x14, 0x18];
        for i in 0..4 {
            if e.r32(e.aer_cap + offs[i]) != e.aer[i] { e.w32(e.aer_cap + offs[i], e.aer[i]); e.fixed |= 4; }
        }
        // Errors latched while the function was in reset (RW1CS).
        let (u, c) = (e.r32(e.aer_cap + 4), e.r32(e.aer_cap + 0x10));
        if u != 0 { e.w32(e.aer_cap + 4, u); }
        if c != 0 { e.w32(e.aer_cap + 0x10, c); }
    }
}

pub fn execute(log: fn(&str)) {
    let n = unsafe { COUNT };
    let plan = unsafe { &mut (&mut *core::ptr::addr_of_mut!(PLAN))[..n] };
    let t0 = now_ns();
    // A compositor on another CPU may have flipped after power.rs put the
    // console surface back; the KMS hook is lock-free (panic-path safe).
    drivers::gpu_kms::console_restore();
    // 1. Save, quiesce once more, wait for outstanding transactions.
    for e in plan.iter_mut() {
        for i in 0..16 { e.hdr[i] = e.r32((i * 4) as u16); }
        if e.pcie != 0 {
            e.devctl = e.r16(e.pcie + 8);
            e.lnkctl = e.r16(e.pcie + 0x10);
            e.devctl2 = e.r16(e.pcie + 0x28);
        }
        if e.ltr_cap != 0 { e.ltr = e.r32(e.ltr_cap + 4); }
        if e.l1ss_cap != 0 { e.l1ss = [e.r32(e.l1ss_cap + 8), e.r32(e.l1ss_cap + 0xC)]; }
        if e.aer_cap != 0 {
            e.aer = [e.r32(e.aer_cap + 8), e.r32(e.aer_cap + 0xC), e.r32(e.aer_cap + 0x14), e.r32(e.aer_cap + 0x18)];
        }
        let cmd = e.hdr[1] as u16;
        let bm = if e.keep_bm { cmd & CMD_BM } else { 0 };
        e.w16(0x04, (cmd & !CMD_BM) | bm | CMD_INTX_OFF);
    }
    let mut any = false;
    for e in plan.iter() {
        match e.how {
            How::Flr => {
                let t = now_ns();
                while e.r16(e.pcie + 0xA) & EXP_DEVSTA_TRPND != 0 && now_ns().wrapping_sub(t) < 100_000_000 { core::hint::spin_loop(); }
                e.w16(e.pcie + 8, e.devctl | EXP_DEVCTL_BCR_FLR);
                any = true;
            }
            How::AfFlr => {
                let t = now_ns();
                while e.r8(e.af + 5) & 1 != 0 && now_ns().wrapping_sub(t) < 100_000_000 { core::hint::spin_loop(); }
                e.w8(e.af + 4, 1);
                any = true;
            }
            How::Pm => {
                let pmcsr = e.r16(e.pm + 4) & !(1 << 15) & !3;
                e.w16(e.pm + 4, pmcsr | 3);
                any = true;
            }
            How::Sbr => {
                let c = e.br16(BRIDGE_CTL);
                if c & BRIDGE_CTL_SBR == 0 { e.bw16(BRIDGE_CTL, c | BRIDGE_CTL_SBR); }
                any = true;
            }
            How::Keep => {}
        }
    }
    if any {
        // 2. D3hot dwell / SBR assertion (Trst >= 1 ms), then back.
        spin_ms(10);
        for e in plan.iter() {
            match e.how {
                How::Pm => { let p = e.r16(e.pm + 4) & !(1 << 15) & !3; e.w16(e.pm + 4, p); }
                How::Sbr => { let c = e.br16(BRIDGE_CTL); if c & BRIDGE_CTL_SBR != 0 { e.bw16(BRIDGE_CTL, c & !BRIDGE_CTL_SBR); } }
                _ => {}
            }
        }
        // 3. One shared settle: FLR 100 ms, D3hot->D0 10 ms, SBR 100 ms.
        spin_ms(100);
    }
    // 4. Back on the bus, header restored, decode on, mastering off.
    for e in plan.iter_mut() {
        if e.how != How::Keep {
            let t = now_ns();
            loop {
                let id = e.r32(0);
                if id != 0xFFFF_FFFF && id & 0xFFFF != 0x0001 { e.back = true; break; }
                if now_ns().wrapping_sub(t) > 1_000_000_000 { break; }
                core::hint::spin_loop();
            }
            e.ms = (now_ns().wrapping_sub(t0) / 1_000_000) as u32;
            if !e.back { continue; }
            e.w32(0x0C, e.hdr[3] & 0xFFFF);
            for i in 4..10 { e.w32((i * 4) as u16, e.hdr[i]); }
            e.w32(0x30, e.hdr[12]);
            e.w8(0x3C, e.hdr[15] as u8);
            if e.pcie != 0 {
                restore_pm(e);
                e.w16(e.pcie + 8, e.devctl & !EXP_DEVCTL_BCR_FLR);
                e.w16(e.pcie + 0x10, e.lnkctl);
                e.w16(e.pcie + 0x28, e.devctl2);
            }
            e.w16(0x04, (e.hdr[1] as u16) & (CMD_IO_MEM | CMD_PERR_SERR));
        } else if e.resettable || e.vendor == 0x1AF4 {
            let c = e.r16(0x04);
            e.w16(0x04, c & !CMD_INTX_OFF);
        }
    }
    // 5. Report.
    let (mut reset, mut failed) = (0, 0);
    for e in plan.iter() {
        let mut l = Line { b: [0; 200], n: 0 };
        let _ = write!(l, "[KEXEC] {:02x}:{:02x}.{} {:04x}:{:04x} class {:04x}: ", e.bus, e.dev, e.func, e.vendor, e.device, e.class);
        if e.how == How::Keep {
            let _ = write!(l, "not reset ({}){}", e.why, if e.keep_bm { ", bus master kept" } else { "" });
        } else if e.back {
            reset += 1;
            let _ = write!(l, "{} done, back after {} ms, header restored", e.why, e.ms);
            if e.fixed != 0 {
                let _ = write!(l, ", restored{}{}{}", if e.fixed & 1 != 0 { " LTR" } else { "" },
                    if e.fixed & 2 != 0 { " L1SS" } else { "" }, if e.fixed & 4 != 0 { " AER" } else { "" });
            }
            if e.how == How::Sbr { let _ = write!(l, " (bridge {:02x}:{:02x}.{})", e.br.0, e.br.1, e.br.2); }
        } else {
            failed += 1;
            let _ = write!(l, "{} done, NOT back after 1 s (reads all-ones)", e.why);
        }
        let _ = l.write_str("\n");
        if let Ok(s) = core::str::from_utf8(&l.b[..l.n]) { log(s); }
    }
    let mut l = Line { b: [0; 200], n: 0 };
    let _ = write!(l, "[KEXEC] device reset: {} function(s) reset, {} not back, {} left as is, {} ms\n",
        reset, failed, n - reset - failed, now_ns().wrapping_sub(t0) / 1_000_000);
    if let Ok(s) = core::str::from_utf8(&l.b[..l.n]) { log(s); }
}
