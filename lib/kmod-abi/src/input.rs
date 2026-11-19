//! Input class (kmod W2-6): evdev nodes fed by module drivers.
pub const INPUT_KEYBOARD: u32 = 0;
pub const INPUT_POINTER: u32 = 1;
/// A hot-pluggable keyboard / mouse with its own evdev node (N >= 2); the kernel picks the node.
pub const INPUT_DEV_KEYBOARD: u32 = 2;
pub const INPUT_DEV_MOUSE: u32 = 3;
/// `kapi_rel_pointer_report` source id for USB mice.
pub const REL_SRC_USB: u32 = 0;

pub mod sys_input {
    use crate::KmodId;
    pub type InputRegisterFn = unsafe extern "C" fn(KmodId, u32, *const u8, usize, u16, u16, u16) -> i32;
    pub type InputEventFn = unsafe extern "C" fn(i32, u16, u16, i32);
    pub type InputSetCapsFn = unsafe extern "C" fn(i32, *const u8, usize, *const u8, usize) -> i32;
    pub type InputUnregisterFn = unsafe extern "C" fn(i32) -> i32;
    pub type InputEventConsoleFn = unsafe extern "C" fn(u16, u16, i32);
    pub type InputLedsFn = unsafe extern "C" fn() -> u32;
    pub type InputRepeatFn = unsafe extern "C" fn(*mut u32, *mut u32);
    pub type RelPointerReportFn = unsafe extern "C" fn(u32, i32, i32, i32, i32, u8);
    pub type RelPointerAdvertiseFn = unsafe extern "C" fn(u8, bool, bool);
    extern "C" {
        pub fn kapi_input_unregister(node: i32) -> i32;
        pub fn kapi_input_event_console(ty: u16, code: u16, value: i32);
        pub fn kapi_input_leds() -> u32;
        pub fn kapi_input_repeat(delay: *mut u32, rate: *mut u32);
        pub fn kapi_rel_pointer_report(src: u32, dx: i32, dy: i32, wheel: i32, hwheel: i32, buttons: u8);
        pub fn kapi_rel_pointer_advertise(buttons: u8, hwheel: bool, hires: bool);
        /// Claim the evdev node for `kind`; returns the node (>= 0) or -errno.
        pub fn kapi_input_register(owner: KmodId, kind: u32, name: *const u8, len: usize,
                                   bus: u16, vid: u16, pid: u16) -> i32;
        pub fn kapi_input_event(node: i32, ty: u16, code: u16, value: i32);
        pub fn kapi_input_set_props(node: i32, props: *const u8, pl: usize, ff: *const u8, fl: usize) -> i32;
        pub fn kapi_input_set_caps(node: i32, rel: *const u8, rl: usize, key: *const u8, kl: usize) -> i32;
    }
}
