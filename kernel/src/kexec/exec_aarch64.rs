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
//! from the purgatory page and calls PSCI CPU_OFF there (`park_off`). The
//! new kernel parks any CPU whose MPIDR.Aff0 != 0 (entry_aarch64.s), so the
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
}

static mut PLAN: Plan = Plan { purgatory: 0, ttbr0: 0, tcr: 0, head: 0, entry: 0, fdt: 0, ranges: 0, nranges: 0 };
static ARMED: AtomicBool = AtomicBool::new(false);

fn log(s: &str) { arch_aarch64::uart::serial_print_str(s); }
fn hex(v: u64) { arch_aarch64::uart::print_hex(v as usize); }
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
    unsafe {
        core::ptr::copy_nonoverlapping(code as *const u8, pv as *mut u8, len);
        let r = (pv + RANGES_OFF) as *mut u64;
        for (i, &(s, e)) in img.footprint.iter().enumerate() {
            r.add(2 * i).write_volatile(s);
            r.add(2 * i + 1).write_volatile(e);
        }
        // Make the copied code fetchable and the tables/params visible to
        // the walker and to the purgatory.
        for va in [pv, mm::phys_to_virt(l0), mm::phys_to_virt(l1)] {
            let mut p = va;
            while p < va + 4096 {
                core::arch::asm!("dc civac, {}", in(reg) p, options(nostack));
                p += 64;
            }
        }
        core::arch::asm!("dsb ish", "ic iallu", "dsb ish", "isb", options(nostack));
        PLAN = Plan {
            purgatory: purg as u64, ttbr0: l0 as u64, tcr: t,
            head: img.head, entry: img.entry, fdt: img.bootinfo,
            ranges: (purg + RANGES_OFF) as u64, nranges: img.footprint.len() as u64,
        };
    }
    crate::serial_print_str(&alloc::format!(
        "[KEXEC] exec plan: purgatory={:#x} ({} B) idmap L0={:#x} ({} GiB) tcr={:#x} mair idx {}\n",
        purg, len, l0, gibs, t, attr));
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
    if unsafe { arch_aarch64::smp::arch_cpu_id() } == 0 { final_cpu0() }
    park_off()
}

fn park_off() -> ! {
    timers_off();
    arch_aarch64::gic::kexec_cpu_off();
    let p = unsafe { core::ptr::addr_of!(PLAN).read() };
    if p.purgatory == 0 { arch_aarch64::smp::cpu_off() }
    extern "C" { static boot_entered_el2: u64; }
    let smc = cfg!(feature = "rpi5")
        || unsafe { core::ptr::read_volatile(core::ptr::addr_of!(boot_entered_el2)) } != 0;
    let off = kexec_purgatory_off as *const () as u64 - kexec_purgatory_start as *const () as u64;
    enter_idmap(p.purgatory + off, [0, smc as u64, 0, 0, 0])
}

/// Switch TTBR0 to the identity map and branch to physical `target` with
/// x0..x4 = `a`. Never returns.
fn enter_idmap(target: u64, a: [u64; 5]) -> ! {
    let p = unsafe { core::ptr::addr_of!(PLAN).read() };
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

/// Interrupts masked, devices quiesced, `prepare` done. Never returns.
pub fn stop_and_jump() -> ! {
    unsafe { core::arch::asm!("msr daifset, #0xf", options(nomem, nostack)); }
    ARMED.store(true, Ordering::Release);
    arch_aarch64::exception::IRQ_DIVERT.store(on_ipi as fn() -> ! as usize, Ordering::Release);
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
    if me == 0 { final_cpu0() }
    park_off()
}

fn final_cpu0() -> ! {
    timers_off();
    // Wait for every other CPU to be OFF: the next kernel's CPU_ON of a CPU
    // still ON fails with ALREADY_ON.
    let n = arch_aarch64::smp::assigned();
    for i in 1..n {
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
    let p = unsafe { core::ptr::addr_of!(PLAN).read() };
    log("[KEXEC] cpu0: other CPUs off; purgatory ");
    hex(p.purgatory);
    log(" -> entry ");
    hex(p.entry);
    log(" fdt ");
    hex(p.fdt);
    log("\n");
    enter_idmap(p.purgatory, [p.head, p.entry, p.fdt, p.ranges, p.nranges])
}

#[allow(dead_code)]
pub fn armed() -> bool { ARMED.load(Ordering::Acquire) }
