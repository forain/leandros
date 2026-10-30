//! Input class (kmod W2-6): evdev nodes fed by module drivers.
pub const INPUT_KEYBOARD: u32 = 0;
pub const INPUT_POINTER: u32 = 1;

pub mod sys_input {
    use crate::KmodId;
    pub type InputRegisterFn = unsafe extern "C" fn(KmodId, u32, *const u8, usize, u16, u16, u16) -> i32;
    pub type InputEventFn = unsafe extern "C" fn(i32, u16, u16, i32);
    pub type InputSetCapsFn = unsafe extern "C" fn(i32, *const u8, usize, *const u8, usize) -> i32;
    extern "C" {
        /// Claim the evdev node for `kind`; returns the node (>= 0) or -errno.
        pub fn kapi_input_register(owner: KmodId, kind: u32, name: *const u8, len: usize,
                                   bus: u16, vid: u16, pid: u16) -> i32;
        pub fn kapi_input_event(node: i32, ty: u16, code: u16, value: i32);
        pub fn kapi_input_set_caps(node: i32, rel: *const u8, rl: usize, key: *const u8, kl: usize) -> i32;
    }
}
