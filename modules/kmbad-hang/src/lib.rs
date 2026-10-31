//! Fixture: exit never returns. delete_module must give up after its timeout and leave a Zombie.
#![no_std]
use kmod_rt::kinfo;

kmod_rt::module! { name: "kmbad_hang", init: init, exit: exit }

extern "C" {
    fn kapi_msleep(ms: u64);
}

fn init() -> i32 {
    kinfo!("kmbad_hang up");
    0
}

fn exit() {
    kinfo!("kmbad_hang exit: hanging");
    loop {
        unsafe { kapi_msleep(100) };
    }
}
