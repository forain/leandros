//! `CONFIG_VIRTIO_SND=n|m`: the kernel claims no virtio-sound device (the module, if any, binds it
//! and attaches through `kapi_snd_register`). `TX_BUF_BYTES` and `monotonic_us` stay: the HD Audio
//! driver and the DRM timing code use them.
#![allow(dead_code)]
pub const TX_BUF_BYTES: usize = 512;
pub use crate::kenv::sched::monotonic_us;
pub struct VirtioSnd;
impl VirtioSnd {
    pub const fn new() -> Self { VirtioSnd }
    pub fn probe(&mut self) -> Result<(), crate::DriverError> { Err(crate::DriverError::NotFound) }
    pub fn reconfigure_stream(&mut self, _stream_id: u32, _freq: u32, _channels: u8) {}
    pub fn send_pcm_data(&mut self, _data: &[u8]) -> usize { 0 }
    pub fn tx_used_idx(&self) -> u16 { 0 }
    pub fn tx_level(&self) -> u16 { 0 }
    pub fn top_up_silence(&mut self) {}
    pub fn recover_stream(&mut self) {}
}
