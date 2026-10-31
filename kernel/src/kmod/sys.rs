//! init_module / finit_module / delete_module and `/proc/modules`.

use alloc::vec::Vec;
use super::load::{load_image, KmodErr, Source};
use super::registry::{self, State};

const EPERM: isize = -1;
const EFAULT: isize = -14;
const EINVAL: isize = -22;
const EBUSY: isize = -16;
const MAX_IMAGE: usize = 16 << 20;

fn errno(e: KmodErr) -> isize {
    match e {
        KmodErr::Init(rc) if rc < 0 => rc as isize,
        e => -e.errno(),
    }
}

fn check() -> Option<isize> {
    if sched::current_euid() != 0 || super::disabled() { Some(EPERM) } else { None }
}

pub fn sys_init_module(ptr: usize, len: usize, _params: usize) -> isize {
    if let Some(e) = check() { return e; }
    if len == 0 || len > MAX_IMAGE { return EINVAL; }
    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve_exact(len).is_err() { return -12; }
    buf.resize(len, 0);
    if unsafe { sched::uaccess::copy_from_user(buf.as_mut_ptr(), ptr, len) } != 0 { return EFAULT; }
    match load_image(&buf, Source::Syscall) { Ok(_) => 0, Err(e) => errno(e) }
}

pub fn sys_finit_module(fd: usize, _params: usize, flags: usize) -> isize {
    if let Some(e) = check() { return e; }
    if flags & 0xffff_ffff & !7 != 0 { return EINVAL; }
    let fd = fd as u32 as i32;
    if fd < 0 { return -9; }
    let fd = fd as usize;
    let size = crate::syscall::sys_lseek_pub(fd, 0, 2);
    if size < 0 { return size; }
    if crate::syscall::sys_lseek_pub(fd, 0, 0) < 0 { return -29; }
    let size = size as usize;
    if size == 0 || size > MAX_IMAGE { return EINVAL; }
    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve_exact(size).is_err() { return -12; }
    buf.resize(size, 0);
    let n = crate::syscall::read_fd_upto(fd, &mut buf);
    if n < 0 { return n; }
    if n as usize != size { return -5; }
    match load_image(&buf, Source::Syscall) { Ok(_) => 0, Err(e) => errno(e) }
}

const ENOENT: isize = -2;

fn user_name(ptr: usize) -> Result<[u8; 32], isize> {
    let mut n = [0u8; 32];
    for i in 0..31 {
        let mut b = 0u8;
        if unsafe { sched::uaccess::copy_from_user(&mut b as *mut u8, ptr + i, 1) } != 0 { return Err(EFAULT); }
        if b == 0 { return Ok(n); }
        n[i] = b;
    }
    Err(-36)
}

fn zombie(id: kmod_abi::KmodId, nm: &str, why: &str) -> isize {
    registry::abort_upper(id);
    registry::set_state(id, State::Zombie);
    drivers::pci::serial_debug(&alloc::format!("[kmod] {} ZOMBIE: {}\n", nm, why));
    EBUSY
}

/// delete_module: U0 busy checks, then Going -> (drain upper, drain lower, exit on a kthread
/// with a timeout, revoke) -> free, or Zombie when anything cannot be proven quiet.
pub fn sys_delete_module(name: usize, _flags: usize) -> isize {
    use super::gate::{self, Class};
    use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    if let Some(e) = check() { return e; }
    let nm = match user_name(name) { Ok(n) => n, Err(e) => return e };
    let m = match registry::find_by_name(&nm) { Some(m) => m, None => return ENOENT };
    let s = super::name_str(&nm);
    if !m.unloadable || m.exit == 0 { return EBUSY; }
    if registry::begin_unload(m.id).is_err() { return EBUSY; }
    let g = match gate::get(m.id) { Some(g) => g, None => return zombie(m.id, s, "no gate") };
    if gate::current().map_or(false, |c| c.id == m.id) { registry::set_state(m.id, State::Live); return -35; }
    let t0 = sched::monotonic_ns();
    // D9: close the Upper gate first, then judge busy rules against a quiescent upper edge.
    if !g.close_and_drain(Class::Upper, 2000) { return zombie(m.id, s, "upper drain timeout"); }
    if let Some(why) = registry::busy_reason(m.id) {
        g.reopen(Class::Upper);
        registry::set_state(m.id, State::Live);
        drivers::pci::serial_debug(&alloc::format!("[kmod] {} busy: {}\n", s, why));
        return EBUSY;
    }
    registry::unbind_upper(m.id);
    // D3 step 3: the device goes inert (INTx, MSI/MSI-X, bus mastering off) before the lower drain and exit().
    super::lower::quiesce_pci(m.id);
    g.stopping.store(true, Ordering::SeqCst);
    if !g.close_and_drain(Class::Lower, 5000) { return zombie(m.id, s, "lower drain timeout"); }
    // exit() on its own thread so a hang only costs a Zombie.
    let done = alloc::sync::Arc::new(AtomicBool::new(false));
    let rc = alloc::sync::Arc::new(AtomicI32::new(0));
    let (d2, r2, id, exit) = (done.clone(), rc.clone(), m.id, m.exit);
    let started = sched::kthread::spawn("kmod-exit", move || {
        let cl = crate::boot_cmdline().unwrap_or("");
        let ctx = kmod_abi::KmodCtx {
            ctx_size: core::mem::size_of::<kmod_abi::KmodCtx>() as u32, abi_version: kmod_abi::KMOD_ABI_VERSION, owner: id,
            _pad: 0, _pad2: 0, cmdline: cl.as_ptr(), cmdline_len: cl.len(), params: core::ptr::null(), params_len: 0,
        };
        let f: extern "C" fn(*const kmod_abi::KmodCtx) -> i32 = unsafe { core::mem::transmute(exit) };
        sched::kmodgen::set(id.0);
        registry::pre_exit(id);
        r2.store(f(&ctx), Ordering::SeqCst);
        sched::kmodgen::set(0);
        d2.store(true, Ordering::SeqCst);
    });
    if started.is_none() { return zombie(m.id, s, "cannot start exit thread"); }
    let end = sched::monotonic_ns() + 5_000_000_000;
    while !done.load(Ordering::SeqCst) {
        if sched::monotonic_ns() >= end { return zombie(m.id, s, "exit timeout"); }
        sched::kthread::msleep(1);
    }
    registry::commit_upper(m.id);
    // Revoke what the kernel can take back; kinds without a revoke path keep the slot (Zombie).
    registry::release_all(m.id);
    let dma = super::lower::dma_leftover(m.id);
    if dma != 0 { drivers::pci::serial_debug(&alloc::format!("[kmod] {} exit left {} DMA pages owned: logged, not freed\n", s, dma)); }
    let left = registry::ungated_left(m.id);
    if left != 0 { return zombie(m.id, s, "ungated registrations left"); }
    registry::forget_regs(m.id);
    let pages = registry::take_pages(m.id);
    // One range unmap (one shootdown). If a remote CPU may still hold a stale entry, leak the
    // frames rather than hand them back to the buddy allocator.
    let acked = unsafe { mm::kmodwin::unmap(m.base, (m.size + 4095) / 4096) };
    if acked {
        for &(_, p) in &pages { mm::buddy::free(p, 0); }
    } else {
        drivers::pci::serial_debug(&alloc::format!("[kmod] {} TLB shootdown unacked: leaking {} frames\n", s, pages.len()));
    }
    mm::kmodwin::release(m.base, m.size);
    registry::remove(m.id);
    gate::remove(m.id);
    drivers::pci::serial_debug(&alloc::format!("[kmod] {} unloaded id {} exit rc {} in {} ms, {} pages freed\n",
        s, m.id.0, rc.load(Ordering::SeqCst), (sched::monotonic_ns() - t0) / 1_000_000, pages.len()));
    0
}

/// `/proc/modules`: `<name> <size> <regs> - <Live|Failed|Loading> 0x<base>` per module.
#[no_mangle]
pub extern "C" fn arch_kmod_report(buf: *mut u8, cap: usize) -> usize {
    if buf.is_null() || cap == 0 { return 0; }
    let mut s = alloc::string::String::new();
    registry::for_each(&mut |m| {
        let st = match m.state { State::Live => "Live", State::Failed => "Failed", State::Coming => "Loading", State::Going => "Unloading", State::Zombie => "Zombie" };
        s.push_str(&alloc::format!("{} {} {} - {} {:#x}\n", super::name_str(&m.name), m.size, m.regs, st, m.base));
    });
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}
