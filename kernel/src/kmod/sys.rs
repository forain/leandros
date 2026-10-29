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

pub fn sys_delete_module(_name: usize, _flags: usize) -> isize {
    if let Some(e) = check() { return e; }
    EBUSY // modules never unload in v1
}

/// `/proc/modules`: `<name> <size> <regs> - <Live|Failed|Loading> 0x<base>` per module.
#[no_mangle]
pub extern "C" fn arch_kmod_report(buf: *mut u8, cap: usize) -> usize {
    if buf.is_null() || cap == 0 { return 0; }
    let mut s = alloc::string::String::new();
    registry::for_each(&mut |m| {
        let st = match m.state { State::Live => "Live", State::Failed => "Failed", State::Coming => "Loading" };
        s.push_str(&alloc::format!("{} {} {} - {} {:#x}\n", super::name_str(&m.name), m.size, m.regs, st, m.base));
    });
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}
