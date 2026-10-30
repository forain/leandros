//! `CONFIG_VIRTIO_KEYBOARD=n|m`: the kernel binds no virtio-input device (the module, if any, binds
//! it and feeds evdev through `kapi_input_*`). The timer tick and boot code keep calling these.
pub const VQ_STATS: bool = false;
pub struct VqCensus {
    pub polls: u32, pub skips: u32, pub drained: u32, pub maxb: u32, pub minfree: u32,
    pub starve: u32, pub aidx: u32, pub uidx: u32, pub notify: u32,
}
pub fn vq_census() -> VqCensus {
    VqCensus { polls: 0, skips: 0, drained: 0, maxb: 0, minfree: u32::MAX, starve: 0, aidx: 0, uidx: 0, notify: 0 }
}
pub fn init() {}
pub fn poll_events() {}
