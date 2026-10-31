//! Minimal Rust module: proves the build pipeline (alloc, fmt, spin hook, kapi logging).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::vec::Vec;
use kmod_rt::{kinfo, kerr};

kmod_rt::module! { name: "hello", init: init, exit: exit, of: ["leandros,test"] }

fn exit() {
    kinfo!("hello exit");
}

extern "C" {
    fn kapi_spin_held_count() -> u32;
}

static LOCK: spin::Mutex<u32> = spin::Mutex::new(0);

fn init() -> i32 {
    kinfo!("hello from rust");
    let mut v: Vec<u32> = Vec::new();
    for i in 0..16 {
        v.push(i * i);
    }
    let s = format!("vec sum={} len={}", v.iter().sum::<u32>(), v.len());
    kinfo!("{}", s);
    let mut g = LOCK.lock();
    *g += 1;
    let held = unsafe { kapi_spin_held_count() };
    kinfo!("spin held count={}", held);
    drop(g);
    if held < 1 {
        kerr!("spin hook did not account the lock");
        return -5;
    }
    0
}
