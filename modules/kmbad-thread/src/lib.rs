//! Fixture: init spawns a kthread that polls kapi_kthread_should_stop; exit returns at once.
//! delete_module must signal the stop and drain the thread before the slot is freed.
#![no_std]
use kmod_rt::kinfo;

kmod_rt::module! { name: "kmbad_thread", init: init, exit: exit }

extern "C" {
    fn kapi_msleep(ms: u64);
    fn kapi_kthread_should_stop() -> bool;
    fn kapi_kthread_spawn(name: *const u8, len: usize, f: extern "C" fn(*mut u8), arg: *mut u8, owner: kmod_rt::KmodId) -> i64;
}

extern "C" fn body(_: *mut u8) {
    let mut n = 0u32;
    unsafe {
        while !kapi_kthread_should_stop() {
            kapi_msleep(10);
            n += 1;
        }
    }
    kinfo!("kmbad_thread body stopped after {} naps", n);
}

fn init() -> i32 {
    let r = unsafe { kapi_kthread_spawn(b"kmbad".as_ptr(), 5, body, core::ptr::null_mut(), kmod_rt::owner()) };
    if r < 0 { return r as i32; }
    kinfo!("kmbad_thread up");
    0
}

fn exit() {
    kinfo!("kmbad_thread exit");
}
