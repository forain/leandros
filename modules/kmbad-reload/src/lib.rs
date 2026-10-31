//! Fixture: registers a tick callback whose ctx is this generation's id, and whose exit
//! unregisters nothing. With kmod.leaktick=1 the kernel also leaves the registration behind.
//! A tick delivered to generation N+1 code with generation N's ctx is logged as STALE REACHED.
#![no_std]
use core::sync::atomic::{AtomicU32, Ordering};
use kmod_rt::{kinfo, kerr};

kmod_rt::module! { name: "kmbad_reload", init: init, exit: exit }

static TICKS: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_tick(ctx: usize) {
    let me = kmod_rt::owner().0 as usize;
    if ctx != me {
        kerr!("kmbad_reload STALE REACHED ctx {} in gen {}", ctx, me);
        return;
    }
    let n = TICKS.fetch_add(1, Ordering::Relaxed);
    if n == 2 {
        kinfo!("kmbad_reload tick ok gen {}", me);
    }
}

fn init() -> i32 {
    let me = kmod_rt::owner().0 as usize;
    kinfo!("kmbad_reload up gen {}", me);
    match kmod_kapi::tick::register(on_tick, me) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

fn exit() {
    kinfo!("kmbad_reload exit gen {} (tick left registered)", kmod_rt::owner().0);
}
