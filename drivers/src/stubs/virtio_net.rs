//! `CONFIG_VIRTIO_NET=n|m`: the kernel claims no virtio-net device (the module, if any, binds it
//! and registers its own netifs); everything reports "no device". The virtio PCI capability
//! walkers live in `virtio.rs`.
#![allow(dead_code)]
pub fn init() {}
pub fn device_count() -> usize { 0 }
pub fn get_mac_address(_i: usize) -> Option<[u8; 6]> { None }
pub fn send_packet(_i: usize, _b: &[u8]) -> bool { false }
pub fn poll_receive(_i: usize, _b: &mut [u8]) -> Option<usize> { None }
pub fn rx_rearm(_i: usize) -> bool { false }
pub fn rx_irq_armed(_i: usize) -> bool { false }
pub fn link_up(_i: usize) -> bool { false }
pub fn link_generation(_i: usize) -> u32 { 0 }
pub fn rx_irq_count(_i: usize) -> u64 { 0 }

