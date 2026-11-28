//! KVM_IRQFD: an eventfd write injects a GSI (arm64 default routing: GSI n = SPI INTID n + 32).
//! With `KVM_IRQFD_FLAG_RESAMPLE` the line stays asserted until the guest EOIs it; the run loop
//! then de-asserts it and signals the resamplefd (`resample`), so a level-triggered source can
//! re-assert. The vfs write path calls `on_eventfd_write` only while an irqfd exists.
use super::*;
use ::kvm::irqfd::{self as pure, Irqfd, IrqfdTable};

static COUNT: AtomicUsize = AtomicUsize::new(0);

pub(super) fn new_table() -> Mutex<IrqfdTable> { Mutex::new(IrqfdTable::new()) }

fn fd_slot(pid: u32, fd: u32) -> Result<usize, isize> {
    match vfs::vfs_get_node_kind(pid, fd as usize) {
        Some(vfs::VnodeKind::EventFd { slot }) => Ok(slot),
        Some(_) => Err(-22),
        None => Err(-9),
    }
}

fn set_line(vm: &Arc<Vm>, gsi: u32, level: bool) {
    if let Some(b) = vgic() {
        let _ = b.irq_line(vm, KVM_ARM_IRQ_TYPE_SPI, 0, pure::gsi_to_intid(gsi), level);
    }
}

impl Vm {
    pub(super) fn irqfd(self: &Arc<Vm>, pid: u32, a: &KvmIrqfd) -> isize {
        if let Err(e) = pure::check(a) { return -(e as isize); }
        if !self.has_vgic() { return -22; }
        let slot = match fd_slot(pid, a.fd) { Ok(s) => s, Err(e) => return e };
        if pure::deassign(a) {
            let removed = self.irqfds.lock().deassign(slot, a.gsi);
            return match removed {
                Some(e) => { release(e); 0 }
                None => -2,
            };
        }
        let resample = if pure::wants_resample(a) {
            match fd_slot(pid, a.resamplefd) { Ok(s) => Some(s), Err(e) => return e }
        } else { None };
        // The intid must exist.
        if self.vgic.lock().as_ref().map_or(true, |g| pure::gsi_to_intid(a.gsi) >= g.nr_irqs) { return -22; }
        let e = Irqfd { slot, gsi: a.gsi, resample };
        if let Err(err) = self.irqfds.lock().assign(e) { return -(err as isize); }
        vfs::eventfd_slot_ref(slot);
        if let Some(r) = resample { vfs::eventfd_slot_ref(r); }
        if COUNT.fetch_add(1, Ordering::AcqRel) == 0 { vfs::eventfd_set_hook(Some(on_eventfd_write)); }
        0
    }

    pub(super) fn drop_irqfds(&self) {
        for e in self.irqfds.lock().take_all() { release(e); }
    }

    /// Guest EOI of the level SPIs `ints`: de-assert each irqfd line and signal its resamplefd.
    pub(super) fn resample(self: &Arc<Vm>, ints: &[u32]) {
        for &intid in ints {
            let hits = self.irqfds.lock().resample_for(intid);
            for (gsi, rslot) in hits {
                set_line(self, gsi, false);
                vfs::eventfd_slot_signal(rslot);
            }
        }
    }
}

fn release(e: Irqfd) {
    vfs::eventfd_slot_unref(e.slot);
    if let Some(r) = e.resample { vfs::eventfd_slot_unref(r); }
    if COUNT.fetch_sub(1, Ordering::AcqRel) == 1 { vfs::eventfd_set_hook(None); }
}

/// vfs eventfd-write hook: inject every GSI bound to `slot`. Like Linux, the eventfd is drained;
/// a non-resample irqfd is an edge (line up then down), a resample one stays up until the EOI.
fn on_eventfd_write(slot: usize) {
    let vms: Vec<Arc<Vm>> = super::vm::VMS.lock().iter().filter_map(|w| w.upgrade()).collect();
    for vm in vms {
        let hits: Vec<(u32, bool)> = {
            let t = vm.irqfds.lock();
            if !t.has_slot(slot) { continue; }
            t.gsis_for(slot).into_iter().map(|g| (g, t.resample_for(pure::gsi_to_intid(g)).iter().any(|x| x.0 == g))).collect()
        };
        vfs::eventfd_slot_drain(slot);
        for (gsi, resampled) in hits {
            set_line(&vm, gsi, true);
            if !resampled { set_line(&vm, gsi, false); }
        }
    }
}
