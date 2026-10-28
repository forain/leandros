//! x86_64 ACPI S3 orchestration (lane s3core). Design: artifacts/notes/design-lapsusp2-2026-10-27.md
//! sections 1 and 1.1, as amended by review-s3aml-2026-10-28.md items 6/7:
//!
//!  * `_WAK(3)` runs only after a real wake (or after the platform ignored
//!    SLP_EN, Linux behaviour). HP's `_WAK(3)` calls SMI 0xEA91 which resets
//!    the machine when no S3 was entered.
//!  * Everything that can fail runs before `_PTS(3)`. After `_PTS` only
//!    infallible steps remain, so the sleep is never abandoned half way.
//!
//! One CPU only (boot with `nosmp`, the AP park/unpark is lane s3cpus).
//! Behind `pm.s3=1`; `pm_test=core` runs everything but the platform sleep.
use alloc::format;
use alloc::string::String;
use core::arch::asm;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use arch_x86_64::{ioapic, rtc, s3, smp, timer};

const SLP_EN: u16 = 1 << 13;
const STS_ALL: u16 = 0x8731 | (1 << 14);

static BUSY: AtomicBool = AtomicBool::new(false);
static WOKE_SEEN: AtomicBool = AtomicBool::new(false);
static SAVED_PM1_EN: AtomicU16 = AtomicU16::new(0);
static mut SAVED_GPE_EN: [u8; 32] = [0; 32];

struct Regs { evt: u16, cnt_a: u16, cnt_b: u16, gpe0: u16, gpe_len: u16, ta: u16, tb: u16 }
static mut REGS: Regs = Regs { evt: 0, cnt_a: 0, cnt_b: 0, gpe0: 0, gpe_len: 0, ta: 0, tb: 0 };

static mut CANARY: [u64; 512] = [0; 512];
const CANARY_PAT: u64 = 0x5333_5F43_414E_4152;

fn say(s: &str) {
    crate::serial_print_str("[PM] ");
    crate::serial_print_str(s);
    crate::serial_print_str("\n");
}

#[inline] unsafe fn outb(p: u16, v: u8) { asm!("out dx, al", in("dx") p, in("al") v, options(nomem, nostack)); }
#[inline] unsafe fn outw(p: u16, v: u16) { asm!("out dx, ax", in("dx") p, in("ax") v, options(nomem, nostack)); }
#[inline] unsafe fn inb(p: u16) -> u8 { let v: u8; asm!("in al, dx", in("dx") p, out("al") v, options(nomem, nostack)); v }
#[inline] unsafe fn inw(p: u16) -> u16 { let v: u16; asm!("in ax, dx", in("dx") p, out("ax") v, options(nomem, nostack)); v }

fn knob(k: &str) -> Option<&'static str> {
    crate::boot_cmdline()?.split_ascii_whitespace().find_map(|t| t.strip_prefix(k).and_then(|r| r.strip_prefix('=')))
}

/// Can this machine do S3 at all (FADT/DSDT facts)?
pub fn valid() -> bool {
    match crate::power::acpi_power() {
        Some(p) => !p.hw_reduced && p.s3_found && p.pm1a_cnt != 0 && p.pm1a_evt != 0 && p.facs != 0
            && p.s3_typ_a < 8 && p.s3_typ_b < 8 && !(p.s5_found && p.s3_typ_a == p.slp_typ_a && p.s3_typ_b == p.slp_typ_b),
        None => false,
    }
}

/// The platform sleep call (runs with IF=0 from `pm_s3_lowlevel`). Mirrors ACPICA
/// `acpi_hw_legacy_sleep`. Returns only when the platform did not sleep.
extern "C" fn enter_s3() {
    unsafe {
        let r = &*core::ptr::addr_of!(REGS);
        outw(r.evt, 1 << 15);
        outw(r.evt, STS_ALL);
        let n = (r.gpe_len / 2).min(32);
        for i in 0..n { outb(r.gpe0 + i, 0xFF); }
        let en = r.evt + 2;
        SAVED_PM1_EN.store(inw(en), Ordering::Relaxed);
        for i in 0..n {
            (*addr_of_mut!(SAVED_GPE_EN))[i as usize] = inb(r.gpe0 + n + i);
            outb(r.gpe0 + n + i, 0);
        }
        // RTC alarm + power button wake; PCIEXP_WAKE_DIS.
        outw(en, (1 << 10) | (1 << 8) | (1 << 14));
        let va = inw(r.cnt_a) & !((7 << 10) | SLP_EN);
        outw(r.cnt_a, va | (r.ta << 10));
        let vb = if r.cnt_b != 0 { let v = inw(r.cnt_b) & !((7 << 10) | SLP_EN); outw(r.cnt_b, v | (r.tb << 10)); v } else { 0 };
        asm!("wbinvd", options(nomem, nostack));
        outw(r.cnt_a, va | (r.ta << 10) | SLP_EN);
        if r.cnt_b != 0 { outw(r.cnt_b, vb | (r.tb << 10) | SLP_EN); }
        // Still running: S3 did not happen. Wait up to ~2 s for WAK_STS.
        let mut i = 0u32;
        while i < 2_000_000 {
            if inw(r.evt) & (1 << 15) != 0 { WOKE_SEEN.store(true, Ordering::Relaxed); break; }
            outb(0x80, 0);
            i += 1;
        }
    }
}

fn facs_rd32(facs: usize, off: usize) -> u32 { unsafe { core::ptr::read_volatile((facs + off) as *const u32) } }
fn facs_wr32(facs: usize, off: usize, v: u32) { unsafe { core::ptr::write_volatile((facs + off) as *mut u32, v) } }

/// FADT DAY_ALRM (byte 113) of the raw FACP table.
fn fadt_day_alrm() -> u8 {
    for i in 0..64 {
        let Some((name, t)) = drivers::acpi_power::raw_table(i) else { break };
        if name == "FACP" { return t.get(113).copied().unwrap_or(0); }
    }
    0
}

/// `state` = mem. `core_only`: pm_test=core, everything except the platform sleep.
pub fn suspend(core_only: bool) -> i32 {
    if BUSY.swap(true, Ordering::AcqRel) { say("S3: busy"); return -16; }
    let r = suspend_inner(core_only);
    BUSY.store(false, Ordering::Release);
    r
}

fn suspend_inner(core_only: bool) -> i32 {
    let Some(p) = crate::power::acpi_power() else { say("S3 refused: no FADT"); return -19 };
    if !valid() { say("S3 refused: no usable \\_S3_ / PM1 registers"); return -22; }
    let n = smp::active_cpu_count();
    if n != 1 { say(&format!("S3 refused: {} CPUs online (boot with nosmp)", n)); return -16; }
    if unsafe { smp::arch_cpu_id() } != 0 { say("S3 refused: not on the BSP"); return -16; }
    let Some(facs) = drivers::acpi_power::facs_va() else { say("S3 refused: FACS not mapped"); return -19 };
    let sig = facs_rd32(facs, 0);
    if sig != 0x5343_4146 { say(&format!("S3 refused: FACS signature {:#x}", sig)); return -22; }
    let hwsig = facs_rd32(facs, 8);
    let facs_ver = unsafe { core::ptr::read_volatile((facs + 32) as *const u8) };
    let beep = knob("pm.s3_beep") == Some("1");
    let run_wak = knob("pm.s3_wak") != Some("0");
    say(&format!("S3: begin ({}), \\_S3_ SLP_TYP {}/{}, FACS ver {} hwsig {:#x}, beep {}, _WAK {}",
        if core_only { "pm_test=core, no platform sleep" } else { "real" }, p.s3_typ_a, p.s3_typ_b, facs_ver, hwsig, beep as u8, run_wak as u8));
    unsafe {
        let r = &mut *addr_of_mut!(REGS);
        *r = Regs { evt: p.pm1a_evt, cnt_a: p.pm1a_cnt, cnt_b: p.pm1b_cnt, gpe0: p.gpe0, gpe_len: p.gpe0_len,
            ta: p.s3_typ_a as u16, tb: p.s3_typ_b as u16 };
    }
    WOKE_SEEN.store(false, Ordering::Relaxed);

    // S1/S2: data safe on the stick before anything can go wrong.
    crate::syscall::sync_all();
    crate::klog::flush_now();

    // S5: devices (pci restore is deep after a real power loss).
    let flags = crate::pm::PM_S3;
    let mut rep = String::new();
    let (done, ok) = crate::pm::devs_suspend(flags, &mut rep);
    if !ok { crate::pm::devs_resume(done, flags, &mut rep); return -5; }

    drivers::acpi_power::pause_poll(true);

    // Everything fallible, before _PTS.
    if !core_only {
        if let Err(e) = unsafe { s3::prepare_wake(beep) } {
            say(&format!("S3 aborted before _PTS: {}", e));
            drivers::acpi_power::pause_poll(false);
            crate::pm::devs_resume(done, flags, &mut rep);
            return -12;
        }
        facs_wr32(facs, 12, s3::WAKE_PHYS as u32);
        if facs_ver >= 1 { facs_wr32(facs, 24, 0); facs_wr32(facs, 28, 0); }
        if fadt_day_alrm() == 0x0D { rtc::clear_day_alarm(); }
    }
    unsafe {
        let c = addr_of_mut!(CANARY) as *mut u64;
        for i in 0..512 { c.add(i).write_volatile(CANARY_PAT ^ i as u64); }
    }

    // _TTS(3), _PTS(3), _SST(3). From here on we never abort.
    if !core_only {
        let a = drivers::acpi_power::sleep_prepare(3);
        say(&format!("S3: _TTS/_PTS/_SST(3) ok={} in {} ms, dropped {}", a.ok, a.ms, a.dropped));
    }

    unsafe { asm!("cli", options(nomem, nostack)); }
    let mono = timer::monotonic_ns();
    let rtc0 = rtc::epoch_secs();
    let io = unsafe { ioapic::pm_save() };
    unsafe { s3::save_ctx(); }
    let mut state_lost = core_only;
    if core_only {
        unsafe { s3::restore_cpu_in_place(); }
    } else {
        crate::serial_print_str("[PM] S3: entering sleep now\n");
        let rc = unsafe { s3::pm_s3_lowlevel(addr_of_mut!(s3::S3_CTX), enter_s3) };
        state_lost = rc == 1;
    }

    // ---- resume (IF=0) ----
    let rr = unsafe { &*core::ptr::addr_of!(REGS) };
    let sts = unsafe { inw(rr.evt) };
    if !core_only {
        unsafe {
            // acpi_leave_sleep_state_prep: SLP_TYP = \_S0_ (0), SLP_EN = 0, SCI_EN = 1.
            let v = inw(rr.cnt_a) & !((7 << 10) | SLP_EN);
            outw(rr.cnt_a, v | 1);
            if rr.cnt_b != 0 { let v = inw(rr.cnt_b) & !((7 << 10) | SLP_EN); outw(rr.cnt_b, v); }
        }
    }
    let mut delta = 0u64;
    if state_lost {
        unsafe {
            if let Some(io) = io.as_ref() { s3::resume_irq_hw(io); } else { arch_x86_64::apic::init(); }
            delta = s3::rebase_clock(mono);
            s3::restart_timer();
        }
        if let Some(n) = rtc::epoch_secs() { sched::set_realtime_ns(n * 1_000_000_000); }
    }
    let cn = unsafe { addr_of_mut!(CANARY) as *mut u64 };
    let bad = (0..512).filter(|&i| unsafe { cn.add(i).read_volatile() } != CANARY_PAT ^ i as u64).count();
    if bad != 0 {
        // Memory was scribbled on: never let the block layer see this kernel.
        say(&format!("S3 VERIFY FAILED: {} of 512 canary words changed; halting with the block gate closed", bad));
        loop { unsafe { asm!("cli; hlt", options(nomem, nostack)); } }
    }
    let woke = state_lost || WOKE_SEEN.load(Ordering::Relaxed);
    let slept = match (rtc0, rtc::epoch_secs()) { (Some(a), Some(b)) => b.saturating_sub(a), _ => 0 };
    say(&format!("S3: {}; PM1_STS {:#06x} (RTC {} PWRBTN {} WAK {}), TSC delta {:#x}, slept ~{} s",
        if core_only { "core restore done" } else if state_lost { "RESUMED from S3" } else if woke { "WAK_STS set, state intact" } else { "sleep did NOT happen (fell through)" },
        sts, (sts >> 10) & 1, (sts >> 8) & 1, (sts >> 15) & 1, delta, slept));
    if !core_only {
        unsafe {
            outw(rr.evt, STS_ALL);
            let n = (rr.gpe_len / 2).min(32);
            for i in 0..n { outb(rr.gpe0 + i, 0xFF); outb(rr.gpe0 + n + i, (*core::ptr::addr_of!(SAVED_GPE_EN))[i as usize]); }
            outw(rr.evt + 2, SAVED_PM1_EN.load(Ordering::Relaxed));
        }
    }
    unsafe { asm!("sti", options(nomem, nostack)); }

    // R5: _WAK only now: a real wake, or the platform ignored SLP_EN (Linux behaviour).
    if !core_only {
        if run_wak {
            let w = drivers::acpi_power::sleep_wake(3);
            say(&format!("S3: _SST(2)/_WAK(3)/_SST(1) ok={} wak={:?} in {} ms", w.ok, w.wak, w.ms));
        } else {
            say("S3: _WAK skipped (pm.s3_wak=0)");
        }
        unsafe { outw(rr.evt, 1 << 15); outw(rr.evt, 1 << 8); }
        facs_wr32(facs, 12, 0);
        let hw2 = facs_rd32(facs, 8);
        if hw2 != hwsig { say(&format!("S3: hardware signature changed {:#x} -> {:#x}", hwsig, hw2)); }
        rtc::clear_alarm();
        drivers::acpi_power::tts(0);
    }
    drivers::acpi_power::pause_poll(false);
    drivers::acpi_power::poll();
    let mut out = String::new();
    crate::pm::devs_resume(done, flags, &mut out);
    if let Some(sec) = knob("pm.s3_reset_after").and_then(|v| v.parse::<u64>().ok()) {
        // Bring-up helper: without the device resume lanes the root stick is gone; come back on the good kernel.
        say(&format!("S3: pm.s3_reset_after={} s, then reset", sec));
        let until = sched::monotonic_ns() + sec * 1_000_000_000;
        while sched::monotonic_ns() < until { sched::kthread::msleep(50); }
        if knob("pm.s3_reset_flush") == Some("1") { crate::klog::flush_now(); }
        say("S3: resetting now");
        unsafe { asm!("cli", options(nomem, nostack)); arch_x86_64::power::restart(crate::power::acpi_power(), arch_x86_64::power::Method::Auto); }
    }
    say("S3: resume complete");
    0
}
