//! /dev/kvm: the system fd ioctls (and the kernel handler behind its VFS port).
use super::*;

pub(super) fn handle(msg: &Message, _caller: u32, _target: u32) -> Message {
    const VFS_OPEN: u64 = 0x10;
    const VFS_CLOSE: u64 = 0x13;
    let v: i64 = match msg.tag { VFS_OPEN | VFS_CLOSE => 0, _ => -25 };
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

pub(super) fn ioctl(pid: u32, cmd: usize, arg: usize) -> isize {
    use ::kvm::abi::ioctl::*;
    match cmd {
        KVM_GET_API_VERSION => KVM_API_VERSION as isize,
        KVM_CHECK_EXTENSION => check_extension(arg as u32, vgic().is_some()) as isize,
        KVM_GET_VCPU_MMAP_SIZE => KVM_VCPU_MMAP_SIZE as isize,
        KVM_CREATE_VM => vm::create(pid, arg),
        _ => -25,
    }
}
