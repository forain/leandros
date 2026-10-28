//! BCM2712 PCIe root complex "pcie2" (x4), the one that hosts RP1 on the
//! Raspberry Pi 5. Brings the controller up from reset the way Linux
//! `pcie-brcmstb.c` does for BCM2712, then plugs a config-space backend into
//! `pci.rs` so RP1 (1de4:0001) enumerates as an ordinary PCI function.
//!
//! Opt-in: `pcie.rpi5=1` on the kernel command line (default off until it is
//! hardware-verified). Every step is bounded; any failure prints one
//! `[pcie] ...` line, leaves the bus unregistered and boot continues.
//!
//! Sequence (Linux `brcm_pcie_probe` / `brcm_pcie_setup` / `brcm_pcie_start_link`):
//!  1. rescal reset pulse; bridge reset pulse through the brcmstb-reset
//!     controller (bit 44) with no RC access while it is asserted;
//!  2. bridge release confirmed by SW_INIT status + an RC REVISION read,
//!     then PERST# asserted and the SerDes out of IDDQ;
//!  3. 54 MHz PLL setup over MDIO, L1SS PM clock period;
//!  4. MISC_CTRL (SCB access, UR-on-read, 512 B burst), UBUS error suppression
//!     (reads that fail return all-ones instead of an SError) and timeouts;
//!  5. inbound RC_BAR1 (RAM) + RC_BAR2 (MIP doorbell), outbound windows;
//!  6. root-port class/endian/gen2/x4/ASPM-L1 setup, PERST# released;
//!  7. link-up poll (<= 300 ms), CRS visibility, bus numbers and window.

use crate::brcmstb_hw as hw;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

/// Virtual address of the register block (0 until `init` mapped it).
static REGS: AtomicUsize = AtomicUsize::new(0);
/// Serialises EXT_CFG_INDEX + data access.
static CFG_LOCK: Mutex<()> = Mutex::new(());
/// Virtual address of the brcmstb-reset SW_INIT bank base (for the kexec hook).
static SWINIT: AtomicUsize = AtomicUsize::new(0);
/// 0 = not run, 1 = link up and registered, 2.. = failed at step (see `state`).
static STATE: AtomicU32 = AtomicU32::new(0);

fn log(s: &str) { crate::pci::serial_debug(s) }

#[inline] fn rd(off: usize) -> u32 { unsafe { leandros_lib::mmio::read((REGS.load(Ordering::Relaxed) + off) as *const u32) } }
#[inline] fn wr(off: usize, v: u32) { unsafe { leandros_lib::mmio::write((REGS.load(Ordering::Relaxed) + off) as *mut u32, v) } }
#[inline] fn rmw(off: usize, clear: u32, set: u32) { wr(off, (rd(off) & !clear) | set) }

/// Human-readable state for logs/diagnostics.
pub fn state() -> &'static str {
    match STATE.load(Ordering::Relaxed) {
        0 => "off",
        1 => "up",
        2 => "map-failed",
        3 => "rescal-timeout",
        4 => "mdio-timeout",
        5 => "link-down",
        7 => "stopped",
        8 => "bridge-stuck",
        9 => "rc-dead",
        _ => "no-rp1",
    }
}

pub fn is_up() -> bool { STATE.load(Ordering::Relaxed) == 1 }

fn fail(code: u32, msg: &str) {
    STATE.store(code, Ordering::Relaxed);
    log("[pcie] FAIL ");
    log(msg);
    log("\n");
}

fn mdio_write(regad: u32, data: u32) -> bool {
    wr(hw::RC_DL_MDIO_ADDR, hw::mdio_pkt(hw::MDIO_PORT0, regad, hw::MDIO_CMD_WRITE));
    let _ = rd(hw::RC_DL_MDIO_ADDR);
    wr(hw::RC_DL_MDIO_WR_DATA, hw::MDIO_DATA_DONE | (data & 0xffff));
    for _ in 0..100 {
        if rd(hw::RC_DL_MDIO_WR_DATA) & hw::MDIO_DATA_DONE == 0 { return true; }
        sched::kthread::udelay(10);
    }
    false
}

/// rescal: start, wait for status (<= 1 ms), stop. Returns false on timeout.
fn rescal_reset(base: usize) -> bool {
    unsafe {
        let s = (base + hw::RESCAL_START) as *mut u32;
        let st = (base + hw::RESCAL_STATUS) as *const u32;
        let v = leandros_lib::mmio::read(s);
        leandros_lib::mmio::write(s, v | 1);
        let mut ok = false;
        for _ in 0..100 {
            if leandros_lib::mmio::read(st) & 1 != 0 { ok = true; break; }
            sched::kthread::udelay(10);
        }
        let v = leandros_lib::mmio::read(s);
        leandros_lib::mmio::write(s, v & !1);
        ok
    }
}

/// Real MMIO behind the host-tested [`hw::PcieBus`] sequencing.
struct MmioBus { rc: usize, swinit: usize }

impl hw::PcieBus for MmioBus {
    fn rd(&mut self, blk: hw::Blk, off: usize) -> u32 {
        let base = match blk { hw::Blk::Rc => self.rc, hw::Blk::SwInit => self.swinit };
        unsafe { leandros_lib::mmio::read((base + off) as *const u32) }
    }
    fn wr(&mut self, blk: hw::Blk, off: usize, v: u32) {
        let base = match blk { hw::Blk::Rc => self.rc, hw::Blk::SwInit => self.swinit };
        unsafe { leandros_lib::mmio::write((base + off) as *mut u32, v) }
    }
    fn udelay(&mut self, us: u32) { sched::kthread::udelay(us as u64) }
}

fn perst(assert: bool) {
    rmw(hw::MISC_PCIE_CTRL, hw::PCIE_CTRL_PERSTB, if assert { 0 } else { hw::PCIE_CTRL_PERSTB });
}

fn map(pa: u64, len: usize) -> usize {
    mm::kva::ioremap(pa, len, mm::kva::CacheMode::Uncached).unwrap_or(0)
}

/// Config-space backend handed to `pci.rs`.
fn cfg_access(bus: u8, dev: u8, func: u8, off: u16, size: u8, write: Option<u32>) -> u32 {
    let ones = match size { 1 => 0xff, 2 => 0xffff, _ => 0xffff_ffff };
    let base = REGS.load(Ordering::Acquire);
    if base == 0 || bus > 1 { return ones; }
    let Some(o) = hw::cfg_offset(bus, dev, func, off) else { return ones };
    let _g = CFG_LOCK.lock();
    if bus != 0 {
        // Never send a config TLP over a dead link.
        if !hw::link_up(rd(hw::MISC_PCIE_STATUS)) { return ones; }
        wr(hw::EXT_CFG_INDEX, hw::cfg_index(bus, dev, func));
    }
    let a = base + o;
    // The controller drops sub-dword config writes (lane pirp1-bar: the
    // root port's 16-bit command write never landed; RP1's bus-master bit
    // stayed clear, so no RP1 DMA reached DRAM). Read-modify-write the
    // aligned dword, as Linux's pci_generic_config_write32 does, with the
    // RW1C Status half written as zero so no error bit is cleared by accident.
    if let (1 | 2, Some(w)) = (size, write) {
        let ad = (a & !3) as *mut u32;
        unsafe {
            let cur = leandros_lib::mmio::read(ad as *const u32);
            leandros_lib::mmio::write(ad, hw::sub_dword_write(cur, off, size, w));
        }
        return 0;
    }
    unsafe {
        match (size, write) {
            (1, None) => leandros_lib::mmio::read(a as *const u8) as u32,
            (2, None) => leandros_lib::mmio::read(a as *const u16) as u32,
            (_, None) => leandros_lib::mmio::read(a as *const u32),
            (1, Some(w)) => { leandros_lib::mmio::write(a as *mut u8, w as u8); 0 }
            (2, Some(w)) => { leandros_lib::mmio::write(a as *mut u16, w as u16); 0 }
            (_, Some(w)) => { leandros_lib::mmio::write(a as *mut u32, w); 0 }
        }
    }
}

fn cfg32(bus: u8, off: u16) -> u32 { cfg_access(bus, 0, 0, off, 4, None) }
fn cfgw32(bus: u8, off: u16, v: u32) { cfg_access(bus, 0, 0, off, 4, Some(v)); }

fn print_hex(v: u64) {
    let mut b = [0u8; 18];
    b[0] = b'0'; b[1] = b'x';
    let digits = if v == 0 { 1 } else { (64 - v.leading_zeros() as usize + 3) / 4 };
    for i in 0..digits {
        let n = ((v >> (4 * (digits - 1 - i))) & 0xf) as u8;
        b[2 + i] = if n < 10 { b'0' + n } else { b'a' + n - 10 };
    }
    log(core::str::from_utf8(&b[..2 + digits]).unwrap_or("?"));
}

/// `pcie.stop=N` (stages 1..=N run). Set by [`configure`] before [`init`].
static STOP: AtomicU32 = AtomicU32::new(hw::LAST_STAGE);
/// `pcie.reuse=1`: keep a link the firmware already trained.
static REUSE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Reads the bring-up knobs from the kernel command line.
pub fn configure(cmdline: &str) {
    STOP.store(hw::parse_stop(cmdline), Ordering::Relaxed);
    REUSE.store(cmdline.split_ascii_whitespace().any(|t| t == "pcie.reuse=1"), Ordering::Relaxed);
    sched::wedge::set_namer(sched::wedge::SLOT_PCIE, hw::stage_name);
}

/// Unbuffered console line straight to the UART FIFO (no per-CPU stage, no
/// lock): once queued, the bytes go out even if the next load stalls the CPU.
fn raw(s: &str) { sched::wedge::raw_str(s) }

/// Enters stage `n`: honours `pcie.stop`, records the stage in the wedge
/// `pcie` slot and prints `[pcie] stage N name` before the stage's first
/// access. Returns false (and leaves everything as is) when stopped.
pub fn stage(n: u32) -> bool {
    let name = hw::stage_name(n);
    if n > STOP.load(Ordering::Relaxed) {
        raw("[pcie] stop before stage ");
        sched::wedge::raw_dec(n as u64);
        raw(" "); raw(name); raw(" (pcie.stop)\n");
        sched::wedge::mark(sched::wedge::SLOT_PCIE, 0, n as u64);
        // Stopping before the RP1 probe keeps a finished bus "up".
        if STATE.load(Ordering::Relaxed) != 1 { STATE.store(7, Ordering::Relaxed); }
        return false;
    }
    sched::wedge::mark(sched::wedge::SLOT_PCIE, n, 0);
    raw("[pcie] stage "); sched::wedge::raw_dec(n as u64); raw(" "); raw(name); raw("\n");
    true
}

/// Bring-up finished or abandoned: the wedge slot goes idle.
pub fn stage_done() { sched::wedge::mark(sched::wedge::SLOT_PCIE, 0, 0); }

fn raw_hex(label: &str, v: u64) { raw(label); sched::wedge::raw_hex(v); }

/// Brings pcie2 up and registers it with `pci.rs`. Task context, once.
/// Never hangs on a poll: every poll is bounded; failures leave the bus
/// unregistered. A stalled AXI access cannot be bounded in software, so every
/// risky step is announced first (see [`stage`]).
pub fn init() {
    if STATE.load(Ordering::Relaxed) != 0 { return; }
    init_inner();
    if !is_up() { stage_done(); }
}

fn init_inner() {
    if !stage(1) { return; }
    let regs = map(hw::PCIE2_REGS, hw::REG_SIZE);
    let rescal = map(hw::RESCAL_REGS & !0xfff, 0x1000);
    let swinit = map(hw::SW_INIT_REGS & !0xfff, 0x1000);
    if regs == 0 || rescal == 0 || swinit == 0 { return fail(2, "ioremap"); }
    let rescal = rescal + (hw::RESCAL_REGS & 0xfff) as usize;
    let swinit = swinit + (hw::SW_INIT_REGS & 0xfff) as usize;
    SWINIT.store(swinit, Ordering::Release);

    // Reset-controller status: is the bridge held in reset at hand-over?
    if !stage(2) { return; }
    let (bank, bit) = hw::sw_init_set(hw::PCIE2_BRIDGE_RESET_ID);
    let st = unsafe { leandros_lib::mmio::read((swinit + bank + hw::SW_INIT_STATUS) as *const u32) };
    raw_hex("[pcie] swinit status=", st as u64);
    raw(if st & bit != 0 { " bridge=in-reset\n" } else { " bridge=released\n" });

    let mut reuse = false;
    if !stage(3) { return; }
    if !rescal_reset(rescal) { return fail(3, "rescal timeout"); }
    REGS.store(regs, Ordering::Release);

    // First RC reads, before touching anything: tells whether the RC
    // register block is clocked at all and whether the firmware left a link.
    if !stage(4) { REGS.store(0, Ordering::Release); return; }
    if st & bit != 0 {
        // Held in reset (early_reset, a previous kernel's kexec hook, or the
        // firmware): the RC block does not answer, so no read before the
        // bridge pulse below; there is no link to reuse either.
        raw("[pcie] rc not read: bridge held in reset at hand-over\n");
    } else {
    let rev = rd(hw::MISC_REVISION);
    let status = rd(hw::MISC_PCIE_STATUS);
    let ctrl = rd(hw::MISC_PCIE_CTRL);
    raw_hex("[pcie] rc rev=", rev as u64);
    raw_hex(" status=", status as u64);
    raw_hex(" ctrl=", ctrl as u64);
    raw(if hw::link_up(status) { " link=up\n" } else { " link=down\n" });
    if REUSE.load(Ordering::Relaxed) && hw::link_up(status) {
        raw("[pcie] reusing the firmware link (pcie.reuse=1)\n");
        reuse = true;
    }
    }

    if !reuse {
        // 1. Resets (Linux brcm_pcie_setup order for BCM2712): bridge reset
        //    pulse with NO RC access while it is asserted (the RC block
        //    does not answer then; a read stalls the CPU forever).
        let mut bus = MmioBus { rc: regs, swinit };
        if !stage(5) { REGS.store(0, Ordering::Release); return; }
        let st = hw::bridge_assert(&mut bus);
        raw_hex("[pcie] bridge asserted swinit=", st as u64);
        raw("\n");
        if !stage(6) { REGS.store(0, Ordering::Release); return; }
        match hw::bridge_release(&mut bus) {
            Ok(rev) => { raw_hex("[pcie] bridge released rc rev=", rev as u64); raw("\n"); }
            Err(hw::BridgeErr::StuckInReset) => {
                REGS.store(0, Ordering::Release);
                return fail(8, "bridge stuck in reset");
            }
            Err(hw::BridgeErr::RcDead(v)) => {
                REGS.store(0, Ordering::Release);
                raw_hex("[pcie] rc dead after release rev=", v as u64);
                raw("\n");
                return fail(9, "rc not answering");
            }
        }
        // 2. First RC writes: PERST# asserted, SerDes power-up.
        if !stage(7) { REGS.store(0, Ordering::Release); return; }
        hw::perst_serdes(&mut bus);
        raw_hex("[pcie] perst+serdes ctrl=", rd(hw::MISC_PCIE_CTRL) as u64);
        raw_hex(" hard_debug=", rd(hw::HARD_DEBUG) as u64);
        raw("\n");

        // 3. 54 MHz reference-clock PLL, L1SS PM clock period (18.52 ns).
        if !stage(8) { REGS.store(0, Ordering::Release); return; }
        let mut ok = mdio_write(hw::MDIO_SET_ADDR_OFFSET, hw::PLL_54MHZ_BLOCK);
        for &(r, d) in hw::PLL_54MHZ_REGS.iter() { ok &= mdio_write(r, d); }
        if !ok { REGS.store(0, Ordering::Release); return fail(4, "mdio timeout"); }
        sched::kthread::udelay(200);
        rmw(hw::RC_PL_PHY_CTL_15, hw::PHY_CTL_15_PM_CLK_PERIOD_MASK, 0x12);
    }

    // 4. Bus behaviour and error suppression.
    if !stage(9) { REGS.store(0, Ordering::Release); return; }
    rmw(hw::MISC_CTRL, hw::MISC_CTRL_MAX_BURST_MASK,
        hw::MISC_CTRL_SCB_ACCESS_EN | hw::MISC_CTRL_CFG_READ_UR_MODE
        | hw::MISC_CTRL_RCB_MPS_MODE | hw::MISC_CTRL_RCB_64B_MODE
        | (2 << hw::MISC_CTRL_MAX_BURST_SHIFT));
    rmw(hw::MISC_UBUS_CTRL, 0, hw::UBUS_CTRL_REPLY_ERR_DIS | hw::UBUS_CTRL_REPLY_DECERR_DIS);
    wr(hw::MISC_AXI_READ_ERROR_DATA, 0xffff_ffff);
    wr(hw::MISC_UBUS_TIMEOUT, 0x0b2d_0000);
    wr(hw::MISC_RC_CONFIG_RETRY_TIMEOUT, 0x0aba_0000);

    // 5. Inbound: BAR1 = RAM, BAR2 = MIP doorbell, BAR3 off. Outbound windows.
    if !stage(10) { REGS.store(0, Ordering::Release); return; }
    for (o, v) in hw::inbound_win_regs(1, hw::IB_RAM_PCI, hw::IB_RAM_CPU, hw::IB_RAM_SIZE) { wr(o, v); }
    for (o, v) in hw::inbound_win_regs(2, hw::MSI_PCI_ADDR, hw::MIP0_REGS, 0x1000) { wr(o, v); }
    for (o, v) in hw::inbound_win_regs(3, 0, 0, 0) { wr(o, v); }
    let bl0 = rd(hw::MISC_CPU2PCIE_WIN0_BASE_LIMIT);
    for (o, v) in hw::outbound_win_regs(0, hw::OB_MEM_CPU, hw::OB_MEM_PCI, hw::OB_MEM_SIZE, bl0) { wr(o, v); }
    let bl1 = rd(hw::MISC_CPU2PCIE_WIN0_BASE_LIMIT + 4);
    for (o, v) in hw::outbound_win_regs(1, hw::OB_PREF_CPU, hw::OB_PREF_PCI, hw::OB_PREF_SIZE, bl1) { wr(o, v); }

    if !reuse {
        // 6. Root port identity and link parameters, then release PERST#.
        if !stage(11) { REGS.store(0, Ordering::Release); return; }
        rmw(hw::RC_PRIV1_ID_VAL3, hw::PRIV1_ID_VAL3_CLASS_MASK, 0x06_0400);
        rmw(hw::RC_VENDOR_SPECIFIC_REG1, hw::VENDOR_REG1_ENDIAN_MODE_BAR2_MASK, 0);
        // x4, ASPM L1 only (`aspm-no-l0s`).
        rmw(hw::RC_PRIV1_LINK_CAPABILITY, hw::PRIV1_LINKCAP_WIDTH_MASK | hw::PRIV1_LINKCAP_ASPM_MASK, (4 << 4) | (2 << 10));
        // max-link-speed = 2.
        rmw(hw::RC_LNKCAP, 0xf, 2);
        let l2 = rd(hw::RC_LNKCTL2 & !3);
        wr(hw::RC_LNKCTL2 & !3, (l2 & !0xf) | 2);
        rmw(hw::HARD_DEBUG, hw::HARD_DEBUG_L1SS_ENABLE, hw::HARD_DEBUG_CLKREQ_DEBUG_ENABLE);
        sched::kthread::udelay(200);
        perst(false);
    }

    // 7. Link-up: 100 ms PERST settle (PCIe T_PVPERL), then poll up to 200 ms.
    if !stage(12) { REGS.store(0, Ordering::Release); return; }
    if !reuse { sched::kthread::msleep(100); }
    let mut up = false;
    for _ in 0..40 {
        if hw::link_up(rd(hw::MISC_PCIE_STATUS)) { up = true; break; }
        sched::kthread::msleep(5);
    }
    if !up {
        log("[pcie] FAIL link down status=");
        print_hex(rd(hw::MISC_PCIE_STATUS) as u64);
        log("\n");
        perst(true);
        REGS.store(0, Ordering::Release);
        STATE.store(5, Ordering::Relaxed);
        return;
    }
    let lnksta = (rd(hw::RC_LNKSTA & !3) >> 16) as u16;
    let (gen, width) = hw::lnksta_decode(lnksta);
    log("[pcie] rc link up gen");
    crate::pci::serial_debug_dec(gen as u64);
    log(" x");
    crate::pci::serial_debug_dec(width as u64);
    log("\n");
    rmw(hw::RC_RTCTL, 0, hw::RTCTL_CRSSVE);

    // Root port: bus 0 → 1..1, memory decode + bus master.
    if !stage(13) { REGS.store(0, Ordering::Release); return; }
    cfgw32(0, 0x18, (cfg32(0, 0x18) & 0xff00_0000) | (1 << 16) | (1 << 8));
    if !stage(14) { REGS.store(0, Ordering::Release); return; }
    let id = cfg32(1, 0);
    if id & 0xffff != hw::RP1_VENDOR as u32 || id >> 16 != hw::RP1_DEVICE as u32 {
        log("[pcie] FAIL no RP1 at 01:00.0 id=");
        print_hex(id as u64);
        log("\n");
        STATE.store(6, Ordering::Relaxed);
        return;
    }
    if !stage(15) { REGS.store(0, Ordering::Release); return; }
    let l = hw::setup_rp1_bars(&mut Cfg);
    if let Some((at, size)) = l.bar(1) {
        RP1_BAR1.store(hw::OB_MEM_CPU - hw::OB_MEM_PCI + at, Ordering::Relaxed);
        RP1_BAR1_SIZE.store(size, Ordering::Release);
    }

    crate::pci::set_cfg_backend(cfg_access, hw::OB_MEM_CPU - hw::OB_MEM_PCI, 1);
    STATE.store(1, Ordering::Relaxed);
    crate::dma_shutdown::register("pcie-perst", crate::dma_shutdown::Stage::Bus, kexec_reset);
    log("[pcie] 0000:01:00.0 1de4:0001");
    for (i, b) in l.bars.iter().enumerate() {
        let n = [b' ', b'b', b'a', b'r', b'0' + i as u8, b'='];
        log(core::str::from_utf8(&n).unwrap_or(" bar?="));
        print_hex(hw::OB_MEM_CPU + b.at);
        log("/");
        print_hex(b.size);
        if b.fallback { log("(mask="); print_hex(b.mask as u64); log(")"); }
        if b.moved { log("(nowrite orig="); print_hex(b.orig as u64); log(")"); }
    }
    log("\n[pcie] rc window=");
    match l.win {
        Some((lo, hi)) => { print_hex(lo); log("-"); print_hex(hi); }
        None => log("closed"),
    }
    log(" rc_win="); print_hex(l.rc_win as u64);
    log(" rc_cmd="); print_hex(l.rc_cmd as u64);
    log(" rp1_cmd="); print_hex(l.rp1_cmd as u64);
    log("\n");
}

/// Dword config accessor for the stage-15 BAR/window setup.
struct Cfg;
impl hw::CfgSpace for Cfg {
    fn rd(&mut self, bus: u8, off: u16) -> u32 { cfg32(bus, off) }
    fn wr(&mut self, bus: u8, off: u16, v: u32) { cfgw32(bus, off, v) }
}

static RP1_BAR1: AtomicU64 = AtomicU64::new(0);
static RP1_BAR1_SIZE: AtomicU64 = AtomicU64::new(0);

/// CPU physical address and size of RP1's BAR1 as placed and read back at
/// bring-up (inside the open root-port window), or None.
pub fn rp1_bar1() -> Option<(u64, u64)> {
    let size = RP1_BAR1_SIZE.load(Ordering::Acquire);
    if size == 0 { None } else { Some((RP1_BAR1.load(Ordering::Relaxed), size)) }
}


// ── kexec: nothing behind pcie2 may master after the jump ──────────────────

/// RP1 (01:00.0) and the root port (00:00.0) lose bus mastering, PERST#
/// holds RP1 in reset, then the bridge reset cuts the RC off the AXI bus.
/// Runs as the last `dma_shutdown` hook (Stage::Bus), interrupts off: after
/// it no config or RC access is possible, so `REGS` is cleared and every
/// later `cfg_access` returns all-ones instead of stalling the CPU.
fn kexec_reset() -> bool {
    let regs = REGS.load(Ordering::Acquire);
    let swinit = SWINIT.load(Ordering::Acquire);
    if regs == 0 || swinit == 0 { return true; }
    for bus in [1u8, 0] {
        let cmd = cfg32(bus, 0x04) & 0xffff;
        cfgw32(bus, 0x04, cmd & !0x4);
    }
    perst(true);
    sched::kthread::udelay(100);
    STATE.store(0, Ordering::Relaxed);
    let _g = CFG_LOCK.lock();
    REGS.store(0, Ordering::Release);
    let mut bus = MmioBus { rc: regs, swinit };
    let (_, bit) = hw::sw_init_set(hw::PCIE2_BRIDGE_RESET_ID);
    hw::bridge_assert(&mut bus) & bit != 0
}

/// Outcome of [`early_reset_identity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EarlyReset {
    /// The bridge was already held in reset; nothing behind pcie2 can reach DRAM.
    AlreadyInReset,
    /// PERST# asserted (it was released: `perstb` = 1) and the bridge put in reset.
    Reset { perstb_was: bool, held: bool },
}

/// New kernel, before the buddy allocator exists (rpi5 direct boot only):
/// a previous kernel may have left RP1 running (GEM RX ring, xHCI, RP1 DMA)
/// with pcie2 up, and its frames would land in pages this kernel is about to
/// call free. Holds RP1 in PERST# and the RC bridge in reset whatever
/// `pcie.rpi5` says; `init` brings it back out when PCIe is wanted.
///
/// # Safety
/// The physical BCM2712 MMIO at 0x10_0000_0000.. must be identity-mapped as
/// Device memory (entry stub PUD 64), and nothing else may touch pcie2.
pub unsafe fn early_reset_identity() -> EarlyReset {
    use leandros_lib::mmio::{read, write};
    let (bank, bit) = hw::sw_init_set(hw::PCIE2_BRIDGE_RESET_ID);
    let sw = hw::SW_INIT_REGS as usize + bank;
    if read((sw + hw::SW_INIT_STATUS) as *const u32) & bit != 0 { return EarlyReset::AlreadyInReset; }
    // Bridge released, so the RC block answers (a read while it is held in
    // reset would stall the CPU; see brcmstb_hw stage 5).
    let ctrl_pa = (hw::PCIE2_REGS as usize + hw::MISC_PCIE_CTRL) as *mut u32;
    let ctrl = read(ctrl_pa as *const u32);
    if ctrl & hw::PCIE_CTRL_PERSTB != 0 { write(ctrl_pa, ctrl & !hw::PCIE_CTRL_PERSTB); }
    let _ = read(ctrl_pa as *const u32);
    // ~100 us for in-flight TLPs to drain (no timer yet: status reads pace it).
    for _ in 0..2000 { let _ = read((sw + hw::SW_INIT_STATUS) as *const u32); }
    write(sw as *mut u32, bit);
    let held = read((sw + hw::SW_INIT_STATUS) as *const u32) & bit != 0;
    EarlyReset::Reset { perstb_was: ctrl & hw::PCIE_CTRL_PERSTB != 0, held }
}
