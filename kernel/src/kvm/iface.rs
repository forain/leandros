//! Seams to the other kvm lanes. Each has a default that answers ENOSYS (or
//! ENODEV for devices) so this lane builds and runs without them; the owning
//! lane installs its backend with the `set_*` functions at init.
use super::*;

pub const ENOSYS: i32 = 38;

/// Per-vCPU architectural state (kvm-switch `VcpuHw` + kvm-sysreg accessors).
pub trait ArchVcpu: Send {
    /// KVM_ARM_VCPU_INIT: reset to the state `init.features` ask for.
    fn init(&mut self, _init: &KvmVcpuInit) -> Result<(), i32> { Err(ENOSYS) }
    /// KVM_GET_ONE_REG: fill `buf` (exactly the register's size) for `id`.
    fn get_one(&mut self, _id: u64, _buf: &mut [u8]) -> Result<(), i32> { Err(ENOSYS) }
    fn set_one(&mut self, _id: u64, _buf: &[u8]) -> Result<(), i32> { Err(ENOSYS) }
    /// KVM_GET_REG_LIST.
    fn reg_list(&self) -> &'static [u64] { &[] }
    /// The hardware context the KVM_RUN loop enters the guest with (None for the stub).
    fn hw_mut(&mut self) -> Option<&mut ::kvm::hw::VcpuHw> { None }
    /// PSCI CPU_ON / first start of a vCPU that was powered off: reset, then pc=`entry`, x0=`ctx`.
    fn power_on(&mut self, _entry: u64, _ctx: u64) {}
}

pub trait ArchBackend: Sync {
    fn new_vcpu(&self, vm: &Vm, id: u32) -> Result<Box<dyn ArchVcpu>, i32>;
}

struct StubVcpu;
impl ArchVcpu for StubVcpu {}
struct StubArch;
impl ArchBackend for StubArch {
    fn new_vcpu(&self, _vm: &Vm, _id: u32) -> Result<Box<dyn ArchVcpu>, i32> { Ok(Box::new(StubVcpu)) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AttrOp { Set, Get, Has }

/// An in-kernel device behind a KVM_CREATE_DEVICE fd (the vGIC).
pub trait DeviceOps: Send {
    fn attr(&mut self, op: AttrOp, a: &mut KvmDeviceAttr) -> Result<(), i32>;
}

/// kvm-vgic: creation of the in-kernel interrupt controller and its line inputs.
pub trait VgicBackend: Sync {
    fn supports(&self, ty: u32) -> bool;
    fn create(&self, vm: &Arc<Vm>, ty: u32) -> Result<Box<dyn DeviceOps>, i32>;
    /// KVM_IRQ_LINE of type SPI/PPI.
    fn irq_line(&self, vm: &Arc<Vm>, ty: u32, vcpu: u32, num: u32, level: bool) -> Result<(), i32>;
    /// KVM_IRQFD (assign or deassign): `target` is the eventfd slot (referenced by the caller).
    fn irqfd(&self, _vm: &Arc<Vm>, _a: &KvmIrqfd) -> Result<(), i32> { Err(ENOSYS) }
}

/// kvm-run: the KVM_RUN loop.
pub type RunFn = fn(&Arc<Vm>, &Arc<Vcpu>) -> isize;
/// kvm-run: wake/kick a vCPU after an interrupt line changed.
pub type KickFn = fn(&Arc<Vcpu>);

struct Hooks {
    arch: &'static dyn ArchBackend,
    vgic: Option<&'static dyn VgicBackend>,
    run: RunFn,
    kick: KickFn,
}

fn stub_run(_vm: &Arc<Vm>, _vcpu: &Arc<Vcpu>) -> isize { -(ENOSYS as isize) }
fn stub_kick(_vcpu: &Arc<Vcpu>) {}

static HOOKS: Mutex<Hooks> = Mutex::new(Hooks { arch: &StubArch, vgic: None, run: stub_run, kick: stub_kick });

pub fn set_arch_backend(b: &'static dyn ArchBackend) { HOOKS.lock().arch = b; }
pub fn set_vgic_backend(b: &'static dyn VgicBackend) { HOOKS.lock().vgic = Some(b); }
pub fn set_run_fn(f: RunFn) { HOOKS.lock().run = f; }
pub fn set_kick_fn(f: KickFn) { HOOKS.lock().kick = f; }

pub(super) fn arch() -> &'static dyn ArchBackend { HOOKS.lock().arch }
pub(super) fn vgic() -> Option<&'static dyn VgicBackend> { HOOKS.lock().vgic }
pub(super) fn run_fn() -> RunFn { HOOKS.lock().run }
pub(super) fn kick_fn() -> KickFn { HOOKS.lock().kick }
