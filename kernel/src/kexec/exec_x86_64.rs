//! `reboot(KEXEC)` on x86_64: put every other CPU into wait-for-SIPI with
//! INIT, then jump through the purgatory (`purgatory_x86_64.s`) on the BSP.
//!
//! `prepare` runs first, in task context, while the kernel is still whole:
//! it allocates the purgatory page (below 4 GiB: the 32-bit tail runs from
//! it) and an identity map (2 MiB pages over every GiB that holds RAM, so
//! sources and indirection pages above 4 GiB are reachable) in pages outside
//! every destination range, copies the purgatory there and fills [`PLAN`].
//! After that nothing on the jump path allocates or takes a lock.
//!
//! `stop_and_jump` (interrupts masked, devices quiesced): INIT sent to the
//! BSP would restart the firmware rather than park it, so the jump must be
//! made by the BSP. A caller on an AP arms [`arch_x86_64::idt::NMI_DIVERT`]
//! and sends the BSP an NMI (it gets through even if the BSP sits with IF=0),
//! then halts. The BSP ([`final_bsp`]) INITs every other started CPU (the
//! state the next kernel's INIT-SIPI-SIPI expects), masks the IOAPIC, LAPIC
//! LVTs and 8259, switches CR3 to the identity map (plus a copy of the
//! current upper half, so the next instruction stays mapped) and jumps.

use core::sync::atomic::{AtomicBool, Ordering};

use super::{Loaded, EINVAL, ENOMEM};

extern "C" {
    fn kexec_purgatory_x86_start();
    fn kexec_purgatory_x86_end();
}

core::arch::global_asm!(include_str!("purgatory_x86_64.s"));

#[derive(Clone, Copy)]
struct Plan {
    purgatory: u64,
    /// Root of the identity map (PML4, or PML5 under LA57); its upper half
    /// is filled from the live CR3 at jump time.
    root: u64,
    head: u64,
    entry: u64,
    mbi: u64,
}

static mut PLAN: Plan = Plan { purgatory: 0, root: 0, head: 0, entry: 0, mbi: 0 };
static TAKEN: AtomicBool = AtomicBool::new(false);

const GIB: u64 = 1 << 30;
const PRESENT_RW: u64 = 0x3;
const PS: u64 = 0x80;

fn log(s: &str) {
    super::prevlog::tee(s.as_bytes());
    for &b in s.as_bytes() { unsafe { arch_x86_64::putc_dump(b); } }
}
fn hex(v: u64) {
    let mut b = [0u8; 18];
    b[0] = b'0';
    b[1] = b'x';
    let mut n = 2;
    let digits = if v == 0 { 1 } else { (64 - v.leading_zeros() as usize).div_ceil(4) };
    for i in (0..digits).rev() {
        let d = ((v >> (i * 4)) & 0xf) as u8;
        b[n] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        n += 1;
    }
    if let Ok(s) = core::str::from_utf8(&b[..n]) { log(s); }
}
fn dec(mut n: u64) {
    let mut b = [0u8; 20];
    let mut i = b.len();
    loop { i -= 1; b[i] = b'0' + (n % 10) as u8; n /= 10; if n == 0 { break; } }
    if let Ok(s) = core::str::from_utf8(&b[i..]) { log(s); }
}

fn cr4() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)); }
    v
}

fn is_bsp() -> bool { (unsafe { arch_x86_64::apic::rdmsr(0x1B) } & (1 << 8)) != 0 }

/// Build the purgatory page and identity map for `img`. Task context.
pub fn prepare(img: &Loaded, map: &[super::info::Raw]) -> Result<(), isize> {
    // The 32-bit tail jumps to `entry` and passes the MBI in EBX.
    if img.entry >= 1 << 32 || img.bootinfo >= 1 << 32 { return Err(EINVAL); }
    let la57 = cr4() & (1 << 12) != 0;
    let code = kexec_purgatory_x86_start as *const () as usize;
    let len = kexec_purgatory_x86_end as *const () as usize - code;
    if len > 4096 { return Err(ENOMEM); }

    let ram = super::info::loadable(map);
    let mut forbidden = img.footprint.clone();
    forbidden.push((0, 0x10_0000));
    let mut a = super::stage::Alloc::new(forbidden);
    let res = (|| -> Result<(usize, usize, usize), isize> {
        let purg = a.block(0, Some(4 << 30))?;
        let pml4 = a.block(0, None)?;
        let root = if la57 { a.block(0, None)? } else { pml4 };
        let pv = mm::phys_to_virt(pml4) as *mut u64;
        // Every GiB the purgatory touches: RAM (sources, list pages,
        // destinations, its own page) and the destinations themselves.
        let top = ram.iter().map(|&(_, e)| e)
            .chain(img.footprint.iter().map(|&(_, e)| e))
            .chain([img.head + 4096, purg as u64 + 4096])
            .max().unwrap_or(0);
        let ngib = top.div_ceil(GIB);
        if ngib > 256 * 512 { return Err(EINVAL); } // identity must stay in the lower half
        let mut gibs = 0u64;
        for g in 0..ngib {
            let (s, e) = (g * GIB, (g + 1) * GIB);
            let hit = ram.iter().any(|&(x, y)| x < e && s < y)
                || img.footprint.iter().any(|&(x, y)| x < e && s < y)
                || [img.head, purg as u64].iter().any(|&p| p >= s && p < e);
            if !hit { continue; }
            let (i4, i3) = ((g / 512) as usize, (g % 512) as usize);
            let e4 = unsafe { pv.add(i4).read_volatile() };
            let pdpt = if e4 & 1 != 0 { (e4 & 0x000f_ffff_ffff_f000) as usize } else {
                let p = a.block(0, None)?;
                unsafe { pv.add(i4).write_volatile(p as u64 | PRESENT_RW); }
                p
            };
            let pd = a.block(0, None)?;
            let pdv = mm::phys_to_virt(pd) as *mut u64;
            for i in 0..512u64 {
                unsafe { pdv.add(i as usize).write_volatile((s + i * (2 << 20)) | PS | PRESENT_RW); }
            }
            unsafe { (mm::phys_to_virt(pdpt) as *mut u64).add(i3).write_volatile(pd as u64 | PRESENT_RW); }
            gibs += 1;
        }
        if la57 {
            unsafe { (mm::phys_to_virt(root) as *mut u64).write_volatile(pml4 as u64 | PRESENT_RW); }
        }
        crate::serial_print_str(&alloc::format!(
            "[KEXEC] exec plan: purgatory={:#x} ({} B) idmap root={:#x} ({} GiB, {}-level)\n",
            purg, len, root, gibs, if la57 { 5 } else { 4 }));
        Ok((purg, root, gibs as usize))
    })();
    let (purg, root, _) = match res {
        Ok(v) => v,
        Err(e) => { a.release_all(); return Err(e); }
    };
    a.release_held();
    // Owned for good: the machine is about to be handed over.
    a.owned.clear();

    unsafe {
        core::ptr::copy_nonoverlapping(code as *const u8, mm::phys_to_virt(purg) as *mut u8, len);
        PLAN = Plan { purgatory: purg as u64, root: root as u64, head: img.head, entry: img.entry, mbi: img.bootinfo };
    }
    Ok(())
}

/// Interrupts masked, devices quiesced, `prepare` done. Never returns.
pub fn stop_and_jump() -> ! {
    unsafe { core::arch::asm!("cli", options(nomem, nostack)); }
    let me = unsafe { arch_x86_64::smp::arch_cpu_id() };
    let n = arch_x86_64::cpus::assigned();
    log("[KEXEC] stopping ");
    dec(n.saturating_sub(1) as u64);
    log(" other CPU(s) from cpu");
    dec(me as u64);
    log(if is_bsp() { " (BSP)\n" } else { " (AP): NMI to the BSP\n" });
    if is_bsp() { final_bsp() }
    arch_x86_64::idt::NMI_DIVERT.store(final_bsp as fn() -> ! as usize, Ordering::Release);
    unsafe { arch_x86_64::smp::arch_send_nmi(0); }
    let t0 = arch_x86_64::timer::monotonic_ns();
    while !TAKEN.load(Ordering::Acquire) {
        if arch_x86_64::timer::monotonic_ns().wrapping_sub(t0) > 2_000_000_000 {
            log("[KEXEC] the BSP did not take the NMI; halting\n");
            break;
        }
        core::hint::spin_loop();
    }
    park()
}

fn park() -> ! {
    loop { unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)); } }
}

fn final_bsp() -> ! {
    unsafe { core::arch::asm!("cli", options(nomem, nostack)); }
    if !is_bsp() { park() }
    TAKEN.store(true, Ordering::Release);
    let me = unsafe { arch_x86_64::smp::arch_cpu_id() };
    let n = arch_x86_64::cpus::assigned();
    let mut sent = 0;
    for i in 0..n {
        if i == me { continue; }
        let id = arch_x86_64::cpus::apic_id_of(i);
        if id == arch_x86_64::cpus::NO_APIC { continue; }
        unsafe {
            arch_x86_64::apic::send_ipi(id,
                arch_x86_64::apic::ICR_INIT | arch_x86_64::apic::ICR_ASSERT | arch_x86_64::apic::ICR_LEVEL_TRIG);
        }
        sent += 1;
    }
    // Let the INITs land before the copy overwrites what the APs ran.
    let t0 = arch_x86_64::timer::monotonic_ns();
    while arch_x86_64::timer::monotonic_ns().wrapping_sub(t0) < 10_000_000 { core::hint::spin_loop(); }
    unsafe {
        arch_x86_64::ioapic::mask_all();
        arch_x86_64::apic::kexec_quiesce();
    }
    // One CPU left, interrupts masked: reset the PCI functions.
    super::devreset::execute(log);
    let p = unsafe { core::ptr::addr_of!(PLAN).read() };
    log("[KEXEC] BSP cpu");
    dec(me as u64);
    log(": INIT sent to ");
    dec(sent);
    log(if arch_x86_64::apic::is_x2apic() { " CPU(s) (x2APIC)" } else { " CPU(s) (xAPIC)" });
    log("; purgatory ");
    hex(p.purgatory);
    log(" -> entry ");
    hex(p.entry);
    log(" mbi ");
    hex(p.mbi);
    log("\n");
    // From an AP this ran on the NMI IST stack (see NMI_DIVERT): report how
    // deep it got, against `gdt::NMI_STACK_SIZE`.
    log("[KEXEC] NMI stack high-water ");
    dec(arch_x86_64::gdt::nmi_stack_high_water(me) as u64);
    log(" of ");
    dec(arch_x86_64::gdt::NMI_STACK_SIZE as u64);
    log(" B\n");
    // Upper half of the live tables into the identity root: the code below
    // runs at its higher-half address until the jump.
    unsafe {
        let cur: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cur, options(nomem, nostack, preserves_flags));
        let src = mm::phys_to_virt((cur & 0x000f_ffff_ffff_f000) as usize) as *const u64;
        let dst = mm::phys_to_virt(p.root as usize) as *mut u64;
        for i in 256..512 { dst.add(i).write_volatile(src.add(i).read_volatile()); }
        super::prevlog::seal();
        core::arch::asm!(
            "mov cr3, {root}",
            "jmp {target}",
            root = in(reg) p.root,
            target = in(reg) p.purgatory,
            in("rdi") p.head,
            in("rsi") p.entry,
            in("rdx") p.mbi,
            options(noreturn, nostack)
        );
    }
}
