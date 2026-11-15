//! GICv3 Interrupt Translation Service: LPIs and MSI/MSI-X delivery.
//!
//! A PCI function signals an MSI by writing its EventID to `GITS_TRANSLATER`
//! (ITS base + 0x10040). The ITS looks the (DeviceID, EventID) pair up in the
//! device's interrupt translation table (ITT), finds the LPI INTID and the
//! collection (= target redistributor) and sets the LPI pending there; the CPU
//! then takes it as an ordinary IRQ whose INTID is >= 8192.
//!
//! # Scope
//!
//! * One ITS (the first discovered), one collection (ICID 0) that targets the
//!   boot CPU's redistributor, so every LPI is delivered to the BSP. LPIs are
//!   enabled on that single redistributor; the other CPUs never see them.
//! * `LPI_COUNT` LPIs (8192..8192+LPI_COUNT) with a flat LPI configuration
//!   table of 8 KiB (IDbits = 13) and a 64 KiB pending table.
//! * Device table: flat, DeviceID range limited to 16 bits (`MAX_DEVID_BITS`);
//!   a larger DeviceID (e.g. an IORT output offset) is refused and the driver
//!   falls back to INTx / polling. Two-level (Indirect) device tables are not
//!   implemented. Collection table: one page.
//! * Commands are issued synchronously (CWRITER, then wait for CREADR), under
//!   a spin lock with IRQs masked. The queue is 4 KiB (128 commands).
//! * DeviceID = PCI RID (`bus << 8 | dev << 3 | func`) unless an IORT ID
//!   mapping was installed with [`set_rid_map`].
//! * The table memory is cleaned to the point of coherency after every CPU
//!   write, so an ITS that is not cache coherent also works.
//!
//! `gic.its=0` on the kernel command line leaves the ITS alone (INTx stays).
//! A kernel entered by kexec finds the previous kernel's ITS/LPI state;
//! [`quiesce`] turns it off on the way out and [`init`] refuses to proceed if
//! the redistributor still has LPIs enabled (the pre-existing tables cannot be
//! trusted), in which case drivers keep INTx.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use leandros_lib::mmio::{MmioRd, MmioWr};
use sched::ksync::KSpinLock;

use super::gic::{self, gicr_r32, gicr_r64, gicr_w32, gicr_w64};

const GITS_CTLR: usize = 0x0000;
const GITS_TYPER: usize = 0x0008;
const GITS_CBASER: usize = 0x0080;
const GITS_CWRITER: usize = 0x0088;
const GITS_CREADR: usize = 0x0090;
const GITS_BASER: usize = 0x0100;
/// Offset of GITS_TRANSLATER from the ITS base (frame 1 + 0x40).
const TRANSLATER: u64 = 0x1_0040;

const GICR_CTLR: usize = 0x000;
const GICR_PROPBASER: usize = 0x070;
const GICR_PENDBASER: usize = 0x078;

/// First LPI INTID.
pub const LPI_BASE: u32 = 8192;
/// LPIs this kernel hands out.
pub const LPI_COUNT: usize = 256;
/// GICR_PROPBASER.IDbits: INTIDs up to 2^(IDBITS+1)-1.
const IDBITS: u64 = 13;
const CONF_BYTES: usize = 8192; // 2^(IDBITS+1) - 8192
const PEND_ORDER: usize = 4; // 64 KiB, the PENDBASER alignment
const CMDQ_BYTES: usize = 4096;
const MAX_DEVID_BITS: u32 = 16;
const MAX_DEVS: usize = 16;
/// EventIDs per device (the ITT is sized for this many).
const EVENTS_PER_DEV: u32 = 32;
const LPI_PRIORITY: u8 = 0xA0;

const MAX_ITS: usize = 4;
const MAX_MAP: usize = 8;

static ACPI_BASES: [AtomicU64; MAX_ITS] = [const { AtomicU64::new(0) }; MAX_ITS];
static READY: AtomicBool = AtomicBool::new(false);
static TRANSLATER_PA: AtomicU64 = AtomicU64::new(0);
/// Virtual base of the redistributor with LPIs enabled (for `quiesce`).
static LPI_RD: AtomicUsize = AtomicUsize::new(0);
/// ITS control frame VA (for `quiesce`).
static ITS_VA: AtomicUsize = AtomicUsize::new(0);
/// IORT ID mappings: (input base, count, output base).
static MAP: KSpinLock<([(u32, u32, u32); MAX_MAP], usize)> = KSpinLock::new(([(0, 0, 0); MAX_MAP], 0));

#[derive(Clone, Copy)]
struct Dev {
    devid: u32,
    used: bool,
    itt_phys: usize,
    /// Allocated EventIDs.
    events: u32,
}

struct State {
    base: usize,
    cmd: usize,
    cmd_phys: usize,
    wr: usize,
    conf: usize,
    /// GICR_TYPER-derived RDbase field for MAPC / SYNC.
    rdbase: u64,
    itt_esz: usize,
    devid_max: u32,
    devs: [Dev; MAX_DEVS],
    lpis: [u64; LPI_COUNT / 64],
    /// LPI index -> (device slot, EventID).
    owner: [(u8, u8); LPI_COUNT],
}

static ST: KSpinLock<Option<State>> = KSpinLock::new(None);

/// Record the ITS bases the firmware's MADT listed (ACPI path).
pub fn set_acpi_bases(b: &[u64]) {
    for (i, &v) in b.iter().take(MAX_ITS).enumerate() {
        ACPI_BASES[i].store(v, Ordering::Relaxed);
    }
}

/// ITS frames recorded by [`set_acpi_bases`] (0 = unused slot).
pub fn acpi_base(i: usize) -> u64 {
    if i < MAX_ITS { ACPI_BASES[i].load(Ordering::Relaxed) } else { 0 }
}

/// Install an IORT PCI-RC ID mapping: RIDs `[input, input+count)` become
/// DeviceIDs `output + (rid - input)`. Without any mapping DeviceID = RID.
pub fn set_rid_map(input: u32, count: u32, output: u32) {
    let mut g = MAP.lock();
    let n = g.1;
    if n < MAX_MAP { g.0[n] = (input, count, output); g.1 = n + 1; }
}

fn devid_for_rid(rid: u32) -> u32 {
    let g = MAP.lock();
    if g.1 == 0 { return rid; }
    for i in 0..g.1 {
        let (inp, cnt, out) = g.0[i];
        if rid >= inp && rid - inp < cnt { return out + (rid - inp); }
    }
    u32::MAX
}

fn say(s: &str) { crate::uart::serial_print_str(s); }
fn hex(v: usize) { crate::uart::print_hex(v); }

/// Clean `len` bytes at `va` to the point of coherency.
unsafe fn clean(va: usize, len: usize) {
    let mut p = va & !63;
    let end = va + len;
    while p < end {
        core::arch::asm!("dc cvac, {}", in(reg) p, options(nostack));
        p += 64;
    }
    core::arch::asm!("dsb sy", options(nostack));
}

fn alloc_zeroed(order: usize) -> Option<(usize, usize)> {
    let pa = mm::buddy::alloc(order)?;
    let va = mm::phys_to_virt(pa);
    unsafe {
        core::ptr::write_bytes(va as *mut u8, 0, 4096 << order);
        clean(va, 4096 << order);
    }
    Some((pa, va))
}

fn order_for(bytes: usize) -> usize {
    let mut o = 0;
    while (4096usize << o) < bytes { o += 1; }
    o
}

unsafe fn r32(b: usize, off: usize) -> u32 { ((b + off) as *const u32).mmio_read() }
unsafe fn w32(b: usize, off: usize, v: u32) { ((b + off) as *mut u32).mmio_write(v) }
unsafe fn r64(b: usize, off: usize) -> u64 { ((b + off) as *const u64).mmio_read() }
unsafe fn w64(b: usize, off: usize, v: u64) { ((b + off) as *mut u64).mmio_write(v) }

/// Wait until GITS_CTLR.Quiescent.
unsafe fn wait_quiescent(b: usize) -> bool {
    for _ in 0..2_000_000 {
        if r32(b, GITS_CTLR) & (1 << 31) != 0 { return true; }
    }
    false
}

/// Program one GITS_BASER for `Devices` (type 1) or `Collections` (type 4).
unsafe fn setup_baser(b: usize, idx: usize, typer: u64, devbits: u32) -> bool {
    let off = GITS_BASER + idx * 8;
    let v = r64(b, off);
    let ty = (v >> 56) & 7;
    if ty != 1 && ty != 4 { return true; } // unused / vPE tables: leave alone
    let esz = (((v >> 48) & 0x1F) + 1) as usize;
    let bytes = if ty == 1 { esz << devbits } else { 4096 };
    // Page size: ask for 4 KiB, take whatever the ITS settles on.
    w64(b, off, 0);
    let mut psz_code = 0u64;
    w64(b, off, psz_code << 8);
    psz_code = (r64(b, off) >> 8) & 3;
    let psize: usize = match psz_code { 0 => 4096, 1 => 16384, _ => 65536 };
    let order = order_for(bytes.max(psize));
    let pages = (4096usize << order) / psize;
    if pages > 256 { say("[ITS] table too large for BASER\n"); return false; }
    let Some((pa, _)) = alloc_zeroed(order) else { say("[ITS] table alloc failed\n"); return false; };
    let mut val = (1u64 << 63) | (7 << 59) | (1 << 10) | (psz_code << 8) | (pages as u64 - 1)
        | (pa as u64 & 0x0000_FFFF_FFFF_F000);
    w64(b, off, val);
    if (r64(b, off) >> 10) & 3 == 0 {
        // Non-shareable ITS: non-cacheable table attributes.
        val = (val & !(7 << 59) & !(3 << 10)) | (1 << 59);
        w64(b, off, val);
    }
    let _ = typer;
    r64(b, off) >> 63 != 0
}

/// Bring up the ITS at physical `its_pa` and enable LPIs on this CPU's
/// redistributor. Run once, on the boot CPU, after the allocator and the
/// kernel VA window exist. Returns true when MSI delivery is usable.
pub fn init(its_pa: u64) -> bool {
    if its_pa == 0 { return false; }
    unsafe { init_inner(its_pa) }
}

unsafe fn init_inner(its_pa: u64) -> bool {
    if !gic::is_v3() { return false; }
    let Some(b) = mm::kva::ioremap(its_pa, 0x2_0000, mm::kva::CacheMode::Uncached) else {
        say("[ITS] ioremap failed\n");
        return false;
    };
    let typer = r64(b, GITS_TYPER);
    let itt_esz = (((typer >> 4) & 0xF) + 1) as usize;
    let devbits = (((typer >> 13) & 0x1F) + 1) as u32;
    let pta = typer & (1 << 19) != 0;
    say("[ITS] base="); hex(its_pa as usize);
    say(" typer="); hex(typer as usize);
    say(" devbits="); hex(devbits as usize);
    say(" itt_esz="); hex(itt_esz);
    say(if pta { " pta=1\n" } else { " pta=0\n" });
    let devbits = devbits.min(MAX_DEVID_BITS);

    // Redistributor of this CPU: LPIs off before we start.
    let Some(rd) = gic::own_redistributor() else { say("[ITS] no redistributor\n"); return false; };
    if gicr_r32(rd, GICR_CTLR) & 1 != 0 {
        gicr_w32(rd, GICR_CTLR, gicr_r32(rd, GICR_CTLR) & !1);
        gic::gicr_wait_rwp(rd);
        if gicr_r32(rd, GICR_CTLR) & 1 != 0 {
            say("[ITS] LPIs already enabled by a previous kernel; ITS not used\n");
            return false;
        }
    }

    // ITS off while its tables move.
    if r32(b, GITS_CTLR) & 1 != 0 {
        w32(b, GITS_CTLR, r32(b, GITS_CTLR) & !1);
    }
    if !wait_quiescent(b) { say("[ITS] never quiescent\n"); return false; }
    for i in 0..8 {
        if !setup_baser(b, i, typer, devbits) { return false; }
    }

    // Command queue.
    let Some((cq_pa, cq_va)) = alloc_zeroed(order_for(CMDQ_BYTES)) else { return false; };
    let mut cb = (1u64 << 63) | (7 << 59) | (1 << 10) | (cq_pa as u64 & 0x000F_FFFF_FFFF_F000) | ((CMDQ_BYTES / 4096) as u64 - 1);
    w64(b, GITS_CBASER, cb);
    if (r64(b, GITS_CBASER) >> 10) & 3 == 0 {
        cb = (cb & !(7 << 59) & !(3 << 10)) | (1 << 59);
        w64(b, GITS_CBASER, cb);
    }
    w64(b, GITS_CWRITER, 0);

    // LPI tables for this redistributor.
    let Some((conf_pa, conf_va)) = alloc_zeroed(order_for(CONF_BYTES)) else { return false; };
    let Some((pend_pa, _)) = alloc_zeroed(PEND_ORDER) else { return false; };
    for i in 0..LPI_COUNT {
        ((conf_va + i) as *mut u8).write_volatile((LPI_PRIORITY & 0xFC) | 2); // disabled, RES1 bit
    }
    clean(conf_va, CONF_BYTES);
    let mut prop = (conf_pa as u64 & 0x000F_FFFF_FFFF_F000) | IDBITS | (7 << 7) | (1 << 10);
    gicr_w64(rd, GICR_PROPBASER, prop);
    if (gicr_r64(rd, GICR_PROPBASER) >> 10) & 3 == 0 {
        prop = (prop & !(7 << 7) & !(3 << 10)) | (1 << 7);
        gicr_w64(rd, GICR_PROPBASER, prop);
    }
    let mut pend = (pend_pa as u64 & 0x000F_FFFF_FFFF_0000) | (7 << 7) | (1 << 10) | (1 << 62);
    gicr_w64(rd, GICR_PENDBASER, pend);
    if (gicr_r64(rd, GICR_PENDBASER) >> 10) & 3 == 0 {
        pend = (pend & !(7 << 7) & !(3 << 10)) | (1 << 7);
        gicr_w64(rd, GICR_PENDBASER, pend);
    }
    gicr_w32(rd, GICR_CTLR, gicr_r32(rd, GICR_CTLR) | 1);
    gic::gicr_wait_rwp(rd);
    LPI_RD.store(rd, Ordering::Release);

    // RDbase field of MAPC / SYNC.
    let rdbase = if pta {
        (mm::virt_to_phys(rd) as u64) >> 16
    } else {
        (gicr_r64(rd, 0x008) >> 8) & 0xFFFF
    };

    w32(b, GITS_CTLR, r32(b, GITS_CTLR) | 1);

    let mut st = State {
        base: b, cmd: cq_va, cmd_phys: cq_pa, wr: 0, conf: conf_va, rdbase, itt_esz,
        devid_max: (1u32 << devbits) - 1,
        devs: [Dev { devid: 0, used: false, itt_phys: 0, events: 0 }; MAX_DEVS],
        lpis: [0; LPI_COUNT / 64],
        owner: [(0, 0); LPI_COUNT],
    };
    let ok = cmd_mapc(&mut st, 0, true) && cmd_sync(&mut st);
    if !ok {
        say("[ITS] collection map failed; ITS not used\n");
        w32(b, GITS_CTLR, r32(b, GITS_CTLR) & !1);
        return false;
    }
    ITS_VA.store(b, Ordering::Release);
    TRANSLATER_PA.store(its_pa + TRANSLATER, Ordering::Release);
    let _ = st.cmd_phys;
    *ST.lock() = Some(st);
    READY.store(true, Ordering::Release);
    say("[ITS] ready: LPIs "); hex(LPI_BASE as usize); say("+"); hex(LPI_COUNT);
    say(" collection 0 -> redistributor RDbase="); hex(rdbase as usize); say("\n");
    true
}

/// Leave the ITS and the redistributor as a fresh boot would find them
/// (kexec): ITS disabled, LPIs off if the redistributor lets us.
pub fn quiesce() {
    if !READY.load(Ordering::Acquire) { return; }
    unsafe {
        let b = ITS_VA.load(Ordering::Acquire);
        if b != 0 {
            w32(b, GITS_CTLR, r32(b, GITS_CTLR) & !1);
            let _ = wait_quiescent(b);
        }
        let rd = LPI_RD.load(Ordering::Acquire);
        if rd != 0 {
            gicr_w32(rd, GICR_CTLR, gicr_r32(rd, GICR_CTLR) & !1);
        }
    }
    READY.store(false, Ordering::Release);
}

/// True once [`init`] succeeded.
pub fn available() -> bool { READY.load(Ordering::Acquire) }

// ── commands ───────────────────────────────────────────────────────────────

unsafe fn submit(st: &mut State, c: [u64; 4]) -> bool {
    let slot = st.cmd + st.wr;
    for (i, v) in c.iter().enumerate() { ((slot + i * 8) as *mut u64).write_volatile(*v); }
    clean(slot, 32);
    st.wr = (st.wr + 32) % CMDQ_BYTES;
    w64(st.base, GITS_CWRITER, st.wr as u64);
    for _ in 0..4_000_000 {
        let rd = r64(st.base, GITS_CREADR);
        if rd & 1 != 0 { say("[ITS] command queue stalled\n"); return false; }
        if (rd & 0xF_FFE0) as usize == st.wr { return true; }
    }
    say("[ITS] command timeout\n");
    false
}

unsafe fn cmd_mapc(st: &mut State, icid: u64, valid: bool) -> bool {
    let rd = st.rdbase;
    submit(st, [0x09, 0, icid | (rd << 16) | ((valid as u64) << 63), 0])
}
unsafe fn cmd_sync(st: &mut State) -> bool {
    let rd = st.rdbase;
    submit(st, [0x05, 0, rd << 16, 0])
}
unsafe fn cmd_mapd(st: &mut State, devid: u32, itt_pa: usize, size_bits: u64, valid: bool) -> bool {
    submit(st, [0x08 | ((devid as u64) << 32), size_bits,
        (itt_pa as u64 & 0x000F_FFFF_FFFF_FF00) | ((valid as u64) << 63), 0])
}
unsafe fn cmd_mapti(st: &mut State, devid: u32, event: u32, lpi: u32) -> bool {
    submit(st, [0x0A | ((devid as u64) << 32), event as u64 | ((lpi as u64) << 32), 0, 0])
}
unsafe fn cmd_inv(st: &mut State, devid: u32, event: u32) -> bool {
    submit(st, [0x0C | ((devid as u64) << 32), event as u64, 0, 0])
}
unsafe fn cmd_discard(st: &mut State, devid: u32, event: u32) -> bool {
    submit(st, [0x0F | ((devid as u64) << 32), event as u64, 0, 0])
}

// ── MSI allocation ─────────────────────────────────────────────────────────

/// One MSI message: `(address, data, lpi)`.
pub struct Msi { pub addr: u64, pub data: u32, pub lpi: u32 }

/// Allocate an LPI for the PCI function with requester ID `rid`, map it in
/// the ITS and enable it. The caller programs `addr`/`data` into the device.
pub fn alloc_msi(rid: u32) -> Option<Msi> {
    if !available() { return None; }
    let devid = devid_for_rid(rid);
    let mut g = ST.lock();
    let st = g.as_mut()?;
    if devid > st.devid_max { return None; }
    // Device slot (MAPD on first use).
    let mut slot = MAX_DEVS;
    for i in 0..MAX_DEVS { if st.devs[i].used && st.devs[i].devid == devid { slot = i; break; } }
    if slot == MAX_DEVS {
        let free = (0..MAX_DEVS).find(|&i| !st.devs[i].used)?;
        let bytes = (EVENTS_PER_DEV as usize * st.itt_esz).max(256);
        let (itt_pa, _) = alloc_zeroed(order_for(bytes))?;
        let bits = EVENTS_PER_DEV.trailing_zeros() as u64; // 2^bits events -> Size = bits - 1
        if !unsafe { cmd_mapd(st, devid, itt_pa, bits - 1, true) } || !unsafe { cmd_sync(st) } {
            mm::buddy::free(itt_pa, order_for(bytes));
            return None;
        }
        st.devs[free] = Dev { devid, used: true, itt_phys: itt_pa, events: 0 };
        slot = free;
    }
    let ev = (0..EVENTS_PER_DEV).find(|&e| st.devs[slot].events & (1 << e) == 0)?;
    let idx = (0..LPI_COUNT).find(|&i| st.lpis[i / 64] & (1 << (i % 64)) == 0)?;
    let lpi = LPI_BASE + idx as u32;
    unsafe {
        if !cmd_mapti(st, devid, ev, lpi) { return None; }
        let c = st.conf + idx;
        (c as *mut u8).write_volatile((LPI_PRIORITY & 0xFC) | 2 | 1);
        clean(c, 1);
        if !cmd_inv(st, devid, ev) || !cmd_sync(st) { return None; }
    }
    st.devs[slot].events |= 1 << ev;
    st.lpis[idx / 64] |= 1 << (idx % 64);
    st.owner[idx] = (slot as u8, ev as u8);
    Some(Msi { addr: TRANSLATER_PA.load(Ordering::Acquire), data: ev, lpi })
}

/// Release an LPI from [`alloc_msi`]: unmap it and disable it.
pub fn free_msi(lpi: u32) {
    if !available() || lpi < LPI_BASE || (lpi - LPI_BASE) as usize >= LPI_COUNT { return; }
    let idx = (lpi - LPI_BASE) as usize;
    let mut g = ST.lock();
    let Some(st) = g.as_mut() else { return };
    if st.lpis[idx / 64] & (1 << (idx % 64)) == 0 { return; }
    let (slot, ev) = st.owner[idx];
    let devid = st.devs[slot as usize].devid;
    unsafe {
        let c = st.conf + idx;
        (c as *mut u8).write_volatile((LPI_PRIORITY & 0xFC) | 2);
        clean(c, 1);
        let _ = cmd_discard(st, devid, ev as u32);
        let _ = cmd_sync(st);
    }
    st.devs[slot as usize].events &= !(1 << ev);
    st.lpis[idx / 64] &= !(1 << (idx % 64));
}

// ── C ABI for `drivers::irq` ───────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn arch_its_available() -> bool { available() }

/// Allocate an MSI for requester ID `rid`; on success fills `addr`, `data`,
/// `lpi` and returns true.
#[no_mangle]
pub unsafe extern "C" fn arch_its_alloc_msi(rid: u32, addr: *mut u64, data: *mut u32, lpi: *mut u32) -> bool {
    match alloc_msi(rid) {
        Some(m) => { *addr = m.addr; *data = m.data; *lpi = m.lpi; true }
        None => false,
    }
}

#[no_mangle]
pub extern "C" fn arch_its_free_msi(lpi: u32) { free_msi(lpi) }
