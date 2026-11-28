//! The VM object: stage 2, VMID, vCPU table, VM-fd ioctls.
use super::*;
use ::kvm::s2::{FrameAlloc, Stage2};
use ::kvm::vmid::{VmidAlloc, VmidSlot};
use ::kvm::ioevent::IoEvent;
use ::kvm::memslot::SlotTable;

/// Stage-2 table frames from the buddy allocator (HHDM-addressed).
pub struct KFrames;

fn zero_frames(pa: usize, order: usize) {
    unsafe { core::ptr::write_bytes(mm::phys_to_virt(pa) as *mut u8, 0, 4096usize << order); }
}

impl FrameAlloc for KFrames {
    fn alloc_zeroed(&mut self) -> Option<(u64, *mut u64)> { self.alloc_zeroed_pages(1) }
    fn free(&mut self, pa: u64) { mm::buddy::free(pa as usize, 0); }
    fn ptr(&self, pa: u64) -> *mut u64 { mm::phys_to_virt(pa as usize) as *mut u64 }
    fn alloc_zeroed_pages(&mut self, n: usize) -> Option<(u64, *mut u64)> {
        let order = n.trailing_zeros() as usize;
        let pa = mm::buddy::alloc(order)?;
        zero_frames(pa, order);
        Some((pa as u64, mm::phys_to_virt(pa) as *mut u64))
    }
    fn free_pages(&mut self, pa: u64, n: usize) { mm::buddy::free(pa as usize, n.trailing_zeros() as usize); }
}

/// 8-bit VMIDs: valid on every CPU (ID_AA64MMFR1_EL1.VMIDBits >= 8).
static VMIDS: Mutex<VmidAlloc> = Mutex::new(VmidAlloc::new(8));

pub struct Vm {
    pub id: u32,
    /// Thread group that created the VM; its ioctls from another group are EIO.
    pub tgid: u32,
    /// Page-table root of the creator's address space (the invalidate hook's key).
    pub as_root: usize,
    pub ipa_bits: u32,
    pub slots: Mutex<SlotTable>,
    pub s2: Mutex<Stage2<KFrames>>,
    pub vmid: Mutex<VmidSlot>,
    pub vttbr: AtomicU64,
    /// VTCR_EL2 for this VM (8-bit VMIDs, so VS=0), loaded on every entry.
    pub vtcr: u64,
    pub vcpus: Mutex<[Option<Weak<Vcpu>>; KVM_MAX_VCPUS as usize]>,
    pub ioevents: Mutex<Vec<IoEvent>>,
    pub irqfds: Mutex<::kvm::irqfd::IrqfdTable>,
    /// Object id of the in-kernel irqchip device (0 = none).
    pub vgic_dev: AtomicU32,
    /// The in-kernel vGIC state machine (created by KVM_CREATE_DEVICE; `vgicdev.rs`).
    pub vgic: Mutex<Option<::kvm::vgic::Vgic>>,
}

impl Vm {
    pub fn has_vgic(&self) -> bool { self.vgic_dev.load(Ordering::Acquire) != 0 }
    pub fn vttbr(&self) -> u64 { self.vttbr.load(Ordering::Acquire) }

    pub fn vcpu(&self, i: usize) -> Option<Arc<Vcpu>> {
        self.vcpus.lock().get(i).and_then(|w| w.as_ref()).and_then(|w| w.upgrade())
    }

    /// Unmap `[ipa, ipa+len)` from stage 2: TLBI first, then unpin the frames.
    pub fn unmap_ipa(&self, ipa: u64, len: u64) {
        let mut pas: Vec<u64> = Vec::new();
        self.s2.lock().unmap(ipa, len, |pa| pas.push(pa));
        if pas.is_empty() { return; }
        unsafe { arch_aarch64::kvm::tlb::tlbi_range(self.vttbr(), ipa, len); }
        for pa in pas { mm::vmm::unpin_user_page(pa as usize); }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let mut pas: Vec<u64> = Vec::new();
        self.s2.get_mut().unmap_all(|pa| pas.push(pa));
        unsafe { arch_aarch64::kvm::tlb::tlbi_vmid(self.vttbr.load(Ordering::Relaxed)); }
        for pa in pas { mm::vmm::unpin_user_page(pa as usize); }
        VMIDS.lock().release(self.vmid.get_mut());
        self.drop_ioevents();
        self.drop_irqfds();
        NVMS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Live VMs (weak: the object table owns them). The invalidate hook walks it.
pub(super) static VMS: Mutex<Vec<Weak<Vm>>> = Mutex::new(Vec::new());
pub(super) static NVMS: AtomicUsize = AtomicUsize::new(0);

/// KVM_CREATE_VM: `arg` bits 7:0 = IPA size (0 = default 40).
pub(super) fn create(pid: u32, arg: usize) -> isize {
    let max = ipa_max_bits();
    let want = match (arg & 0xff) as u32 { 0 => 40u32.min(max), b => b };
    if want < 32 || want > max { return -22; }
    let Some(as_root) = sched::with_current_address_space(|a| a.root()) else { return -22 };
    let s2 = match Stage2::new(KFrames, want) { Ok(s) => s, Err((_, _)) => return -12 };
    let mut slot = VmidSlot::default();
    // The VMID is fixed for the VM's life (the run loop never re-calls get()), so it is
    // taken without rollover; when all 255 are live CREATE_VM fails with ENOMEM. A VMID
    // freed by a destroyed VM was flushed by that VM's tlbi_vmid in Drop.
    let Some(vmid) = VMIDS.lock().alloc_fixed(&mut slot) else { return -12 };
    let vttbr = s2.vttbr(vmid);
    let vtcr = s2.vtcr_with(super::parange(), false);
    let id = next_serial();
    let vm = Arc::new(Vm {
        id, tgid: sched::tgid_of(pid), as_root, ipa_bits: want,
        slots: Mutex::new(SlotTable::new()), s2: Mutex::new(s2), vmid: Mutex::new(slot),
        vttbr: AtomicU64::new(vttbr), vtcr, vcpus: Mutex::new([const { None }; KVM_MAX_VCPUS as usize]),
        ioevents: Mutex::new(Vec::new()), irqfds: super::irqfd::new_table(), vgic_dev: AtomicU32::new(0), vgic: Mutex::new(None),
    });
    NVMS.fetch_add(1, Ordering::AcqRel);
    { let mut v = VMS.lock(); v.retain(|w| w.strong_count() > 0); v.push(Arc::downgrade(&vm)); }
    install(pid, K_VM, id, Obj::Vm(vm))
}

pub(super) fn ioctl(vm: &Arc<Vm>, pid: u32, cmd: usize, arg: usize) -> isize {
    use ::kvm::abi::ioctl::*;
    if sched::tgid_of(pid) != vm.tgid { return -5; } // EIO, as on Linux
    match cmd {
        KVM_CHECK_EXTENSION => check_extension(arg as u32, vgic().is_some()) as isize,
        KVM_SET_USER_MEMORY_REGION => match rd::<KvmUserspaceMemoryRegion>(arg) {
            Ok(r) => vm.set_memory_region(&r),
            Err(e) => e,
        },
        KVM_CREATE_VCPU => vcpu::create(vm, pid, arg),
        KVM_ARM_PREFERRED_TARGET => {
            let init = KvmVcpuInit { target: KVM_ARM_TARGET_GENERIC_V8, features: [0; 7] };
            wr(arg, &init)
        }
        KVM_IRQ_LINE => match rd::<KvmIrqLevel>(arg) { Ok(l) => irq_line(vm, &l), Err(e) => e },
        KVM_CREATE_DEVICE => device::create(vm, pid, arg),
        KVM_IOEVENTFD => match rd::<KvmIoeventfd>(arg) { Ok(a) => vm.ioeventfd(pid, &a), Err(e) => e },
        KVM_IRQFD => match rd::<KvmIrqfd>(arg) { Ok(a) => vm.irqfd(pid, &a), Err(e) => e },
        KVM_ENABLE_CAP => -22,
        _ => -25, // ENOTTY
    }
}

fn irq_line(vm: &Arc<Vm>, l: &KvmIrqLevel) -> isize {
    let (ty, vcpu_id, num) = irq_line_decode(l.irq);
    let level = l.level != 0;
    match ty {
        KVM_ARM_IRQ_TYPE_CPU => {
            let Some(v) = vm.vcpu(vcpu_id as usize) else { return -22 };
            let bit = match num { KVM_ARM_IRQ_CPU_IRQ => vcpu::IRQ_PENDING, KVM_ARM_IRQ_CPU_FIQ => vcpu::FIQ_PENDING, _ => return -22 };
            if level { v.irq_lines.fetch_or(bit, Ordering::AcqRel); } else { v.irq_lines.fetch_and(!bit, Ordering::AcqRel); }
            (kick_fn())(&v);
            0
        }
        KVM_ARM_IRQ_TYPE_SPI | KVM_ARM_IRQ_TYPE_PPI => match (vm.has_vgic(), vgic()) {
            (true, Some(b)) => match b.irq_line(vm, ty, vcpu_id, num, level) { Ok(()) => 0, Err(e) => -(e as isize) },
            _ => -6, // ENXIO: no in-kernel irqchip
        },
        _ => -22,
    }
}
