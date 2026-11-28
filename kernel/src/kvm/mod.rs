//! KVM-compatible hypervisor, kernel glue (kvm-plan D2/D3; lane kvm-dev).
//!
//! Built only with `drv-kvm` on aarch64, and active only when the kernel runs
//! at EL2 with VHE. Files: `dev.rs` (/dev/kvm system ioctls), `vm.rs` (VM,
//! stage 2, VM ioctls), `vcpu.rs` (vCPU ioctls, kvm_run page), `memslot.rs`
//! (memslots, stage-2 fault, invalidate hook), `eventfd.rs` (IOEVENTFD),
//! `device.rs` (CREATE_DEVICE), `iface.rs` (seams to kvm-switch/-sysreg/-vgic/-run),
//! `arch.rs` (arch backend), `vgicdev.rs` (vGIC backend), `run.rs`/`timer.rs`/`kick.rs` (KVM_RUN).
use super::*;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use spin::Mutex;
use ::kvm::abi::*;
use ::kvm::abi::cap;

mod arch;
mod dev;
mod device;
mod eventfd;
mod iface;
mod kick;
mod memslot;
mod run;
mod timer;
mod vcpu;
mod vgicdev;
mod vm;

pub use device::Device;
pub use iface::*;
pub use memslot::FaultErr;
pub use vcpu::Vcpu;
pub use vm::Vm;

pub const K_VM: u32 = 1;
pub const K_VCPU: u32 = 2;
pub const K_DEV: u32 = 3;

pub enum Obj { Vm(Arc<Vm>), Vcpu(Arc<Vcpu>), Dev(Arc<Device>) }
impl Clone for Obj {
    fn clone(&self) -> Obj {
        match self { Obj::Vm(v) => Obj::Vm(v.clone()), Obj::Vcpu(v) => Obj::Vcpu(v.clone()), Obj::Dev(v) => Obj::Dev(v.clone()) }
    }
}

struct Entry { obj: Obj, refs: usize }
static OBJS: Mutex<BTreeMap<u32, Entry>> = Mutex::new(BTreeMap::new());
static SERIAL: AtomicU32 = AtomicU32::new(1);
static KVM_PORT: AtomicU32 = AtomicU32::new(u32::MAX);

pub(crate) fn next_serial() -> u32 { SERIAL.fetch_add(1, Ordering::Relaxed) & 0x0fff_ffff }
pub(crate) fn obj_id(kind: u32, serial: u32) -> u32 { kind << 28 | (serial & 0x0fff_ffff) }

/// Register `obj` (one reference, moved to the fd) and install its fd. On
/// failure the object is dropped again.
pub(crate) fn install(pid: u32, kind: u32, serial: u32, obj: Obj) -> isize {
    let id = obj_id(kind, serial);
    OBJS.lock().insert(id, Entry { obj, refs: 1 });
    let fd = vfs::install_kvm_obj(pid, id);
    if fd < 0 { obj_put(id); }
    fd
}

/// vfs hook: an fd for `id` was duplicated.
pub fn obj_get(id: u32) {
    if let Some(e) = OBJS.lock().get_mut(&id) { e.refs += 1; }
}

/// vfs hook: an fd for `id` closed. The last one drops the object (outside the lock).
pub fn obj_put(id: u32) {
    let gone = {
        let mut t = OBJS.lock();
        match t.get_mut(&id) {
            Some(e) if e.refs > 1 => { e.refs -= 1; None }
            Some(_) => t.remove(&id),
            None => None,
        }
    };
    drop(gone);
}

fn lookup(id: u32) -> Option<Obj> { OBJS.lock().get(&id).map(|e| e.obj.clone()) }

pub(crate) fn rd<T: Copy>(arg: usize) -> Result<T, isize> {
    let mut v = core::mem::MaybeUninit::<T>::uninit();
    if unsafe { ua::copy_from_user(v.as_mut_ptr() as *mut u8, arg, core::mem::size_of::<T>()) } != 0 { return Err(-14); }
    Ok(unsafe { v.assume_init() })
}

pub(crate) fn wr<T>(arg: usize, v: &T) -> isize {
    if unsafe { ua::copy_to_user(arg, v as *const T as *const u8, core::mem::size_of::<T>()) } != 0 { -14 } else { 0 }
}

/// Host IPA limit: min(ID_AA64MMFR0_EL1.PARange, 44).
pub(crate) fn parange() -> u64 {
    let r: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64mmfr0_el1", out(reg) r, options(nomem, nostack)); }
    r & 0xf
}

pub(crate) fn ipa_max_bits() -> u32 {
    let pa = match parange() { 0 => 32, 1 => 36, 2 => 40, 3 => 42, 4 => 44, _ => 48 };
    pa.min(44)
}

pub(crate) fn check_extension(c: u32, vgic: bool) -> i32 { cap::answer(c, ipa_max_bits(), vgic) }

/// Boot: /dev/kvm exists only on a VHE host (kernel at EL2, E2H=1).
pub(crate) fn init() {
    if !arch_aarch64::hyp::vhe() { return; }
    let Some(port) = ipc::port::create(0) else { return };
    if !ipc::port::register_kernel_handler(port, dev::handle) { return; }
    KVM_PORT.store(port, Ordering::Release);
    set_arch_backend(&arch::BACKEND);
    set_vgic_backend(&vgicdev::BACKEND);
    set_run_fn(run::run);
    set_kick_fn(kick::kick);
    kick::init();
    vfs::set_kvm_hooks(obj_get, obj_put);
    mm::vmm::set_range_invalidate_hook(memslot::invalidate);
    vfs::register_device("/dev/kvm", port, 0, 10, 232);
    serial_print_str("[KVM] /dev/kvm registered (EL2/VHE host)\n");
}

/// sys_ioctl hook (next to `syncfile_ioctl::intercept`): every `0xAE` ioctl on
/// /dev/kvm or a KVM object fd. `None` leaves everything else to the VFS.
pub(super) fn intercept(pid: u32, fd: usize, cmd: usize, arg: usize) -> Option<isize> {
    if (cmd >> 8) & 0xff != ::kvm::abi::ioctl::KVMIO { return None; }
    match vfs::vfs_get_node_kind(pid, fd)? {
        vfs::VnodeKind::KvmObj { id } => Some(match lookup(id)? {
            Obj::Vm(v) => vm::ioctl(&v, pid, cmd, arg),
            Obj::Vcpu(v) => vcpu::ioctl(&v, pid, cmd, arg),
            Obj::Dev(d) => device::ioctl(&d, pid, cmd, arg),
        }),
        vfs::VnodeKind::DynamicDevice { port, .. } if port == KVM_PORT.load(Ordering::Acquire) => Some(dev::ioctl(pid, cmd, arg)),
        _ => None,
    }
}

/// sys_mmap hook: mapping a vCPU fd gives the `kvm_run` page (MAP_SHARED, RW, NX).
/// `None` = not a KVM fd (the normal mmap path continues).
pub(super) fn mmap_vcpu(pid: u32, fd: usize, len: usize, off: usize, flags: usize, prot: usize,
                        virt: usize, place: MmapCand) -> Option<isize> {
    const MAP_SHARED: usize = 1;
    const MAP_FIXED: usize = 0x10;
    // No VM alive (a vCPU fd keeps its VM alive): no fd of ours can be a vCPU, so skip the
    // fd-table lookup on every file mmap.
    if vm::NVMS.load(Ordering::Acquire) == 0 { return None; }
    let id = vfs::kvm_obj_of_fd(pid, fd)?;
    let Some(Obj::Vcpu(v)) = lookup(id) else { return Some(-19) }; // ENODEV: only vCPU fds map
    if sched::tgid_of(pid) != v.vm.tgid { return Some(-5); }
    if flags & MAP_SHARED == 0 || off != 0 || len > KVM_VCPU_MMAP_SIZE || prot & 4 != 0 { return Some(-22); }
    let pf = prot_to_page_flags(prot);
    mm::pageref::inc(v.run_pa); // the VMA's reference (moved into map_shared_frames)
    let mut consumed = false;
    let r = sched::with_current_address_space_mut(|as_| {
        let at = if flags & MAP_FIXED != 0 { as_.unmap_range(virt, len); virt } else { mmap_place(as_, virt, len, place)? };
        consumed = true;
        Some((as_.map_shared_frames(at, &[v.run_pa], pf), at))
    });
    Some(match r {
        Some(Some((true, at))) => at as isize,
        _ => { if !consumed { mm::pageref::unref_or_free(v.run_pa, 0); } -12 }
    })
}
