#![no_std]
extern "C" {
    fn mm_secret() -> u32;
}
kmod_rt::module! { name: "hello", init: init }
fn init() -> i32 {
    unsafe { mm_secret() as i32 }
}
