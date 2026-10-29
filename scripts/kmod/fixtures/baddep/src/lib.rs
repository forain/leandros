#![no_std]
extern crate mm;
kmod_rt::module! { name: "hello", init: init }
fn init() -> i32 {
    0
}
