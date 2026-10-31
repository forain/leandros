//! `reboot(KEXEC)` on aarch64: stop every other CPU, then jump through the
//! purgatory (`purgatory_aarch64.s`) on dense CPU 0.
//!
//! `prepare` runs first, in task context, while the kernel is still whole:
//! it allocates the purgatory page and an identity map (TTBR0, 1 GiB blocks
//! over every GiB that holds RAM) in pages outside every destination range,
//! copies the purgatory there and fills [`PLAN`]. After that nothing on the
//! jump path allocates or takes a lock.
//!
//! `stop_and_jump` (interrupts masked, devices quiesced) arms the IRQ divert
//! hook and sends SGI 1 to every other CPU. Each CPU's next interrupt ends in
//! [`on_ipi`]: CPU 0 runs [`final_cpu0`], every other one turns its MMU off
//! from the AP park page (a copy of the purgatory in the FDT block, which
//! the next kernel keeps) and calls PSCI CPU_OFF there (`park_off`), or on
//! rpi5 stays in that page's mailbox loop (TF-A never restarts a CPU it
//! powered off; `boot::mpidr::kexec_cpu_off`). The
//! new kernel parks any CPU whose MPIDR affinity != 0 (entry_aarch64.s), so the
//! jump must be made by the boot CPU; when the caller is not CPU 0 it powers
//! itself off after the IPIs and CPU 0 takes over.

use core::sync::atomic::{AtomicBool, Ordering};

use super::{Loaded, EINVAL, ENOMEM};

extern "C" {
    fn kexec_purgatory_start();
    fn kexec_purgatory_off();
    fn kexec_purgatory_end();
}

core::arch::global_asm!(include_str!("purgatory_aarch64.s"));

/// Offset of the (start, end) range table inside the purgatory page.
const RANGES_OFF: usize = 0x800;
const MAX_RANGES: usize = (4096 - RANGES_OFF) / 16;

#[derive(Clone, Copy)]
struct Plan {
    purgatory: u64,
    ttbr0: u64,
    tcr: u64,
    head: u64,
    entry: u64,
    fdt: u64,
    ranges: u64,
    nranges: u64,
    /// AP park page (FDT block + `info::AP_PARK_OFF`), 0 = none.
    park: u64,
    /// 1 = secondaries call PSCI CPU_OFF, 0 = they stay in the park loop
    /// (`boot::mpidr::kexec_cpu_off`).
    cpu_off: u64,
}

static mut PLAN: Plan = Plan { purgatory: 0, ttbr0: 0, tcr: 0, head: 0, entry: 0, fdt: 0, ranges: 0, nranges: 0, park: 0, cpu_off: 1 };
static ARMED: AtomicBool = AtomicBool::new(false);

/// Per-CPU stage words in the park page (8 B per dense CPU, 16 CPUs):
/// 1 divert entry, 2 after gic/timer off, 3 just before the `br`, 4 park page entry.
const STAGE_OFF: u64 = 0x980;
static CLEAN_PTR: [core::sync::atomic::AtomicUsize; 16] = [const { core::sync::atomic::AtomicUsize::new(0) }; 16];
static CLEAN_LEN: [core::sync::atomic::AtomicUsize; 16] = [const { core::sync::atomic::AtomicUsize::new(0) }; 16];

fn stage(n: u64) {
    let park = unsafe { core::ptr::addr_of!(PLAN.park).read() };
    let me = unsafe { arch_aarch64::smp::arch_cpu_id() };
    if park == 0 || me >= 16 { return; }
    let w = mm::phys_to_virt((park + STAGE_OFF) as usize + 8 * me) as *mut u64;
    unsafe {
        w.write_volatile(n);
        core::arch::asm!("dc civac, {}", "dsb sy", in(reg) w as usize, options(nostack));
    }
}

fn note_clean() {
    let me = unsafe { arch_aarch64::smp::arch_cpu_id() };
    if me >= 16 { return; }
    let r = sched::irqctx::sleep_forbidden_reason().unwrap_or("clean");
    CLEAN_LEN[me].store(r.len(), Ordering::Relaxed);
    CLEAN_PTR[me].store(r.as_ptr() as usize, Ordering::Release);
}

fn read_stage(p: u64, i: usize) -> u64 {
    let w = mm::phys_to_virt((p + STAGE_OFF) as usize + 8 * i) as *const u64;
    unsafe {
        core::arch::asm!("dc civac, {}", "dsb sy", in(reg) w as usize, options(nostack));
        w.read_volatile()
    }
}

fn log(s: &str) {
    super::prevlog::tee(s.as_bytes());
    arch_aarch64::uart::serial_print_str(s);
}
/// 16 hex digits, as `uart::print_hex` printed them.
fn hex(v: u64) {
    let mut b = [0u8; 16];
    for (i, c) in b.iter_mut().enumerate() {
        let d = ((v >> (60 - 4 * i)) & 0xf) as u8;
        *c = if d < 10 { b'0' + d } else { b'a' + d - 10 };
    }
    if let Ok(s) = core::str::from_utf8(&b) { log(s); }
}
fn dec(mut n: u64) {
    let mut b = [0u8; 20];
    let mut i = b.len();
    loop { i -= 1; b[i] = b'0' + (n % 10) as u8; n /= 10; if n == 0 { break; } }
    if let Ok(s) = core::str::from_utf8(&b[i..]) { log(s); }
}

/// Build the purgatory page and identity map for `img`. Task context.
pub fn prepare(img: &Loaded, map: &[super::info::Raw]) -> Result<(), isize> {
    let (mair, tcr, mmfr0): (u64, u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, mair_el1", out(reg) mair, options(nomem, nostack));
        core::arch::asm!("mrs {}, tcr_el1", out(reg) tcr, options(nomem, nostack));
        core::arch::asm!("mrs {}, id_aa64mmfr0_el1", out(reg) mmfr0, options(nomem, nostack));
    }
    // Normal, inner/outer write-back non-transient RW-allocate.
    let attr = (0..8u64).find(|&i| (mair >> (8 * i)) & 0xff == 0xff).ok_or(EINVAL)?;
    if img.footprint.len() > MAX_RANGES { return Err(EINVAL); }

    let mut a = super::stage::Alloc::new(img.footprint.clone());
    let res = (|| -> Result<(usize, usize, usize), isize> {
        Ok((a.block(0, None)?, a.block(0, None)?, a.block(0, None)?))
    })();
    let (purg, l0, l1) = match res {
        Ok(v) => v,
        Err(e) => { a.release_all(); return Err(e); }
    };
    a.release_held();
    // Owned for good: the machine is about to be handed over.
    a.owned.clear();

    // Identity map: 1 GiB blocks for every GiB holding RAM the jump touches
    // (sources, list, FDT, destinations, purgatory and these tables are all
    // in that RAM). Device space stays unmapped.
    let ram = super::info::loadable(map);
    let l1v = mm::phys_to_virt(l1) as *mut u64;
    let mut gibs = 0;
    for g in 0..512u64 {
        let (s, e) = (g << 30, (g + 1) << 30);
        let hit = ram.iter().any(|&(a, b)| a < e && s < b)
            || [purg as u64, l0 as u64, l1 as u64, img.head, img.bootinfo].iter().any(|&p| p >= s && p < e)
            || img.footprint.iter().any(|&(a, b)| a < e && s < b);
        if !hit { continue; }
        // Block, AttrIndx, AP=00 (EL1 RW), SH=inner, AF.
        let d = s | 0b01 | (attr << 2) | (3 << 8) | (1 << 10);
        unsafe { l1v.add(g as usize).write_volatile(d); }
        gibs += 1;
    }
    unsafe { (mm::phys_to_virt(l0) as *mut u64).write_volatile(l1 as u64 | 0b11); }

    // TTBR0 regime: 48-bit, 4 KiB granule, walks on and cacheable; IPS
    // widened to the CPU's PA range so sources above 4 GiB translate.
    let ips = (mmfr0 & 0xf).min(5);
    let mut t = tcr;
    t &= !(0x3f | (1 << 7) | (0x3 << 8) | (0x3 << 10) | (0x3 << 12) | (0x3 << 14) | (0x7 << 32));
    t |= 16 | (1 << 8) | (1 << 10) | (3 << 12) | (ips << 32);

    // Purgatory code, then the range table.
    let code = kexec_purgatory_start as *const () as usize;
    let len = kexec_purgatory_end as *const () as usize - code;
    if len > RANGES_OFF { return Err(ENOMEM); }
    let pv = mm::phys_to_virt(purg);
    // A second copy in the AP park page: the secondaries call CPU_OFF from
    // there, in memory the next kernel keeps (purgatory_aarch64.s, HVF).
    let park = img.bootinfo + super::info::AP_PARK_OFF;
    let cpu_off = boot::mpidr::kexec_cpu_off(cfg!(feature = "rpi5"), crate::boot_cmdline().unwrap_or(""));
    crate::serial_print_str(if cpu_off {
        "[KEXEC] secondaries: PSCI CPU_OFF, then the park loop\n"
    } else {
        "[KEXEC] secondaries: park loop, no PSCI CPU_OFF (next kernel starts them from the mailbox)\n"
    });
    let kv = mm::phys_to_virt(park as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(code as *const u8, pv as *mut u8, len);
        core::ptr::copy_nonoverlapping(code as *const u8, kv as *mut u8, len);
        let r = (pv + RANGES_OFF) as *mut u64;
        for (i, &(s, e)) in img.footprint.iter().enumerate() {
            r.add(2 * i).write_volatile(s);
            r.add(2 * i + 1).write_volatile(e);
        }
        // Make the copied code fetchable and the tables/params visible to
        // the walker and to the purgatory.
        for va in [pv, kv, mm::phys_to_virt(l0), mm::phys_to_virt(l1)] {
            let mut p = va;
            while p < va + 4096 {
                core::arch::asm!("dc civac, {}", in(reg) p, options(nostack));
                p += 64;
            }
        }
        // Zero the diagnostic stage words (park page +STAGE_OFF), then the icache op.
        for i in 0..16usize {
            let w = mm::phys_to_virt((park + STAGE_OFF) as usize + 8 * i) as *mut u64;
            w.write_volatile(0);
            core::arch::asm!("dc civac, {}", in(reg) w as usize, options(nostack));
        }
        // `kexec.icache=is`: inner-shareable I-cache invalidate (Pi diag A/B).
        if crate::boot_cmdline().map_or(false, |c| c.split_ascii_whitespace().any(|t| t == "kexec.icache=is")) {
            crate::serial_print_str("[KEXEC] icache: ic ialluis\n");
            core::arch::asm!("dsb ish", "ic ialluis", "dsb ish", "isb", options(nostack));
        } else {
            core::arch::asm!("dsb ish", "ic iallu", "dsb ish", "isb", options(nostack));
        }
        PLAN = Plan {
            purgatory: purg as u64, ttbr0: l0 as u64, tcr: t,
            head: img.head, entry: img.entry, fdt: img.bootinfo,
            ranges: (purg + RANGES_OFF) as u64, nranges: img.footprint.len() as u64,
            park,
            cpu_off: cpu_off as u64,
        };
    }
    crate::serial_print_str(&alloc::format!(
        "[KEXEC] exec plan: purgatory={:#x} ({} B) park={:#x} idmap L0={:#x} ({} GiB) tcr={:#x} mair idx {}\n",
        purg, len, park, l0, gibs, t, attr));
    Ok(())
}

fn timers_off() {
    unsafe {
        core::arch::asm!("msr cntv_ctl_el0, xzr", "msr cntp_ctl_el0, xzr", "isb", options(nomem, nostack));
    }
}

/// Divert target: every CPU's interrupt after `stop_and_jump` armed it.
fn on_ipi() -> ! {
    unsafe { core::arch::asm!("msr daifset, #0xf", options(nomem, nostack)); }
    stage(1);
    note_clean();
    if unsafe { arch_aarch64::smp::arch_cpu_id() } == 0 { final_cpu0() }
    park_off()
}

fn park_off() -> ! {
    timers_off();
    arch_aarch64::gic::kexec_cpu_off();
    stage(2);
    let p = unsafe { core::ptr::addr_of!(PLAN).read() };
    if p.purgatory == 0 { arch_aarch64::smp::cpu_off() }
    extern "C" { static boot_entered_el2: u64; }
    let smc = cfg!(feature = "rpi5")
        || unsafe { core::ptr::read_volatile(core::ptr::addr_of!(boot_entered_el2)) } != 0;
    let off = kexec_purgatory_off as *const () as u64 - kexec_purgatory_start as *const () as u64;
    let me = unsafe { arch_aarch64::smp::arch_cpu_id() } as u64;
    enter_idmap(p.park + off, [0, smc as u64, (p.cpu_off == 0) as u64, me, 0])
}

/// Switch TTBR0 to the identity map and branch to physical `target` with
/// x0..x4 = `a`. Never returns.
fn enter_idmap(target: u64, a: [u64; 5]) -> ! {
    let p = unsafe { core::ptr::addr_of!(PLAN).read() };
    stage(3);
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, x6",
            "msr tcr_el1, x7",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            // WXN would make the identity map non-executable.
            "mrs x9, sctlr_el1",
            "bic x9, x9, #(1 << 19)",
            "msr sctlr_el1, x9",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            "br x5",
            in("x6") p.ttbr0,
            in("x7") p.tcr,
            in("x5") target,
            in("x0") a[0],
            in("x1") a[1],
            in("x2") a[2],
            in("x3") a[3],
            in("x4") a[4],
            options(noreturn, nostack)
        );
    }
}

/// Set on entry to `final_cpu0`: a spin inside it (logging -> serial) must not
/// re-enter `final_cpu0` via divert -> on_ipi.
static FINAL: AtomicBool = AtomicBool::new(false);

fn park_hook() {
    stage(1);
    if FINAL.load(Ordering::Relaxed) && unsafe { arch_aarch64::smp::arch_cpu_id() } == 0 { return; }
    arch_aarch64::exception::divert()
}

/// `kexec.test_straggler=<cpu>` guard: CPU `<cpu>` is the CPU a stuck lock
/// would hold. Its first IRQ-divert sends it into a masked `uart::getc` loop
/// while the RX lock is held forever (the Pi 5 failure); it must still park.
static STRAGGLER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(usize::MAX);
static STRAGGLER_IN: AtomicBool = AtomicBool::new(false);

fn straggler_entry() -> ! {
    unsafe { core::arch::asm!("msr daifset, #0xf", options(nomem, nostack)); }
    stage(1);
    note_clean();
    let me = unsafe { arch_aarch64::smp::arch_cpu_id() };
    if me == STRAGGLER.load(Ordering::Relaxed) && !STRAGGLER_IN.swap(true, Ordering::AcqRel) {
        log("[KEXEC] test: straggler spinning in uart::getc with IRQs masked\n");
        // From now on a divert is the normal park.
        arch_aarch64::exception::IRQ_DIVERT.store(on_ipi as fn() -> ! as usize, Ordering::Release);
        if straggler_mode("lock") {
            // Two-CPU variant: another CPU forgot LOCK and parks; this CPU spins on
            // it with IRQs masked and must still park via spin's relax() hook.
            log("[KEXEC] test: straggler spinning on a spin::Mutex held by a parked CPU\n");
            while !LOCK_HELD.load(Ordering::Acquire) { core::hint::spin_loop(); }
            let _g = LOCK.lock();
            loop { core::hint::spin_loop(); }
        }
        if straggler_mode("rq") {
            // Like "lock", but the held lock is a run-queue lock (spins with IRQs
            // masked in cpuq::lock): only tlb_service_pending() reaches the park hook.
            log("[KEXEC] test: straggler spinning on a run-queue lock held by a parked CPU\n");
            while !RQ_HELD.load(Ordering::Acquire) { core::hint::spin_loop(); }
            let _g = sched::cpuq::lock(RQ_IDX.load(Ordering::Relaxed));
            loop { core::hint::spin_loop(); }
        }
        if straggler_sleep_mode() {
            // The brcmf radio loop: a spin::Mutex held across msleep(1).
            log("[KEXEC] test: straggler looping msleep(1) holding a spin::Mutex\n");
            static M: spin::Mutex<()> = spin::Mutex::new(());
            let _g = M.lock();
            // Also a KSpinLock-style depth, so sleep_until takes its pure busy-wait branch.
            sched::irqctx::atomic_inc();
            log("[KEXEC] test: msleep context: ");
            log(sched::irqctx::sleep_forbidden_reason().unwrap_or("may sleep"));
            log("\n");
            loop { sched::kthread::msleep(1); }
        }
        loop { let _ = unsafe { arch_aarch64::uart::getc() }; core::hint::spin_loop(); }
    }
    if straggler_mode("lock") && STRAGGLER.load(Ordering::Relaxed) != usize::MAX
        && !LOCK_HELD.swap(true, Ordering::AcqRel)
    {
        core::mem::forget(LOCK.lock());
    }
    if straggler_mode("rq") && STRAGGLER.load(Ordering::Relaxed) != usize::MAX
        && !RQ_TAKEN.swap(true, Ordering::AcqRel)
    {
        let c = me;
        core::mem::forget(sched::cpuq::lock(c));
        RQ_IDX.store(c, Ordering::Relaxed);
        RQ_HELD.store(true, Ordering::Release);
    }
    on_ipi()
}

static RQ_TAKEN: AtomicBool = AtomicBool::new(false);
static RQ_HELD: AtomicBool = AtomicBool::new(false);
static RQ_IDX: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static LOCK: spin::Mutex<()> = spin::Mutex::new(());
static LOCK_HELD: AtomicBool = AtomicBool::new(false);

fn straggler_mode(m: &str) -> bool {
    crate::boot_cmdline().map_or(false, |c| c.split_ascii_whitespace()
        .any(|t| t.strip_prefix("kexec.test_straggler_mode=") == Some(m)))
}

fn straggler_sleep_mode() -> bool { straggler_mode("sleep") }

fn test_straggler() -> Option<usize> {
    crate::boot_cmdline()?.split_ascii_whitespace()
        .filter_map(|t| t.strip_prefix("kexec.test_straggler=")).last()?.parse().ok()
}

/// Interrupts masked, devices quiesced, `prepare` done. Never returns.
pub fn stop_and_jump() -> ! {
    unsafe { core::arch::asm!("msr daifset, #0xf", options(nomem, nostack)); }
    ARMED.store(true, Ordering::Release);
    let strag = test_straggler().filter(|&c| c != 0 && c < arch_aarch64::smp::assigned());
    if let Some(c) = strag {
        STRAGGLER.store(c, Ordering::Release);
        if !straggler_sleep_mode() && !straggler_mode("lock") && !straggler_mode("rq") { arch_aarch64::uart::test_leak_rx_lock(); }
    }
    let divert: fn() -> ! = if strag.is_some() { straggler_entry } else { on_ipi };
    arch_aarch64::exception::IRQ_DIVERT.store(divert as usize, Ordering::Release);
    unsafe { core::arch::asm!("dsb ish", options(nostack)); }
    let me = unsafe { arch_aarch64::smp::arch_cpu_id() };
    let n = arch_aarch64::smp::assigned();
    log("[KEXEC] stopping ");
    dec((n - 1) as u64);
    log(" other CPU(s) from cpu");
    dec(me as u64);
    log("\n");
    for i in 0..n {
        if i != me { arch_aarch64::gic::send_sgi(i, arch_aarch64::gic::SGI_RESCHED); }
    }
    // Contended spins on masked CPUs also park (see `wedge::Spin::tick`).
    sched::wedge::PARK_HOOK.store(park_hook as fn() as usize, Ordering::Release);
    spin::held::set_park_hook(park_hook);
    if me == 0 { final_cpu0() }
    park_off()
}

fn final_cpu0() -> ! {
    FINAL.store(true, Ordering::Release);
    timers_off();
    // rpi5 wedge watchdog (lane piwdt): if armed, give the next kernel the
    // full 15 s to reach its own watchdog init (`rpi_misc::boot` re-arms it,
    // or an older kernel stops it); a next kernel that hangs before that is
    // reset to the card's kernel.img instead of needing a power cycle.
    #[cfg(feature = "rpi5")]
    if drivers::rpi_pm::wdt_running_nolock() {
        drivers::rpi_pm::wdt_kick_nolock(drivers::rpi_misc_logic::wdt::HW_SECS);
    }
    // Wait for every other CPU to be OFF: the next kernel's CPU_ON of a CPU
    // still ON fails with ALREADY_ON.
    let n = arch_aarch64::smp::assigned();
    let p = unsafe { core::ptr::addr_of!(PLAN).read() };
    // Every other CPU first reports that it runs from the park page with its
    // caches flushed: nothing it does after that touches old-kernel memory.
    let mut stuck: Option<usize> = None;
    for i in 1..n {
        let w = mm::phys_to_virt((p.park + super::info::AP_ARRIVED_OFF) as usize + 8 * i) as *const u64;
        let t0 = arch_aarch64::timer::monotonic_ns();
        let mut resent = t0;
        let arrived = || unsafe {
            core::arch::asm!("dc civac, {}", "dsb sy", in(reg) w as usize, options(nostack));
            w.read_volatile() != 0
        };
        let mut ok = arrived();
        while !ok && arch_aarch64::timer::monotonic_ns().wrapping_sub(t0) < 2_000_000_000 {
            core::hint::spin_loop();
            ok = arrived();
            // A SGI that landed in a window where the CPU could not take it
            // (or was lost) is sent again.
            let now = arch_aarch64::timer::monotonic_ns();
            if !ok && now.wrapping_sub(resent) >= 100_000_000 {
                resent = now;
                arch_aarch64::gic::send_sgi(i, arch_aarch64::gic::SGI_RESCHED);
            }
        }
        if !ok {
            log("[KEXEC] cpu");
            dec(i as u64);
            log(" never reached the park page; irq#");
            let (id, cnt, elr) = sched::wedge::trail(i);
            if id == u32::MAX { log("-"); } else { dec(id as u64); }
            log(" n=");
            dec(cnt);
            log(" elr=");
            hex(elr);
            log(" stage=");
            dec(read_stage(p.park, i));
            let (cp, cl) = (CLEAN_PTR[i].load(Ordering::Acquire), CLEAN_LEN[i].load(Ordering::Relaxed));
            if cp != 0 {
                log(" clean=");
                // SAFETY: a &'static str recorded by note_clean.
                log(unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(cp as *const u8, cl)) });
            }
            log(" uart_rx_lock_held=");
            log(if arch_aarch64::uart::rx_lock_held() { "1" } else { "0" });
            log("\n");
            stuck = Some(i);
        }
    }
    if let Some(i) = stuck {
        // Entering the purgatory now would overwrite the kernel under a CPU
        // that is still running it. Reset instead.
        log("[KEXEC] cpu");
        dec(i as u64);
        log(" still in the old kernel; aborting kexec, system reset\n");
        #[cfg(feature = "rpi5")]
        {
            drivers::rpi_pm::wdt_kick_nolock(1);
        }
        #[cfg(not(feature = "rpi5"))]
        unsafe { arch_aarch64::power::system_reset(); }
        loop { unsafe { core::arch::asm!("wfe", options(nomem, nostack)); } }
    }
    log("[KEXEC] park stages:");
    for i in 1..n {
        log(" cpu");
        dec(i as u64);
        log("=");
        dec(read_stage(p.park, i));
    }
    log("\n");
    // Bus-stage hooks (pcie-perst + bridge reset) only now: every other CPU
    // is on the park page, so none can be mid-access to RP1 when the bridge
    // goes into reset. No allocation, no timer, no locks that a parked CPU
    // could hold.
    {
        let mut any = false;
        drivers::dma_shutdown::run_parked(drivers::dma_shutdown::Stage::Bus, |name, quiet| {
            if !any { log("[kexec] bus quiesced:"); any = true; }
            if quiet { log(" "); log(name); } else { log(" FAILED: "); log(name); }
        });
        if any { log("\n"); }
    }
    for i in 1..n {
        if p.cpu_off == 0 { break; }
        let Some(m) = arch_aarch64::smp::mpidr_of(i) else { continue };
        let t0 = arch_aarch64::timer::monotonic_ns();
        let mut st = arch_aarch64::smp::affinity_info(m);
        while st != 1 && arch_aarch64::timer::monotonic_ns().wrapping_sub(t0) < 2_000_000_000 {
            core::hint::spin_loop();
            st = arch_aarch64::smp::affinity_info(m);
        }
        if st != 1 {
            log("[KEXEC] cpu");
            dec(i as u64);
            log(" mpidr=");
            hex(m);
            log(" still not OFF (AFFINITY_INFO ");
            hex(st as u64);
            log("); jumping anyway\n");
        }
    }
    arch_aarch64::gic::kexec_quiesce();
    // One CPU left, interrupts masked: reset the PCI functions.
    super::devreset::execute(log);
    log(if p.cpu_off != 0 { "[KEXEC] cpu0: other CPUs off; purgatory " } else { "[KEXEC] cpu0: other CPUs parked; purgatory " });
    hex(p.purgatory);
    log(" -> entry ");
    hex(p.entry);
    log(" fdt ");
    hex(p.fdt);
    log("\n");
    super::prevlog::seal();
    enter_idmap(p.purgatory, [p.head, p.entry, p.fdt, p.ranges, p.nranges])
}

#[allow(dead_code)]
pub fn armed() -> bool { ARMED.load(Ordering::Acquire) }
