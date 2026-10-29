#![no_std]
kmod_rt::module! { name: "hello", init: init }
fn init() -> i32 {
    // fmov d0, x0 (encoded: the softfloat target cannot assemble FP mnemonics)
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!(".inst 0x9e670000", options(nomem, nostack));
    }
    0
}
