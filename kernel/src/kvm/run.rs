//! The KVM_RUN loop (kvm-plan D5, lane kvm-run).
//!
//! One vCPU = one normal task: the QEMU vCPU thread calls KVM_RUN and this loop runs
//! on its stack. Guest FP/SIMD state is swapped once per KVM_RUN (the kernel is
//! softfloat and every context switch saves q0-q31 as the thread's own, so a
//! preemption inside the loop carries the guest's registers along). IRQs are masked
//! only around `enter()`; after every exit they are opened for one instruction so a
//! pending host interrupt (tick, kick SGI, maintenance, the guest-timer PPI) is
//! taken before the exit is handled.
//!
//! Locking: `Vcpu::arch` is held for the whole iteration except while blocked (WFI /
//! powered off), so other vCPUs (PSCI CPU_ON) only touch `Vcpu::boot` and kick.
use super::*;
use ::kvm::abi::exit::*;
use ::kvm::esr::{self, Exit};
use ::kvm::hw::{self as khw, RawExit, VcpuHw, GUEST_HCR, HCR_VF, HCR_VI, HCR_VSE};
use ::kvm::psci::{self, CpuState, PsciOut, PsciVm};
use ::kvm::sysreg::{self, TrapResult};
use ::kvm::vgic::lr::{VgicHw, V2Codec, V3Codec, HCR_EN};
use ::kvm::vgic::Version;
use arch_aarch64::kvm::vgic_hw::{self, V2Hw, V3Hw};

const EINTR: isize = 4;
const ENOSYS_: isize = 38;

#[derive(Clone, Copy)]
struct PendingRead { rt: u8, sse: bool, sf: bool, sas: u8 }

/// Per-vCPU loop state (locked only by the vCPU's own thread).
#[derive(Default)]
pub struct RunState {
    /// An MMIO read that went to userspace: finish it on the next KVM_RUN.
    pending_read: Option<PendingRead>,
    /// The guest executed WFI (or PSCI CPU_SUSPEND) and no interrupt was pending yet.
    wfi: bool,
    /// Inject a virtual SError on the next entry.
    vse: bool,
    /// `device_irq_level` last given to userspace (kernel-irqchip=off).
    last_dev_level: u64,
    /// Timer lines last pushed into the in-kernel vGIC (PPI 27 / PPI 30).
    vt_line: bool,
    pt_line: bool,
}

extern "C" {
    fn fpsimd_save_live(buf: *mut u8);
    fn fpsimd_load_live(buf: *const u8);
}
#[repr(C, align(16))]
struct FpBuf([u8; 528]);

enum Flow { Resume, User(isize) }

fn set_exit(run: *mut KvmRun, reason: u32) { unsafe { (*run).exit_reason = reason; } }

fn internal_error(run: *mut KvmRun, sub: u32, data: &[u64]) -> Flow {
    let mut ev = KvmRunSystemEvent { type_: sub, ndata: data.len().min(16) as u32, data: [0; 16] };
    ev.data[..data.len().min(16)].copy_from_slice(&data[..data.len().min(16)]);
    unsafe { (*run).exit.system_event = ev; }
    set_exit(run, KVM_EXIT_INTERNAL_ERROR);
    Flow::User(0)
}

/// KVM_RUN.
pub(super) fn run(vm: &Arc<Vm>, v: &Arc<Vcpu>) -> isize {
    let run = v.run_ptr();
    unsafe {
        if (*run).immediate_exit != 0 { (*run).exit_reason = KVM_EXIT_INTR; return -EINTR; }
    }
    // Finish an MMIO read userspace has now served.
    {
        let pr = v.run.lock().pending_read.take();
        if let Some(p) = pr {
            let data = unsafe { u64::from_le_bytes((*run).exit.mmio.data) };
            let mut g = v.arch.lock();
            if let Some(hw) = g.hw_mut() { ::kvm::mmio::finish_read(hw, p.rt, p.sse, p.sf, p.sas, data); }
        }
    }
    let gfp: *mut u8 = {
        let mut g = v.arch.lock();
        match g.hw_mut() {
            // q0-q31, fpsr, fpcr: the layout of `fpsimd_{save,load}_live` (El1Regs::vregs is followed by fpsr/fpcr).
            Some(h) => &mut h.el1.vregs as *mut _ as *mut u8,
            None => return -ENOSYS_,
        }
    };
    let mut host_fp = FpBuf([0; 528]);
    unsafe { fpsimd_save_live(host_fp.0.as_mut_ptr()); fpsimd_load_live(gfp); }
    let r = run_loop(vm, v, run, gfp);
    unsafe { fpsimd_save_live(gfp); fpsimd_load_live(host_fp.0.as_ptr()); }
    publish_levels(vm, v, run);
    r
}

fn timer_lines(hw: &VcpuHw) -> u64 {
    super::timer::vtimer_level(hw) as u64 | (super::timer::ptimer_level(hw) as u64) << 1
}

/// Every return to userspace tells it the timer output levels (kernel-irqchip=off).
fn publish_levels(vm: &Arc<Vm>, v: &Arc<Vcpu>, run: *mut KvmRun) {
    if vm.has_vgic() { return; }
    let mut g = v.arch.lock();
    let Some(hw) = g.hw_mut() else { return };
    let lvl = timer_lines(hw);
    unsafe { (*run).s[0] = lvl; }
    v.run.lock().last_dev_level = lvl;
}

fn vcpu_irq_pending(vm: &Vm, v: &Vcpu) -> bool {
    if v.irq_lines.load(Ordering::Acquire) != 0 { return true; }
    match vm.vgic.lock().as_ref() { Some(g) => g.pending(v.id as usize), None => false }
}

fn run_loop(vm: &Arc<Vm>, v: &Arc<Vcpu>, run: *mut KvmRun, gfp: *mut u8) -> isize {
    loop {
        sched::preempt_check();
        if unsafe { (*run).immediate_exit } != 0 || sched::signal::has_deliverable_signal() {
            set_exit(run, KVM_EXIT_INTR);
            return -EINTR;
        }
        // Clear the request bit, then evaluate everything: a kick after this point is seen
        // by the re-check with IRQs masked right before the entry.
        v.req.swap(0, Ordering::SeqCst);

        if v.mp_state.load(Ordering::Acquire) == KVM_MP_STATE_STOPPED {
            let boot = v.boot.lock().take();
            match boot {
                Some((entry, ctx)) => {
                    v.arch.lock().power_on(entry, ctx);
                    unsafe { fpsimd_load_live(gfp); }
                    v.run.lock().wfi = false;
                    v.mp_state.store(KVM_MP_STATE_RUNNABLE, Ordering::Release);
                }
                None => { v.block_until(u64::MAX); continue; }
            }
        }

        let mut g = v.arch.lock();
        let hw = g.hw_mut().unwrap();
        let has_vgic = vm.has_vgic();
        let (vt, pt) = (super::timer::vtimer_level(hw), super::timer::ptimer_level(hw));
        let mut kicks = 0u64;
        {
            let mut rs = v.run.lock();
            if has_vgic {
                if let Some(vg) = vm.vgic.lock().as_mut() {
                    if vt != rs.vt_line { rs.vt_line = vt; kicks |= vg.set_line(27, v.id as usize, vt); }
                    if pt != rs.pt_line { rs.pt_line = pt; kicks |= vg.set_line(30, v.id as usize, pt); }
                }
            } else {
                // kernel-irqchip=off: userspace owns the interrupt line, tell it on change.
                let lvl = vt as u64 | (pt as u64) << 1;
                unsafe { (*run).s[0] = lvl; }
                if lvl != rs.last_dev_level {
                    rs.last_dev_level = lvl;
                    set_exit(run, KVM_EXIT_INTR);
                    return -EINTR;
                }
            }
        }
        if kicks != 0 { super::kick::kick_mask(vm, kicks & !(1u64 << v.id)); }

        // WFI: sleep until an interrupt, a kick, a signal, or the timer deadline.
        let want_wfi = v.run.lock().wfi;
        if want_wfi {
            // An asserted timer output wakes a WFI even when nothing was injected
            // (architecturally WFI wakes on a pending interrupt regardless of PSTATE masks).
            if !vcpu_irq_pending(vm, v) && !vt && !pt {
                let dl = super::timer::wfi_deadline_ns(hw);
                drop(g);
                v.block_until(dl);
                continue;
            }
            v.run.lock().wfi = false;
        }

        // ---- enter -------------------------------------------------------------------
        let lines = v.irq_lines.load(Ordering::Acquire);
        let vse = core::mem::take(&mut v.run.lock().vse);
        hw.vttbr = vm.vttbr();
        hw.vtcr = vm.vtcr;
        hw.hcr = GUEST_HCR
            | if lines & vcpu::IRQ_PENDING != 0 { HCR_VI } else { 0 }
            | if lines & vcpu::FIQ_PENDING != 0 { HCR_VF } else { 0 }
            | if vse { HCR_VSE } else { 0 };
        vgic_enter(vm, v, hw);
        let forced = vt;
        super::timer::load(hw, forced);

        let flags = sched::ksync::irq_save();
        v.guest_cpu.store(unsafe { sched::cpu_id() }, Ordering::Relaxed);
        v.in_guest.store(true, Ordering::SeqCst);
        sched::set_current_vcpu_mode(sched::task::VCPU_IN_GUEST);
        let abort = v.req.load(Ordering::SeqCst) != 0
            || unsafe { (*run).immediate_exit } != 0
            || sched::signal::has_deliverable_signal();
        let raw = if abort { None } else { Some(unsafe { arch_aarch64::kvm::enter(hw) }) };
        sched::set_current_vcpu_mode(sched::task::VCPU_OUTSIDE);
        v.in_guest.store(false, Ordering::SeqCst);
        super::timer::save(hw, forced);
        let k = vgic_leave(vm, v, hw);
        // Let a pending host interrupt in before the exit is handled.
        unsafe { core::arch::asm!("msr daifclr, #2", "isb", "msr daifset, #2", options(nomem, nostack)); }
        sched::ksync::irq_restore(flags);
        if k != 0 { super::kick::kick_mask(vm, k & !(1u64 << v.id)); }
        let Some(raw) = raw else { continue };

        match handle_exit(vm, v, hw, run, &raw) {
            Flow::Resume => {}
            Flow::User(r) => return r,
        }
    }
}

// ---- vGIC <-> hardware ------------------------------------------------------------------

fn vgic_enter(vm: &Vm, v: &Vcpu, hw: &VcpuHw) {
    let mut guard = vm.vgic.lock();
    let Some(g) = guard.as_mut() else { return };
    if !g.ready { return; }
    let id = v.id as usize;
    match g.version {
        Version::V3 => {
            vgic_hw::enable_v3_guest_access();
            vgic_hw::restore_v3(&hw.ich);
            let mut h = V3Hw::new();
            g.sync_in(id, &mut h, &V3Codec);
            let c = h.hcr(); h.set_hcr(c | HCR_EN);
        }
        Version::V2 => {
            let mut h = unsafe { V2Hw::new(vgic_hw::gich_va()) };
            h.restore(&hw.ich);
            g.sync_in(id, &mut h, &V2Codec);
            let c = h.hcr(); h.set_hcr(c | HCR_EN);
        }
    }
}

/// Fold the list registers back; returns the vCPU kick mask the fold produced.
fn vgic_leave(vm: &Vm, v: &Vcpu, hw: &mut VcpuHw) -> u64 {
    let mut guard = vm.vgic.lock();
    let Some(g) = guard.as_mut() else { return 0 };
    if !g.ready { return 0; }
    let id = v.id as usize;
    match g.version {
        Version::V3 => {
            vgic_hw::save_v3(&mut hw.ich);
            let mut h = V3Hw::new();
            g.sync_out(id, &mut h, &V3Codec);
        }
        Version::V2 => {
            let mut h = unsafe { V2Hw::new(vgic_hw::gich_va()) };
            h.save(&mut hw.ich);
            g.sync_out(id, &mut h, &V2Codec);
        }
    }
    g.take_kicks()
}

// ---- exits ----------------------------------------------------------------------------

struct VmPsci<'a>(&'a Vm);
impl PsciVm for VmPsci<'_> {
    fn nr_vcpus(&self) -> u32 {
        let t = self.0.vcpus.lock();
        t.iter().rposition(|w| w.as_ref().map_or(false, |w| w.strong_count() > 0)).map_or(0, |i| i as u32 + 1)
    }
    fn vcpu_mpidr(&self, vcpu: u32) -> u64 { 0x8000_0000 | vcpu as u64 }
    fn vcpu_state(&self, vcpu: u32) -> CpuState {
        match self.0.vcpu(vcpu as usize) {
            Some(v) if v.mp_state.load(Ordering::Acquire) == KVM_MP_STATE_RUNNABLE => CpuState::On,
            Some(v) if v.boot.lock().is_some() => CpuState::OnPending,
            _ => CpuState::Off,
        }
    }
    fn cpu_on(&self, target: u32, entry: u64, ctx: u64) -> Result<(), i64> {
        let v = self.0.vcpu(target as usize).ok_or(psci::INVALID_PARAMS)?;
        if v.mp_state.load(Ordering::Acquire) == KVM_MP_STATE_RUNNABLE { return Err(psci::ALREADY_ON); }
        {
            let mut b = v.boot.lock();
            if b.is_some() { return Err(psci::ON_PENDING); }
            *b = Some((entry, ctx));
        }
        super::kick::kick(&v);
        Ok(())
    }
}

fn advance(hw: &mut VcpuHw, esr_val: u64) { ::kvm::mmio::advance_pc(hw, esr::insn_len(esr_val)); }

/// Make the guest take a synchronous UNDEFINED exception at EL1 (trapped system register
/// the VM does not implement). TODO(kvm-run): SPSR.PAN/SSBS/DIT bits and SCTLR.SPAN
/// handling like Linux's enter_exception64.
fn inject_undef(hw: &mut VcpuHw) {
    let off = match hw.pstate & 0xf { 0 => 0x400, 4 => 0x000, _ => 0x200 }; // EL0 / EL1t / EL1h
    hw.el1.elr_el1 = hw.pc;
    hw.el1.spsr[0] = hw.pstate;
    hw.el1.esr = 1 << 25; // EC=0 (unknown), IL=1
    hw.pstate = 0x3c5;
    hw.pc = (hw.el1.vbar & !0x7ff) + off;
}

fn handle_exit(vm: &Arc<Vm>, v: &Arc<Vcpu>, hw: &mut VcpuHw, run: *mut KvmRun, raw: &RawExit) -> Flow {
    match esr::decode_raw(raw) {
        Exit::Irq | Exit::Fiq => Flow::Resume,
        Exit::SError => { v.run.lock().vse = true; Flow::Resume }
        Exit::Wfx { is_wfe } => {
            advance(hw, raw.esr);
            if is_wfe { sched::yield_now("kvm_wfe"); } else { v.run.lock().wfi = true; }
            Flow::Resume
        }
        Exit::Hvc { .. } | Exit::Smc { .. } => {
            let is_smc = matches!(esr::decode_raw(raw), Exit::Smc { .. });
            let mut x = [hw.gp[0], hw.gp[1], hw.gp[2], hw.gp[3]];
            let out = psci::handle(&VmPsci(vm), v.id, &mut x);
            hw.gp[..4].copy_from_slice(&x);
            if is_smc { advance(hw, raw.esr); }
            match out {
                PsciOut::Resume => Flow::Resume,
                PsciOut::Wfi => { v.run.lock().wfi = true; Flow::Resume }
                PsciOut::CpuOff => {
                    v.run.lock().wfi = false;
                    v.mp_state.store(KVM_MP_STATE_STOPPED, Ordering::Release);
                    Flow::Resume
                }
                PsciOut::SystemOff | PsciOut::SystemReset => {
                    let ty = if out == PsciOut::SystemOff { KVM_SYSTEM_EVENT_SHUTDOWN } else { KVM_SYSTEM_EVENT_RESET };
                    unsafe { (*run).exit.system_event = KvmRunSystemEvent { type_: ty, ndata: 0, data: [0; 16] }; }
                    set_exit(run, KVM_EXIT_SYSTEM_EVENT);
                    Flow::User(0)
                }
            }
        }
        Exit::SysReg(t) => {
            // GICv3 SGI generation traps (HCR_EL2.IMO): ICC_SGI1R/ASGI1R/SGI0R.
            if !t.is_read && t.op0 == 3 && t.crn == 12 && t.crm == 11
                && matches!((t.op1, t.op2), (0, 5) | (1, 6) | (0, 7)) {
                let val = if t.rt == 31 { 0 } else { hw.gp[t.rt as usize] };
                let kicks = vm.vgic.lock().as_mut().map(|g| g.inject_sgi(v.id as usize, val));
                match kicks {
                    Some(k) => { advance(hw, raw.esr); super::kick::kick_mask(vm, k & !(1u64 << v.id)); }
                    None => inject_undef(hw),
                }
                return Flow::Resume;
            }
            match sysreg::trap(hw, esr::iss(raw.esr)) {
                TrapResult::Handled => {}
                TrapResult::Undef => inject_undef(hw),
            }
            Flow::Resume
        }
        Exit::DataAbort(da) => {
            match vm.fault(da.ipa, da.write, false) {
                Ok(()) => return Flow::Resume,
                Err(FaultErr::Bad) => return internal_error(run, KVM_INTERNAL_ERROR_EMULATION, &[raw.esr, raw.far, da.ipa]),
                Err(_) => {} // no memslot / read-only slot: MMIO
            }
            if da.cm { advance(hw, raw.esr); return Flow::Resume; }
            if !da.isv { return internal_error(run, KVM_INTERNAL_ERROR_EMULATION, &[raw.esr, raw.far, da.ipa]); }
            let len = da.size as usize;
            let mask = if len >= 8 { u64::MAX } else { (1u64 << (8 * len)) - 1 };
            let mut val = if da.write && da.srt < 31 { hw.gp[da.srt as usize] & mask } else { 0 };
            if da.write && vm.try_ioeventfd(da.ipa, len as u32, val) {
                advance(hw, raw.esr);
                return Flow::Resume;
            }
            let vg = {
                let mut guard = vm.vgic.lock();
                match guard.as_mut() {
                    Some(g) => if g.mmio(v.id as usize, da.ipa, len, da.write, &mut val) { Some(g.take_kicks()) } else { None },
                    None => None,
                }
            };
            if let Some(k) = vg {
                if !da.write { ::kvm::mmio::finish_read(hw, da.srt, da.sse, da.sf, da.sas, val); }
                advance(hw, raw.esr);
                super::kick::kick_mask(vm, k & !(1u64 << v.id));
                return Flow::Resume;
            }
            // To userspace: the write data now, the read completed on the next KVM_RUN.
            unsafe {
                (*run).exit.mmio = KvmRunMmio {
                    phys_addr: da.ipa, data: val.to_le_bytes(), len: len as u32, is_write: da.write as u8,
                };
            }
            set_exit(run, KVM_EXIT_MMIO);
            v.run.lock().pending_read = if da.write { None } else {
                Some(PendingRead { rt: da.srt, sse: da.sse, sf: da.sf, sas: da.sas })
            };
            advance(hw, raw.esr);
            Flow::User(0)
        }
        Exit::InstAbort(ia) => match vm.fault(ia.ipa, false, true) {
            Ok(()) => Flow::Resume,
            // TODO(kvm-run): inject a prefetch abort for a fetch from a non-memslot IPA.
            Err(_) => internal_error(run, KVM_INTERNAL_ERROR_EMULATION, &[raw.esr, raw.far, ia.ipa]),
        },
        Exit::Unknown { ec, iss } => internal_error(run, KVM_INTERNAL_ERROR_UNEXPECTED_EXIT_REASON, &[ec as u64, iss as u64, raw.esr]),
    }
}

#[allow(dead_code)]
const _: u64 = khw::RESET_PSTATE;
