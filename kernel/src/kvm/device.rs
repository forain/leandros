//! KVM_CREATE_DEVICE and the device-attribute ioctls (the in-kernel vGIC).
use super::*;

pub struct Device {
    pub vm: Arc<Vm>,
    pub ty: u32,
    pub ops: Mutex<Box<dyn DeviceOps>>,
}

pub(super) fn create(vm: &Arc<Vm>, pid: u32, arg: usize) -> isize {
    let mut cd = match rd::<KvmCreateDevice>(arg) { Ok(c) => c, Err(e) => return e };
    let Some(b) = vgic() else { return -19 }; // ENODEV: no in-kernel irqchip yet
    if !matches!(cd.type_, KVM_DEV_TYPE_ARM_VGIC_V2 | KVM_DEV_TYPE_ARM_VGIC_V3) || !b.supports(cd.type_) { return -19; }
    if cd.flags & !KVM_CREATE_DEVICE_TEST != 0 { return -22; }
    if cd.flags & KVM_CREATE_DEVICE_TEST != 0 { return 0; }
    if vm.has_vgic() { return -17; } // EEXIST: one irqchip per VM
    let ops = match b.create(vm, cd.type_) { Ok(o) => o, Err(e) => return -(e as isize) };
    let id = next_serial();
    let fd = install(pid, K_DEV, id, Obj::Dev(Arc::new(Device { vm: vm.clone(), ty: cd.type_, ops: Mutex::new(ops) })));
    if fd < 0 { return fd; }
    vm.vgic_dev.store(obj_id(K_DEV, id), Ordering::Release);
    cd.fd = fd as u32;
    wr(arg, &cd)
}

pub(super) fn ioctl(d: &Arc<Device>, pid: u32, cmd: usize, arg: usize) -> isize {
    use ::kvm::abi::ioctl::*;
    if sched::tgid_of(pid) != d.vm.tgid { return -5; }
    let op = match cmd {
        KVM_SET_DEVICE_ATTR => AttrOp::Set,
        KVM_GET_DEVICE_ATTR => AttrOp::Get,
        KVM_HAS_DEVICE_ATTR => AttrOp::Has,
        _ => return -25,
    };
    let mut a = match rd::<KvmDeviceAttr>(arg) { Ok(a) => a, Err(e) => return e };
    if let Err(e) = d.ops.lock().attr(op, &mut a) { return -(e as isize); }
    0
}
