//! Fixture: claims the fixed pointer input node, which the kernel cannot take back. exit returns at once,
//! so delete_module must leave the module a Zombie with its slot mapped and never free it.
#![no_std]
use kmod_rt::kinfo;

kmod_rt::module! { name: "kmbad_leak", init: init, exit: exit }

extern "C" {
    fn kapi_input_register(owner: kmod_rt::KmodId, kind: u32, name: *const u8, len: usize, bus: u16, vid: u16, pid: u16) -> i32;
}

fn init() -> i32 {
    let r = unsafe { kapi_input_register(kmod_rt::owner(), 1, core::ptr::null(), 0, 0, 0, 0) };
    if r < 0 { return r; }
    kinfo!("kmbad_leak up node {}", r);
    0
}

fn exit() { kinfo!("kmbad_leak exit"); }
