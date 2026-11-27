//! vCPU objects: creation, register ioctls, the kvm_run page.
use super::*;

pub const IRQ_PENDING: u8 = 1;
pub const FIQ_PENDING: u8 = 2;

pub struct Vcpu {
    pub id: u32,
    pub vm: Arc<Vm>,
    /// The `kvm_run` page (one kernel frame, mapped into QEMU by mmap of the vCPU fd).
    pub run_pa: usize,
    pub arch: Mutex<Box<dyn ArchVcpu>>,
    /// Features from KVM_ARM_VCPU_INIT (None until then; KVM_RUN needs it).
    pub init: Mutex<Option<KvmVcpuInit>>,
    /// Level of the CPU IRQ/FIQ lines driven by KVM_IRQ_LINE type CPU.
    pub irq_lines: AtomicU8,
    pub mp_state: AtomicU32,
    /// KVM_RUN loop state (only the vCPU's own thread locks it; see `run.rs`).
    pub run: Mutex<run::RunState>,
    /// Request bits (`kick.rs`): set by kick(), consumed by the run loop.
    pub req: AtomicU32,
    /// True while the thread executes guest code; `guest_cpu` is where.
    pub in_guest: AtomicBool,
    pub guest_cpu: AtomicUsize,
    /// Poll-channel tag of this vCPU's blocked state (WFI / powered off).
    pub wait_tag: u64,
    /// PSCI CPU_ON request for a stopped vCPU: (entry, context id).
    pub boot: Mutex<Option<(u64, u64)>>,
}

impl Vcpu {
    pub fn run_ptr(&self) -> *mut KvmRun { mm::phys_to_virt(self.run_pa) as *mut KvmRun }
    pub fn initialized(&self) -> bool { self.init.lock().is_some() }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        // The frame may still back a user mapping (the VMA holds its own reference).
        mm::pageref::unref_or_free(self.run_pa, 0);
    }
}

pub(super) fn create(vm: &Arc<Vm>, pid: u32, id: usize) -> isize {
    if id >= KVM_MAX_VCPUS as usize { return -22; }
    let arch = match arch().new_vcpu(vm, id as u32) { Ok(a) => a, Err(e) => return -(e as isize) };
    let Some(run_pa) = mm::buddy::alloc(0) else { return -12 };
    unsafe { core::ptr::write(mm::phys_to_virt(run_pa) as *mut KvmRun, KvmRun::zeroed()); }
    let v = Arc::new(Vcpu {
        id: id as u32, vm: vm.clone(), run_pa, arch: Mutex::new(arch), init: Mutex::new(None),
        irq_lines: AtomicU8::new(0), mp_state: AtomicU32::new(KVM_MP_STATE_UNINITIALIZED),
        run: Mutex::new(run::RunState::default()), req: AtomicU32::new(0),
        in_guest: AtomicBool::new(false), guest_cpu: AtomicUsize::new(0),
        wait_tag: 1u64 << (next_serial() & 63), boot: Mutex::new(None),
    });
    {
        let mut t = vm.vcpus.lock();
        if t[id].as_ref().map_or(false, |w| w.strong_count() > 0) { return -17; } // EEXIST
        t[id] = Some(Arc::downgrade(&v));
    }
    install(pid, K_VCPU, next_serial(), Obj::Vcpu(v))
}

pub(super) fn ioctl(v: &Arc<Vcpu>, pid: u32, cmd: usize, arg: usize) -> isize {
    use ::kvm::abi::ioctl::*;
    if sched::tgid_of(pid) != v.vm.tgid { return -5; }
    match cmd {
        KVM_RUN => {
            if !v.initialized() { return -8; } // ENOEXEC
            (run_fn())(&v.vm, v)
        }
        KVM_ARM_VCPU_INIT => {
            let init = match rd::<KvmVcpuInit>(arg) { Ok(i) => i, Err(e) => return e };
            if init.target != KVM_ARM_TARGET_GENERIC_V8 { return -22; }
            // Only POWER_OFF and PSCI_0_2 are supported (no 32-bit EL1, SVE, PMU).
            let known = (1u32 << KVM_ARM_VCPU_POWER_OFF) | (1 << KVM_ARM_VCPU_PSCI_0_2);
            if init.features[0] & !known != 0 || init.features[1..].iter().any(|&f| f != 0) { return -22; }
            if let Err(e) = v.arch.lock().init(&init) { return -(e as isize); }
            let off = vcpu_feature_set(&init.features, KVM_ARM_VCPU_POWER_OFF);
            v.mp_state.store(if off { KVM_MP_STATE_STOPPED } else { KVM_MP_STATE_RUNNABLE }, Ordering::Release);
            *v.init.lock() = Some(init);
            0
        }
        KVM_ARM_PREFERRED_TARGET => wr(arg, &KvmVcpuInit { target: KVM_ARM_TARGET_GENERIC_V8, features: [0; 7] }),
        KVM_GET_ONE_REG | KVM_SET_ONE_REG => one_reg(v, cmd == KVM_SET_ONE_REG, arg),
        KVM_GET_REG_LIST => reg_list(v, arg),
        KVM_SET_SIGNAL_MASK => 0, // accepted, unused: QEMU kicks with immediate_exit
        KVM_GET_MP_STATE => wr(arg, &KvmMpState { mp_state: v.mp_state.load(Ordering::Acquire) }),
        KVM_SET_MP_STATE => match rd::<KvmMpState>(arg) {
            Ok(s) if matches!(s.mp_state, KVM_MP_STATE_RUNNABLE | KVM_MP_STATE_STOPPED) => { v.mp_state.store(s.mp_state, Ordering::Release); (kick_fn())(v); 0 }
            Ok(_) => -22,
            Err(e) => e,
        },
        KVM_ENABLE_CAP => -22,
        _ => -25,
    }
}

fn one_reg(v: &Arc<Vcpu>, set: bool, arg: usize) -> isize {
    let r = match rd::<KvmOneReg>(arg) { Ok(r) => r, Err(e) => return e };
    let Some(sz) = RegSize::from_bits((r.id & KVM_REG_SIZE_MASK) >> KVM_REG_SIZE_SHIFT) else { return -22 };
    let n = sz.bytes();
    let mut buf = [0u8; 32];
    if set {
        if unsafe { ua::copy_from_user(buf.as_mut_ptr(), r.addr as usize, n) } != 0 { return -14; }
        match v.arch.lock().set_one(r.id, &buf[..n]) { Ok(()) => 0, Err(e) => -(e as isize) }
    } else {
        if let Err(e) = v.arch.lock().get_one(r.id, &mut buf[..n]) { return -(e as isize); }
        if unsafe { ua::copy_to_user(r.addr as usize, buf.as_ptr(), n) } != 0 { -14 } else { 0 }
    }
}

/// struct kvm_reg_list { u64 n; u64 reg[] }: too small -> E2BIG with n = count.
fn reg_list(v: &Arc<Vcpu>, arg: usize) -> isize {
    let hdr = match rd::<KvmRegList>(arg) { Ok(h) => h, Err(e) => return e };
    let list = v.arch.lock().reg_list();
    let count = list.len() as u64;
    if wr(arg, &KvmRegList { n: count }) != 0 { return -14; }
    if hdr.n < count { return -7; } // E2BIG
    if count == 0 { return 0; }
    if unsafe { ua::copy_to_user(arg + 8, list.as_ptr() as *const u8, list.len() * 8) } != 0 { -14 } else { 0 }
}
