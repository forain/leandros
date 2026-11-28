//! Kicking vCPUs: wake a blocked one, or knock a running one out of the guest
//! (lane kvm-run). Also the host-side interrupt plumbing KVM adds at boot.
use super::*;

pub const REQ_KICK: u32 = 1;

fn noop() {}

/// Host handler of the vtimer PPI: it fired while the guest ran (or just after the
/// exit). Mask it again at this CPU's redistributor; `timer::save` has already
/// recorded the timer, and the run loop re-evaluates it.
fn vtimer_ppi() { arch_aarch64::gic::ppi_set_enabled(arch_aarch64::gic::ppi_virt_timer_guest(), false); }

/// Boot: SGI 2 (kick), the vGIC maintenance PPI and the guest vtimer PPI handlers, and the
/// scheduler's signal-kick hook.
pub(super) fn init() {
    arch_aarch64::gic::register_handler(arch_aarch64::gic::SGI_KVM_KICK, noop);
    arch_aarch64::gic::register_handler(arch_aarch64::gic::PPI_KVM_MAINT, noop);
    arch_aarch64::gic::register_handler(arch_aarch64::gic::ppi_virt_timer_guest(), vtimer_ppi);
    sched::set_kvm_kick_hook(kick_cpu);
}

fn kick_cpu(cpu: usize) { arch_aarch64::gic::send_sgi(cpu, arch_aarch64::gic::SGI_KVM_KICK); }

/// Request an exit/re-evaluation of `v`: wake it if it sleeps in WFI or powered off,
/// or send SGI 2 to its CPU if it is inside the guest.
pub fn kick(v: &Arc<Vcpu>) {
    v.req.fetch_or(REQ_KICK, Ordering::SeqCst);
    if v.in_guest.load(Ordering::SeqCst) {
        kick_cpu(v.guest_cpu.load(Ordering::Relaxed));
    }
    sched::wake_poll_tagged(v.wait_tag);
}

/// Kick every vCPU named in `mask` (bit i = vCPU i), as returned by the vGIC.
pub fn kick_mask(vm: &Arc<Vm>, mut mask: u64) {
    while mask != 0 {
        let i = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        if let Some(v) = vm.vcpu(i) { kick(&v); }
    }
}

impl Vcpu {
    /// Block the calling thread (which is this vCPU's KVM_RUN thread) until `deadline_ns`
    /// (monotonic; `u64::MAX` = none), a kick, or a signal. Spurious returns are fine:
    /// the run loop re-evaluates everything.
    pub fn block_until(&self, deadline_ns: u64) {
        sched::block_on_poll_prepare_masked(deadline_ns, self.wait_tag);
        // Re-check after publishing "blocked": a kick that raced us is not lost.
        if self.req.load(Ordering::SeqCst) != 0 || sched::signal::has_deliverable_signal()
            || unsafe { (*self.run_ptr()).immediate_exit } != 0 {
            sched::block_on_poll_cancel();
            return;
        }
        sched::block_on_poll_commit();
    }
}
