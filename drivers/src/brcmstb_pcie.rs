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
//!  1. rescal reset pulse; bridge SW_INIT asserted; PERST# asserted;
//!  2. bridge out of reset, SerDes out of IDDQ;
//!  3. 54 MHz PLL setup over MDIO, L1SS PM clock period;
//!  4. MISC_CTRL (SCB access, UR-on-read, 512 B burst), UBUS error suppression
//!     (reads that fail return all-ones instead of an SError) and timeouts;
//!  5. inbound RC_BAR1 (RAM) + RC_BAR2 (MIP doorbell), outbound windows;
//!  6. root-port class/endian/gen2/x4/ASPM-L1 setup, PERST# released;
//!  7. link-up poll (<= 300 ms), CRS visibility, bus numbers and window.

use crate::brcmstb_hw as hw;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;

/// Virtual address of the register block (0 until `init` mapped it).
static REGS: AtomicUsize = AtomicUsize::new(0);
/// Serialises EXT_CFG_INDEX + data access.
static CFG_LOCK: Mutex<()> = Mutex::new(());
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

fn bridge_reset(swinit: usize, assert: bool) {
    let (bank, bit) = hw::sw_init_set(hw::PCIE2_BRIDGE_RESET_ID);
    let off = bank + if assert { 0 } else { hw::SW_INIT_CLEAR };
    unsafe { leandros_lib::mmio::write((swinit + off) as *mut u32, bit) };
    sched::kthread::udelay(200);
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

/// Brings pcie2 up and registers it with `pci.rs`. Task context, once.
/// Never hangs: every poll is bounded; failures leave the bus unregistered.
pub fn init() {
    if STATE.load(Ordering::Relaxed) != 0 { return; }
    let regs = map(hw::PCIE2_REGS, hw::REG_SIZE);
    let rescal = map(hw::RESCAL_REGS & !0xfff, 0x1000);
    let swinit = map(hw::SW_INIT_REGS & !0xfff, 0x1000);
    if regs == 0 || rescal == 0 || swinit == 0 { return fail(2, "ioremap"); }
    let rescal = rescal + (hw::RESCAL_REGS & 0xfff) as usize;
    let swinit = swinit + (hw::SW_INIT_REGS & 0xfff) as usize;

    // 1. Resets.
    if !rescal_reset(rescal) { return fail(3, "rescal timeout"); }
    REGS.store(regs, Ordering::Release);
    bridge_reset(swinit, true);
    perst(true);
    sched::kthread::udelay(200);
    bridge_reset(swinit, false);
    // 2. SerDes power-up.
    rmw(hw::HARD_DEBUG, hw::HARD_DEBUG_SERDES_IDDQ, 0);
    sched::kthread::udelay(200);
    log("[pcie] rc rev ");
    print_hex(rd(hw::MISC_REVISION) as u64);
    log("\n");

    // 3. 54 MHz reference-clock PLL, L1SS PM clock period (18.52 ns).
    let mut ok = mdio_write(hw::MDIO_SET_ADDR_OFFSET, hw::PLL_54MHZ_BLOCK);
    for &(r, d) in hw::PLL_54MHZ_REGS.iter() { ok &= mdio_write(r, d); }
    if !ok { REGS.store(0, Ordering::Release); return fail(4, "mdio timeout"); }
    sched::kthread::udelay(200);
    rmw(hw::RC_PL_PHY_CTL_15, hw::PHY_CTL_15_PM_CLK_PERIOD_MASK, 0x12);

    // 4. Bus behaviour and error suppression.
    rmw(hw::MISC_CTRL, hw::MISC_CTRL_MAX_BURST_MASK,
        hw::MISC_CTRL_SCB_ACCESS_EN | hw::MISC_CTRL_CFG_READ_UR_MODE
        | hw::MISC_CTRL_RCB_MPS_MODE | hw::MISC_CTRL_RCB_64B_MODE
        | (2 << hw::MISC_CTRL_MAX_BURST_SHIFT));
    rmw(hw::MISC_UBUS_CTRL, 0, hw::UBUS_CTRL_REPLY_ERR_DIS | hw::UBUS_CTRL_REPLY_DECERR_DIS);
    wr(hw::MISC_AXI_READ_ERROR_DATA, 0xffff_ffff);
    wr(hw::MISC_UBUS_TIMEOUT, 0x0b2d_0000);
    wr(hw::MISC_RC_CONFIG_RETRY_TIMEOUT, 0x0aba_0000);

    // 5. Inbound: BAR1 = RAM, BAR2 = MIP doorbell, BAR3 off. Outbound windows.
    for (o, v) in hw::inbound_win_regs(1, hw::IB_RAM_PCI, hw::IB_RAM_CPU, hw::IB_RAM_SIZE) { wr(o, v); }
    for (o, v) in hw::inbound_win_regs(2, hw::MSI_PCI_ADDR, hw::MIP0_REGS, 0x1000) { wr(o, v); }
    for (o, v) in hw::inbound_win_regs(3, 0, 0, 0) { wr(o, v); }
    let bl0 = rd(hw::MISC_CPU2PCIE_WIN0_BASE_LIMIT);
    for (o, v) in hw::outbound_win_regs(0, hw::OB_MEM_CPU, hw::OB_MEM_PCI, hw::OB_MEM_SIZE, bl0) { wr(o, v); }
    let bl1 = rd(hw::MISC_CPU2PCIE_WIN0_BASE_LIMIT + 4);
    for (o, v) in hw::outbound_win_regs(1, hw::OB_PREF_CPU, hw::OB_PREF_PCI, hw::OB_PREF_SIZE, bl1) { wr(o, v); }

    // 6. Root port identity and link parameters, then release PERST#.
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

    // 7. Link-up: 100 ms PERST settle (PCIe T_PVPERL), then poll up to 200 ms.
    sched::kthread::msleep(100);
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
    print_hex(gen as u64);
    log(" x");
    print_hex(width as u64);
    log("\n");
    rmw(hw::RC_RTCTL, 0, hw::RTCTL_CRSSVE);

    // Root port: bus 0 → 1..1, memory decode + bus master.
    cfgw32(0, 0x18, (cfg32(0, 0x18) & 0xff00_0000) | (1 << 16) | (1 << 8));
    let id = cfg32(1, 0);
    if id & 0xffff != hw::RP1_VENDOR as u32 || id >> 16 != hw::RP1_DEVICE as u32 {
        log("[pcie] FAIL no RP1 at 01:00.0 id=");
        print_hex(id as u64);
        log("\n");
        STATE.store(6, Ordering::Relaxed);
        return;
    }
    let end = assign_rp1_bars();
    cfgw32(0, 0x20, hw::bridge_mem_window(hw::OB_MEM_PCI, end));
    cfgw32(0, 0x24, 0x0000_fff0); // prefetchable window closed
    cfg_access(0, 0, 0, 0x1c, 2, Some(0x00f0)); // I/O window closed
    cfg_access(0, 0, 0, 0x04, 2, Some(0x0006));

    crate::pci::set_cfg_backend(cfg_access, hw::OB_MEM_CPU - hw::OB_MEM_PCI, 1);
    STATE.store(1, Ordering::Relaxed);
    log("[pcie] 0000:01:00.0 1de4:0001");
    for i in 0..6u16 {
        let v = cfg32(1, 0x10 + 4 * i);
        if v & 1 == 0 && (v & !0xf) != 0 || i == 1 {
            let n = [b' ', b'b', b'a', b'r', b'0' + i as u8, b'='];
            log(core::str::from_utf8(&n).unwrap_or(" bar?="));
            print_hex(hw::OB_MEM_CPU + (v & !0xf) as u64);
        }
    }
    log("\n");
}

/// Sizes RP1's memory BARs and places them largest-first from PCI 0 (BAR1,
/// the peripheral block, lands at 0 exactly as the firmware DTB's `rp1`
/// `ranges` assume). Returns the end of the used space.
fn assign_rp1_bars() -> u64 {
    let mut reqs = alloc::vec::Vec::new();
    let cmd = cfg32(1, 0x04);
    cfgw32(1, 0x04, cmd & !0x7);
    let mut i = 0u16;
    while i < 6 {
        let off = 0x10 + 4 * i;
        let orig = cfg32(1, off);
        if orig & 1 != 0 { i += 1; continue; }
        let is64 = orig & 6 == 4;
        cfgw32(1, off, 0xffff_ffff);
        let lo = cfg32(1, off);
        let hi = if is64 { let h = cfg32(1, off + 4); cfgw32(1, off + 4, 0xffff_ffff); let s = cfg32(1, off + 4); cfgw32(1, off + 4, h); Some(s) } else { None };
        cfgw32(1, off, orig);
        let size = hw::bar_size(lo, hi);
        if size != 0 { reqs.push(hw::BarReq { index: i as usize, size, is64 }); }
        i += if is64 { 2 } else { 1 };
    }
    let (placed, end) = hw::assign_bars(&reqs, hw::OB_MEM_PCI, hw::OB_MEM_PCI + hw::OB_MEM_SIZE);
    for (idx, at) in placed {
        let off = 0x10 + 4 * idx as u16;
        let flags = cfg32(1, off) & 0xf;
        cfgw32(1, off, at as u32 | flags);
        if reqs.iter().any(|r| r.index == idx && r.is64) { cfgw32(1, off + 4, (at >> 32) as u32); }
    }
    cfgw32(1, 0x04, cmd | 0x2); // memory decode on (bus master is the driver's call)
    end
}
