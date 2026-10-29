//! reboot(2) and the kernel's shutdown sequence (Linux kernel/reboot.c).
//!
//! `sys_reboot` checks CAP_SYS_BOOT (here: euid 0, which holds every
//! capability) and the magic numbers exactly as Linux does, then runs one of
//! `kernel_power_off`, `kernel_restart` or `kernel_halt`. Each mirrors Linux's
//! ordering: mark the system state (so nothing new starts), run the shutdown
//! notifiers, shut the devices down, and only then hand the machine to the
//! arch code — ACPI S5 / RESET_REG on x86_64 (arch_x86_64::power), PSCI
//! SYSTEM_OFF / SYSTEM_RESET on aarch64 (arch_aarch64::power).
//!
//! "Shut the devices down" is, for this kernel, the block layer: every f2fs
//! volume gets its block cache flushed, a checkpoint marked as a clean
//! unmount (CP_UMOUNT_FLAG) and a virtio-blk FLUSH, and is made read-only.
//! Linux's reboot(2) does not sync on its own (reboot(8) calls sync() first,
//! and the block drivers flush their write caches in `device_shutdown`); here
//! the write-back cache lives in the in-kernel f2fs server, so its flush is
//! this kernel's `device_shutdown`, and doing it unconditionally means even
//! `reboot -f` leaves a consistent volume. Init's orderly path (userland/init)
//! has already remounted / read-only by then, which makes this a no-op.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use crate::serial_print_str;

// include/uapi/linux/reboot.h
const LINUX_REBOOT_MAGIC1:  u32 = 0xfee1_dead;
const LINUX_REBOOT_MAGIC2:  u32 = 672_274_793; // 0x28121969
const LINUX_REBOOT_MAGIC2A: u32 = 85_072_278;  // 0x05121996
const LINUX_REBOOT_MAGIC2B: u32 = 369_367_448; // 0x16041998
const LINUX_REBOOT_MAGIC2C: u32 = 537_993_216; // 0x20112000

const LINUX_REBOOT_CMD_RESTART:    u32 = 0x0123_4567;
const LINUX_REBOOT_CMD_HALT:       u32 = 0xCDEF_0123;
const LINUX_REBOOT_CMD_CAD_ON:     u32 = 0x89AB_CDEF;
const LINUX_REBOOT_CMD_CAD_OFF:    u32 = 0x0000_0000;
const LINUX_REBOOT_CMD_POWER_OFF:  u32 = 0x4321_FEDC;
const LINUX_REBOOT_CMD_RESTART2:   u32 = 0xA1B2_C3D4;
const LINUX_REBOOT_CMD_KEXEC:      u32 = 0x4558_4543;
/// LeandrOS private: run one freeze-only device suspend/resume cycle
/// (`pm::sys_pm_test`, arg = user `PmTestReq`). "PMTS".
const LEANDROS_REBOOT_CMD_PM_TEST: u32 = 0x504D_5453;

const EPERM:  isize = -1;
const EINVAL: isize = -22;

/// Ctrl-Alt-Del policy (`C_A_D`): true = the kernel reboots at once, false =
/// init gets SIGINT. Linux boots with it enabled; init normally turns it off.
/// Nothing generates the key event yet, so this is state only.
static CAD_ENABLED: AtomicBool = AtomicBool::new(true);

/// `system_state` for the shutdown paths: 0 running, else one of below.
const SYSTEM_RUNNING:   u8 = 0;
const SYSTEM_HALT:      u8 = 1;
const SYSTEM_POWER_OFF: u8 = 2;
const SYSTEM_RESTART:   u8 = 3;
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
const SYSTEM_KEXEC:     u8 = 4;
static SYSTEM_STATE: AtomicU8 = AtomicU8::new(SYSTEM_RUNNING);

/// True once a halt/power-off/restart has begun.
#[allow(dead_code)]
pub fn shutting_down() -> bool {
    SYSTEM_STATE.load(Ordering::Acquire) != SYSTEM_RUNNING
}

/// Whether Ctrl-Alt-Del reboots directly (reboot(2) CAD_ON/CAD_OFF).
#[allow(dead_code)]
pub fn cad_enabled() -> bool {
    CAD_ENABLED.load(Ordering::Relaxed)
}

#[cfg(target_arch = "x86_64")]
static mut ACPI_POWER: Option<boot::acpi::AcpiPower> = None;

/// Read the FADT/DSDT power registers once at boot, while the tables are
/// known to be intact (x86_64; aarch64 uses PSCI and needs nothing).
#[cfg(target_arch = "x86_64")]
pub fn init_acpi(rsdp_phys: u64, hhdm_offset: u64) {
    let info = unsafe { boot::acpi::find_power_info(rsdp_phys, hhdm_offset) };
    match info {
        Some(p) => {
            serial_print_str("[ACPI] PM1a_CNT=");
            crate::serial_print_hex(p.pm1a_cnt as usize);
            serial_print_str(" PM1b_CNT=");
            crate::serial_print_hex(p.pm1b_cnt as usize);
            serial_print_str(if p.s5_found { " \\_S5_ SLP_TYP=" } else { " no \\_S5_, SLP_TYP=" });
            crate::serial_print_hex(p.slp_typ_a as usize);
            serial_print_str("/");
            crate::serial_print_hex(p.slp_typ_b as usize);
            serial_print_str(" RESET_REG=");
            crate::serial_print_hex(p.reset_port as usize);
            serial_print_str(" value ");
            crate::serial_print_hex(p.reset_value as usize);
            serial_print_str("\n");
            serial_print_str(&alloc::format!(
                "[ACPI] sleep: LOW_POWER_S0={} \\_S3_={} SLP_TYP={:#x}/{:#x} FACS={:#x} _PTS={} _WAK={}\n",
                p.low_power_s0 as u8, p.s3_found as u8, p.s3_typ_a, p.s3_typ_b, p.facs, p.has_pts as u8, p.has_wak as u8));
            unsafe { ACPI_POWER = Some(p); }
        }
        None => serial_print_str("[ACPI] no FADT: power-off falls back to halt\n"),
    }
}

/// The FADT's verdict on a PS/2 (i8042) controller: `Some(present)` from
#[cfg(target_arch = "x86_64")]
pub fn acpi_power() -> Option<&'static boot::acpi::AcpiPower> {
    unsafe { (*core::ptr::addr_of!(ACPI_POWER)).as_ref() }
}

/// IAPC_BOOT_ARCH bit 1 on a revision >= 2 FADT, `None` when there is no
/// FADT or it predates the field (Linux's rule, `acpi_boot_init`).
#[cfg(target_arch = "x86_64")]
pub fn acpi_has_8042() -> Option<bool> {
    let p = unsafe { (*core::ptr::addr_of!(ACPI_POWER)).as_ref()? };
    if p.fadt_revision < 2 { return None; }
    Some(p.iapc_boot_arch & 0x2 != 0)
}

/// reboot(magic1, magic2, cmd, arg) — Linux `SYSCALL_DEFINE4(reboot, ...)`.
pub fn sys_reboot(magic1: usize, magic2: usize, cmd: usize, _arg: usize) -> isize {
    // CAP_SYS_BOOT first, as Linux does: an unprivileged caller learns
    // nothing about the magic numbers.
    if sched::current_euid() != 0 { return EPERM; }
    // The magics and the command are C `int`s; compare the low 32 bits so a
    // sign-extending caller matches too.
    let (m1, m2, cmd) = (magic1 as u32, magic2 as u32, cmd as u32);
    if m1 != LINUX_REBOOT_MAGIC1
        || (m2 != LINUX_REBOOT_MAGIC2 && m2 != LINUX_REBOOT_MAGIC2A
            && m2 != LINUX_REBOOT_MAGIC2B && m2 != LINUX_REBOOT_MAGIC2C) {
        return EINVAL;
    }
    match cmd {
        LEANDROS_REBOOT_CMD_PM_TEST => crate::pm::sys_pm_test(_arg),
        LINUX_REBOOT_CMD_RESTART | LINUX_REBOOT_CMD_RESTART2 => kernel_restart(),
        LINUX_REBOOT_CMD_CAD_ON  => { CAD_ENABLED.store(true, Ordering::Relaxed); 0 }
        LINUX_REBOOT_CMD_CAD_OFF => { CAD_ENABLED.store(false, Ordering::Relaxed); 0 }
        LINUX_REBOOT_CMD_HALT    => kernel_halt(),
        LINUX_REBOOT_CMD_POWER_OFF => kernel_power_off(),
        LINUX_REBOOT_CMD_KEXEC   => kernel_kexec(),
        // SW_SUSPEND and anything else: not configured, as on a Linux
        // built without CONFIG_KEXEC_CORE / CONFIG_HIBERNATION.
        _ => EINVAL,
    }
}

/// `kernel_shutdown_prepare` + `device_shutdown`. Exactly one caller gets
/// past the state transition; a concurrent second reboot(2) parks here (the
/// first one is about to take the machine down, as Linux's
/// `system_transition_mutex` would make it wait).
fn shutdown_prepare(state: u8) {
    if SYSTEM_STATE.compare_exchange(SYSTEM_RUNNING, state, Ordering::AcqRel, Ordering::Acquire).is_err() {
        loop { core::hint::spin_loop(); }
    }
    // Shutdown notifiers: none are registered in this kernel.
    // Device shutdown: the block layer (see the module comment).
    crate::syscall::sync_all();
    let n = f2fs_server::shutdown_all();
    serial_print_str("[POWER] filesystems synced; ");
    crate::print_number(n as u32);
    serial_print_str(" f2fs volume(s) committed with a clean-unmount checkpoint (the rest were already read-only)\n");
    // NVMe: delete the I/O queues and send CC.SHN (Linux `nvme_shutdown`).
    drivers::nvme::shutdown_all();
}

// ── Reboot / power-off knobs (kernel command line) ───────────────────────────
//
// `reboot.quiesce=0`     skip the device quiesce below (the pre-lane-rebootq
//                        behaviour: filesystems + NVMe only).
// `reboot.type=cold|warm` x86_64 reset flavour (default warm; cold is an
//                        alias for reboot.method=cf9full): cold = PCI
//                        reset control 0xCF9 FULL_RST (0x0E: the chipset
//                        drops SLP_S3#/S4#/S5# for 3-5 s, i.e. a power
//                        cycle, what EDK2's ResetSystem(EfiResetCold) does on
//                        Intel PCHs); warm = Linux's default chain on an ACPI
//                        machine (FADT RESET_REG — 0xCF9 <- 0x06 on the HP —
//                        then the 8042, then 0xCF9 0x06, then triple fault).
// `reboot.method=auto|acpi|kbd|cf9|cf9full|triple` try this one first, then the
//                        rest of the type's chain (default auto).
// `reboot.hda_crst=1`    also hold the HDA link in reset after the codec is
//                        in D3 (Linux skips it at shutdown: pops).
// `reboot.xhci_hcrst=0`  halt the xHCI but do not reset it.
// `reboot.usb_vbus=off`  switch root-port power off after the xHCI reset
//                        (only on controllers with port power control).
// `reboot.busmaster=0`   leave PCI bus mastering on.
// `reboot.efi`           not implemented: no EFI runtime services in this
//                        kernel (Limine does not call SetVirtualAddressMap and
//                        nothing maps the runtime regions); 0xCF9 0x0E is
//                        what the Intel ResetSystemLib does for EfiResetCold.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResetMethod { Auto, Acpi, Cf9, Cf9Full, Kbd, Triple }

#[derive(Clone, Copy)]
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
struct RebootCfg {
    quiesce: bool,
    cold: bool,
    method: ResetMethod,
    hda_crst: bool,
    xhci_hcrst: bool,
    usb_vbus_off: bool,
    busmaster_off: bool,
}

fn opt(key: &str) -> Option<&'static str> {
    crate::boot_cmdline()?.split_ascii_whitespace()
        .filter_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .last()
}

impl RebootCfg {
    fn from_cmdline() -> Self {
        let off = |k: &str| matches!(opt(k), Some("0") | Some("off") | Some("no"));
        let on = |k: &str| matches!(opt(k), Some("1") | Some("on") | Some("yes"));
        RebootCfg {
            quiesce: !off("reboot.quiesce"),
            cold: opt("reboot.type") == Some("cold"),
            method: match opt("reboot.method") {
                Some("acpi") => ResetMethod::Acpi,
                Some("cf9") => ResetMethod::Cf9,
                Some("cf9full") => ResetMethod::Cf9Full,
                Some("kbd") => ResetMethod::Kbd,
                Some("triple") => ResetMethod::Triple,
                // reboot.type=cold is an alias for method=cf9full.
                _ if opt("reboot.type") == Some("cold") => ResetMethod::Cf9Full,
                _ => ResetMethod::Auto,
            },
            hda_crst: on("reboot.hda_crst"),
            xhci_hcrst: !off("reboot.xhci_hcrst"),
            usb_vbus_off: off("reboot.usb_vbus"),
            busmaster_off: !off("reboot.busmaster"),
        }
    }
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    fn method_name(&self) -> &'static str {
        match self.method { ResetMethod::Auto => "auto", ResetMethod::Acpi => "acpi",
                            ResetMethod::Cf9 => "cf9", ResetMethod::Cf9Full => "cf9full", ResetMethod::Kbd => "kbd", ResetMethod::Triple => "triple" }
    }
}

fn plog(s: &str) {
    serial_print_str("[REBOOT] ");
    serial_print_str(s);
    serial_print_str("\n");
}

/// Phase 1 of the device quiesce (Linux `device_shutdown`), interrupts on,
/// before the kernel log's final flush so every line below reaches the
/// stick: the sound device (HDA codec silenced and in D3, DMA stopped) and
/// the wired NIC. Returns the PCI functions whose bus mastering phase 2
/// clears.
fn quiesce_phase1(cfg: &RebootCfg, what: &str) -> alloc::vec::Vec<drivers::pci::PciFunction> {
    let mut bm = alloc::vec::Vec::new();
    #[cfg(target_arch = "x86_64")]
    let plan = alloc::format!("{}: quiesce={} type={} method={} hda_crst={} xhci_hcrst={} usb_vbus={} busmaster_off={}",
        what, cfg.quiesce as u8, if cfg.cold { "cold" } else { "warm" }, cfg.method_name(),
        cfg.hda_crst as u8, cfg.xhci_hcrst as u8, if cfg.usb_vbus_off { "off" } else { "on" }, cfg.busmaster_off as u8);
    #[cfg(target_arch = "aarch64")]
    let plan = alloc::format!("{}: quiesce={} (PSCI) hda_crst={} xhci_hcrst={} usb_vbus={} busmaster_off={}",
        what, cfg.quiesce as u8, cfg.hda_crst as u8, cfg.xhci_hcrst as u8,
        if cfg.usb_vbus_off { "off" } else { "on" }, cfg.busmaster_off as u8);
    plog(&plan);
    if !cfg.quiesce { return bm; }
    let t = sched::monotonic_ns();
    let (audio, f) = pipewire_server::shutdown(cfg.hda_crst);
    plog(&alloc::format!("audio: {}", audio));
    if let Some(f) = f { bm.push(f); }
    if let Some(s) = crate::kmod::exports::pm_shutdown().or_else(drivers::e1000e::shutdown) { plog(&s); }
    // Pi 5 Wi-Fi: leave the AP, WLC_DOWN, chip off with WL_REG_ON held low
    // across a kexec (the CYW43455 otherwise keeps running into the next kernel).
    #[cfg(all(target_arch = "aarch64", feature = "rpi5", feature = "drv-brcmf"))]
    if let Some(s) = drivers::brcmf_boot::shutdown() { plog(&s); }
    if cfg.busmaster_off {
        // Network (02xx: e1000e, iwlwifi, virtio-net, r8169) and display
        // (03xx: i915, NVIDIA, virtio-gpu) functions; xHCI is done by its
        // own quiesce. NVMe was shut down above (CC.SHN) and keeps it.
        for d in drivers::pci::scan().iter() {
            if d.class == 0x02 || d.class == 0x03 {
                bm.push(drivers::pci::PciFunction::new(d.bus, d.dev, d.func));
            }
        }
    }
    plog(&alloc::format!("phase 1 done in {} us; next: klog final flush, disk cache flush, then (serial only) xHCI halt{}, bus master off on {} function(s), reset",
        sched::monotonic_ns().saturating_sub(t) / 1000, if cfg.xhci_hcrst { " + HCRST" } else { "" }, bm.len()));
    bm
}

/// Phase 2, interrupts off, after the last write to any disk: xHCI halt
/// (+ reset, + port power off), then bus mastering off. Serial only — the
/// stick is gone.
fn quiesce_phase2(cfg: &RebootCfg, bm: &[drivers::pci::PciFunction]) -> usize {
    if !cfg.quiesce { return 0; }
    let mut halted = 0;
    for line in drivers::usb_hcd::quiesce_all(cfg.xhci_hcrst, cfg.usb_vbus_off) {
        if line.contains(" halted in ") { halted += 1; }
        serial_print_str("[REBOOT] ");
        serial_print_str(&line);
        serial_print_str("\n");
    }
    if cfg.busmaster_off {
        // kexec: the Intel IGD scanning out the handed-over framebuffer
        // keeps it (kexec::devreset); never the case on restart/power-off.
        for f in bm {
            if !crate::kexec::devreset::keeps_bm(f.bus, f.dev, f.func) { f.set_bus_master(false); }
        }
        serial_print_str("[REBOOT] bus mastering off on the audio, network and display functions\n");
    }
    halted
}

/// Last step of every path: no more interrupts on this CPU, every queued
/// console line written out, the kernel log's final flush and the disks'
/// write caches committed after it.
fn machine_prepare() {
    sched::mark_system_down();
    crate::console_drain_outbox();
    crate::console_staging_disable_and_drain();
    crate::klog::flush_final();
    let (ok, bad) = drivers::blkdev::flush_all_disks();
    if ok + bad > 0 {
        serial_print_str(&alloc::format!("[REBOOT] disk write caches flushed: {} ok, {} failed\n", ok, bad));
    }
    unsafe {
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("cli", options(nomem, nostack));
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("msr daifset, #0xf", options(nomem, nostack));
    }
}

/// `kernel_power_off`. Falls back to `kernel_halt`'s end state when the
/// platform cannot power off (no FADT / no PSCI), as Linux does without a
/// `pm_power_off` handler.
fn kernel_power_off() -> ! {
    shutdown_prepare(SYSTEM_POWER_OFF);
    let cfg = RebootCfg::from_cmdline();
    let bm = quiesce_phase1(&cfg, "power-off");
    serial_print_str("reboot: Power down\n");
    machine_prepare();
    quiesce_phase2(&cfg, &bm);
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if let Some(p) = (*core::ptr::addr_of!(ACPI_POWER)).as_ref() {
            arch_x86_64::power::acpi_power_off(p);
        }
        serial_print_str("reboot: ACPI power-off failed; System halted\n");
        arch_x86_64::power::halt_forever();
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        arch_aarch64::power::system_off();
        #[cfg(feature = "rpi5")]
        if drivers::rpi_pm::power_off() {
            serial_print_str("reboot: PSCI SYSTEM_OFF refused; PM halt partition + reset\n");
            for _ in 0..100_000_000u64 { core::hint::spin_loop(); }
        }
        serial_print_str("reboot: PSCI SYSTEM_OFF failed; System halted\n");
        arch_aarch64::power::halt_forever();
    }
}

/// Restart from kernel context (netwatch): the reboot(2) path, no caller checks.
pub fn restart_now() -> ! { kernel_restart() }

/// `kernel_restart`.
fn kernel_restart() -> ! {
    shutdown_prepare(SYSTEM_RESTART);
    let cfg = RebootCfg::from_cmdline();
    let bm = quiesce_phase1(&cfg, "restart");
    serial_print_str("reboot: Restarting system\n");
    #[cfg(target_arch = "x86_64")]
    plog(&alloc::format!("chain: {}", arch_x86_64::power::chain_text(match cfg.method {
        ResetMethod::Auto => arch_x86_64::power::Method::Auto,
        ResetMethod::Acpi => arch_x86_64::power::Method::Acpi,
        ResetMethod::Cf9 => arch_x86_64::power::Method::Cf9,
        ResetMethod::Cf9Full => arch_x86_64::power::Method::Cf9Full,
        ResetMethod::Kbd => arch_x86_64::power::Method::Kbd,
        ResetMethod::Triple => arch_x86_64::power::Method::Triple,
    })));
    machine_prepare();
    quiesce_phase2(&cfg, &bm);
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let m = match cfg.method {
            ResetMethod::Auto => arch_x86_64::power::Method::Auto,
            ResetMethod::Acpi => arch_x86_64::power::Method::Acpi,
            ResetMethod::Cf9 => arch_x86_64::power::Method::Cf9,
            ResetMethod::Cf9Full => arch_x86_64::power::Method::Cf9Full,
            ResetMethod::Kbd => arch_x86_64::power::Method::Kbd,
            ResetMethod::Triple => arch_x86_64::power::Method::Triple,
        };
        arch_x86_64::power::restart((*core::ptr::addr_of!(ACPI_POWER)).as_ref(), m);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        arch_aarch64::power::system_reset();
        #[cfg(feature = "rpi5")]
        if drivers::rpi_pm::restart() {
            serial_print_str("reboot: PSCI SYSTEM_RESET refused; PM watchdog reset\n");
            for _ in 0..100_000_000u64 { core::hint::spin_loop(); }
        }
        serial_print_str("reboot: PSCI SYSTEM_RESET failed; System halted\n");
        arch_aarch64::power::halt_forever();
    }
}

/// `kernel_kexec`: boot the image staged by `kexec_file_load`. EINVAL when
/// none is loaded. Same shutdown as a restart (filesystems, NVMe CC.SHN,
/// audio, e1000e, xHCI halt + reset, but no USB VBUS off), with the HDA link
/// held in reset (CRST), then the kexec-only driver hooks (`kexec_hooks`)
/// and every PCI function stopped: virtio devices reset, bus mastering and
/// MSI/MSI-X off. The arch code then stops the other CPUs and, on the last
/// one, resets the PCI functions (kexec::devreset) and jumps.
fn kernel_kexec() -> isize {
    if let Err(e) = crate::kexec::exec_prepare() { return e; }
    shutdown_prepare(SYSTEM_KEXEC);
    nvidia_kexec_unload();
    let mut cfg = RebootCfg::from_cmdline();
    cfg.usb_vbus_off = false;
    cfg.hda_crst = !matches!(opt("reboot.hda_crst"), Some("0") | Some("off") | Some("no"));
    let bm = quiesce_phase1(&cfg, "kexec");
    // Reset plan while interrupts are on (ECAM pages get mapped); it also
    // decides which function keeps bus mastering (see quiesce_phase2).
    crate::kexec::devreset::plan(crate::kexec::handover_fb());
    serial_print_str("kexec_core: Starting new kernel\n");
    machine_prepare();
    let xhci = quiesce_phase2(&cfg, &bm);
    if cfg.quiesce { kexec_hooks(); }
    // Every registered DMA engine, even with reboot.quiesce=0: a bus master
    // left running writes into pages the next kernel calls free (Pi 5 RP1
    // GEM, 2026-10-27). Then config-space bus mastering off everywhere, and
    // last the bus resets (PCIe PERST# + bridge), after which the RC is gone.
    let (mut ok, mut bad) = drivers::dma_shutdown::run(drivers::dma_shutdown::Stage::Device);
    quiesce_all_pci();
    // aarch64: the Bus stage runs later, from `final_cpu0` once every other
    // CPU is parked (a bridge reset under live RP1 driver threads stalls them
    // on AXI). Other architectures have no parked-CPU point, run it here.
    #[cfg(not(target_arch = "aarch64"))]
    {
        let (bus_ok, bus_bad) = drivers::dma_shutdown::run(drivers::dma_shutdown::Stage::Bus);
        ok.extend(bus_ok);
        bad.extend(bus_bad);
    }
    serial_print_str(&kexec_quiesced_line(&ok, xhci, &bad));
    crate::kexec::exec_jump()
}

/// `[kexec] quiesced: gem xhci0 xhci1 dma40 pcie-perst` (+ `FAILED: ...`):
/// one line naming every bus master stopped before the jump.
fn kexec_quiesced_line(ok: &[&str], xhci: usize, bad: &[&str]) -> alloc::string::String {
    use core::fmt::Write as _;
    let mut l = alloc::string::String::from("[kexec] quiesced:");
    let (dev, bus): (alloc::vec::Vec<&str>, alloc::vec::Vec<&str>) =
        ok.iter().partition(|n| **n != "pcie-perst");
    let mut it = dev.iter();
    if let Some(first) = it.next() { let _ = write!(l, " {}", first); }
    for i in 0..xhci { let _ = write!(l, " xhci{}", i); }
    for n in it { let _ = write!(l, " {}", n); }
    for n in bus { let _ = write!(l, " {}", n); }
    if ok.is_empty() && xhci == 0 { l.push_str(" (no bus masters registered)"); }
    if !bad.is_empty() {
        l.push_str(" FAILED:");
        for n in bad { let _ = write!(l, " {}", n); }
    }
    l.push('\n');
    l
}

/// kexec only, right after the filesystems are synced and before phase 1
/// clears bus mastering on display functions: GSP-RM must be unloaded while
/// interrupts, the scheduler and DMA still work (its RPC queues live in
/// system memory). Bounded by `nvidia.kexec_timeout_ms`; a failure makes
/// devreset reset the GPU instead (drivers/nvidia/src/kexec.rs).
fn nvidia_kexec_unload() {
    #[cfg(all(feature = "nvidia", target_arch = "x86_64"))]
    {
        let r = nvidia_rm::kexec::kexec_unload();
        for l in &r.lines {
            serial_print_str("[KEXEC] ");
            serial_print_str(l);
            serial_print_str("\n");
        }
        if let Some((b, d, f)) = r.force_reset {
            crate::kexec::devreset::force_reset(b, d, f);
        }
    }
}

/// kexec only, interrupts off, after the last disk write: the drivers whose
/// hardware a generic reset may not reach (no FLR on an integrated function,
/// or the framebuffer's GPU that is not reset at all). The scanout goes back
/// to the boot loader's surface first, so the next kernel's console shows.
fn kexec_hooks() {
    drivers::gpu_kms::console_restore();
    let mut lines: alloc::vec::Vec<alloc::string::String> = drivers::nvme::kexec_disable();
    lines.extend(drivers::i915::kexec_quiesce());
    lines.extend(drivers::r8169::shutdown());
    lines.extend(drivers::iwlwifi::kexec_stop());
    for l in lines {
        serial_print_str("[KEXEC] ");
        serial_print_str(&l);
        serial_print_str("\n");
    }
}

/// kexec only: stop DMA and interrupts from every PCI function, whatever
/// its driver did. Virtio functions get a device reset (status 0 through
/// their common configuration), then every endpoint loses bus mastering and
/// MSI/MSI-X enable (bus mastering stays on the one function
/// `kexec::devreset::keeps_bm` names). Bridges (class 06) keep forwarding.
/// The device resets proper run later, on the last CPU (kexec::devreset).
fn quiesce_all_pci() {
    use drivers::pci::{cap, PciFunction};
    let (mut virtio, mut fns) = (0, 0);
    for d in drivers::pci::scan().iter() {
        if d.class == 0x06 { continue; }
        let f = PciFunction::new(d.bus, d.dev, d.func);
        if d.vendor_id == 0x1AF4 {
            // VIRTIO_PCI_CAP_COMMON_CFG (cfg_type 1): bar at +4, offset at +8.
            for (id, off) in f.capabilities() {
                if id != cap::VENDOR || f.read8(off + 3) != 1 { continue; }
                let bar = f.read8(off + 4) as usize;
                let o = f.read32(off + 8) as u64;
                let Some(b) = f.bar(bar) else { break };
                if b.io || b.base == 0 { break; }
                f.enable_memory();
                if let Some(va) = mm::kva::ioremap(b.base + o, 0x40, mm::kva::CacheMode::Uncached) {
                    unsafe {
                        core::ptr::write_volatile((va + 0x14) as *mut u8, 0);
                        for _ in 0..100_000 {
                            if core::ptr::read_volatile((va + 0x14) as *const u8) == 0 { break; }
                            core::hint::spin_loop();
                        }
                    }
                    virtio += 1;
                }
                break;
            }
        }
        if let Some(m) = f.find_capability(cap::MSI) {
            let c = f.read16(m + 2);
            f.write16(m + 2, c & !1);
        }
        if let Some(m) = f.find_capability(cap::MSIX) {
            let c = f.read16(m + 2);
            f.write16(m + 2, c & !(1 << 15));
        }
        if !crate::kexec::devreset::keeps_bm(d.bus, d.dev, d.func) { f.set_bus_master(false); }
        fns += 1;
    }
    serial_print_str(&alloc::format!(
        "[KEXEC] PCI quiesced: {} virtio device(s) reset, bus master + MSI/MSI-X off on {} function(s)\n",
        virtio, fns));
}

/// `kernel_halt`: everything stopped and synced, the machine left on. The
/// sound device is silenced (phase 1's audio step only); the rest stays up.
fn kernel_halt() -> ! {
    shutdown_prepare(SYSTEM_HALT);
    if RebootCfg::from_cmdline().quiesce {
        let (audio, _) = pipewire_server::shutdown(false);
        plog(&alloc::format!("audio: {}", audio));
    }
    serial_print_str("reboot: System halted\n");
    machine_prepare();
    #[cfg(target_arch = "x86_64")]
    arch_x86_64::power::halt_forever();
    #[cfg(target_arch = "aarch64")]
    arch_aarch64::power::halt_forever();
}

/// `[PMC] reset cause`: why did the last reset happen (x86_64, Intel PCH PMC
/// at 00:1f.2). Log only, nothing is written. Register offsets and bit
/// positions are from memory of the 100/200-series PCH datasheet and coreboot
/// `soc/intel/skylake/include/soc/pmc.h`: UNVERIFIED, raw values are logged
/// next to the decode. Needs mm::kva (ETR3 mapping).
#[cfg(target_arch = "x86_64")]
pub fn log_reset_cause() {
    unsafe fn outl(p: u16, v: u32) { core::arch::asm!("out dx, eax", in("dx") p, in("eax") v, options(nomem, nostack)); }
    unsafe fn inl(p: u16) -> u32 { let v: u32; core::arch::asm!("in eax, dx", in("dx") p, out("eax") v, options(nomem, nostack)); v }
    unsafe fn inw(p: u16) -> u16 { let v: u16; core::arch::asm!("in ax, dx", in("dx") p, out("ax") v, options(nomem, nostack)); v }
    let rd = |reg: u32| -> u32 {
        unsafe { outl(0xCF8, 0x8000_0000 | (0x1f << 11) | (2 << 8) | (reg & 0xFC)); inl(0xCFC) }
    };
    let id = rd(0);
    if id & 0xFFFF != 0x8086 || id == 0xFFFF_FFFF {
        serial_print_str(&alloc::format!("[PMC] reset cause: no Intel PMC at 00:1f.2 (id {:#x})\n", id));
        return;
    }
    let a = rd(0xA0);
    let b = rd(0xA4);
    let bar = rd(0x10);
    let mut etr3 = None;
    let base = (bar & !0xFFF) as u64;
    if bar & 1 == 0 && base != 0 && bar != 0xFFFF_FFFF {
        if let Some(va) = mm::kva::ioremap(base, 0x1000, mm::kva::CacheMode::Uncached) {
            etr3 = Some(unsafe { core::ptr::read_volatile((va + 0x48) as *const u32) });
        }
    }
    let pm1_sts = match unsafe { (*core::ptr::addr_of!(ACPI_POWER)).as_ref() } {
        Some(p) if p.pm1a_evt != 0 => Some(unsafe { inw(p.pm1a_evt) }),
        _ => None,
    };
    // (unverified) GEN_PMCON_A: GBL_RST_STS bit 24, HOST_RST_STS bit 9;
    // GEN_PMCON_B: PWR_FLR bit 1, SUS_PWR_FLR bit 14; ETR3: CF9GR bit 20,
    // CF9LOCK bit 31; PM1_STS: PWRBTN_STS bit 8, PRBTNOR_STS bit 11, WAK_STS bit 15.
    serial_print_str(&alloc::format!(
        "[PMC] reset cause: GEN_PMCON_A={:#010x} B={:#010x} PWRMBASE={:#x} ETR3={} PM1_STS={} | decode(unverified): GBL_RST={} HOST_RST={} PWR_FLR={} SUS_PWR_FLR={} CF9GR={} PWRBTN={} PRBTNOR={} WAK={}\n",
        a, b, base,
        match etr3 { Some(v) => alloc::format!("{:#010x}", v), None => alloc::string::String::from("n/a") },
        match pm1_sts { Some(v) => alloc::format!("{:#06x}", v), None => alloc::string::String::from("n/a") },
        (a >> 24) & 1, (a >> 9) & 1, (b >> 1) & 1, (b >> 14) & 1,
        etr3.map_or(0, |v| (v >> 20) & 1),
        pm1_sts.map_or(0, |v| (v >> 8) & 1), pm1_sts.map_or(0, |v| (v >> 11) & 1), pm1_sts.map_or(0, |v| (v >> 15) & 1)));
}
