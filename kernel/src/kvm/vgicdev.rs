//! In-kernel vGIC backend: KVM_CREATE_DEVICE (GICv3 / GICv2), device attributes, KVM_IRQ_LINE.
//! The state machine is `kvm::vgic::Vgic` (pure, host-tested); `run.rs` syncs it with the
//! ICH_* registers around every guest entry.
use super::*;
use ::kvm::abi::vgic::*;
use ::kvm::vgic::{Version, Vgic};

const ENODEV: i32 = 19;

pub struct Dev { vm: Arc<Vm> }

/// Bytes the user buffer behind a device attribute holds.
fn val_size(group: u32, attr: u64) -> usize {
    match group {
        GRP_CTRL => 0,
        GRP_ADDR | GRP_CPU_SYSREGS => 8,
        // GICv3 IROUTER<n> (offset 0x6100..0x8000) is a 64-bit register.
        GRP_DIST_REGS if (0x6100..0x8000).contains(&(attr & 0xffff_ffff)) => 8,
        _ => 4,
    }
}

impl DeviceOps for Dev {
    fn attr(&mut self, op: AttrOp, a: &mut KvmDeviceAttr) -> Result<(), i32> {
        let n = self.vm.vcpus.lock().iter().filter(|w| w.as_ref().map_or(false, |w| w.strong_count() > 0)).count();
        let mut guard = self.vm.vgic.lock();
        let g = guard.as_mut().ok_or(ENODEV)?;
        if op == AttrOp::Has { return g.attr_has(a.group, a.attr); }
        let sz = val_size(a.group, a.attr);
        let mut val = 0u64;
        if op == AttrOp::Set && sz != 0 {
            let mut b = [0u8; 8];
            if unsafe { ua::copy_from_user(b.as_mut_ptr(), a.addr as usize, sz) } != 0 { return Err(14); }
            val = u64::from_le_bytes(b);
        }
        if op == AttrOp::Set && a.group == GRP_CTRL && a.attr == CTRL_INIT {
            // QEMU creates every vCPU before INIT: size the redistributor table to them.
            g.set_nr_vcpus(n);
        }
        if op == AttrOp::Set { g.attr_set(a.group, a.attr, &mut val)?; } else { g.attr_get(a.group, a.attr, &mut val)?; }
        if op == AttrOp::Get && sz != 0 {
            let b = val.to_le_bytes();
            if unsafe { ua::copy_to_user(a.addr as usize, b.as_ptr(), sz) } != 0 { return Err(14); }
        }
        Ok(())
    }
}

pub struct Backend;
impl VgicBackend for Backend {
    fn supports(&self, ty: u32) -> bool {
        match ty {
            KVM_DEV_TYPE_ARM_VGIC_V3 => arch_aarch64::gic::is_v3(),
            KVM_DEV_TYPE_ARM_VGIC_V2 => !arch_aarch64::gic::is_v3() && arch_aarch64::kvm::vgic_hw::gich_va() != 0,
            _ => false,
        }
    }

    fn create(&self, vm: &Arc<Vm>, ty: u32) -> Result<Box<dyn DeviceOps>, i32> {
        let ver = if ty == KVM_DEV_TYPE_ARM_VGIC_V3 { Version::V3 } else { Version::V2 };
        let mut g = vm.vgic.lock();
        if g.is_some() { return Err(17); }
        *g = Some(Vgic::new(ver, 96, KVM_MAX_VCPUS as usize));
        Ok(Box::new(Dev { vm: vm.clone() }))
    }

    fn irq_line(&self, vm: &Arc<Vm>, ty: u32, vcpu: u32, num: u32, level: bool) -> Result<(), i32> {
        let irq = ty << KVM_ARM_IRQ_TYPE_SHIFT | vcpu << KVM_ARM_IRQ_VCPU_SHIFT | (num & KVM_ARM_IRQ_NUM_MASK);
        let kicks = {
            let mut g = vm.vgic.lock();
            g.as_mut().ok_or(6)?.irq_line(irq, level as u32)?
        };
        kick::kick_mask(vm, kicks);
        Ok(())
    }
    // irqfd lives in `irqfd.rs` (it needs the caller's fd table), not in this trait.
}
pub(super) static BACKEND: Backend = Backend;
