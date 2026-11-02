//! virtio-input (keyboard + tablet) as a loadable module: the SAME driver source as the built-in
//! (`drivers/src/virtio_keyboard.rs`), with `kenv` pointing at the kmod-kapi shim. Autoloads by PCI
//! alias (1af4:1052), feeds the evdev nodes through `kapi_input_*` and polls from `kapi_tick_register`.
#![no_std]

extern crate alloc;

mod kenv {
    pub use kmod_kapi::{input, irq, mm, pci, sched, serial_raw};
}

#[path = "../../../drivers/src/virtio.rs"]
#[allow(dead_code)]
mod virtio;
#[path = "../../../drivers/src/virtio_keyboard.rs"]
#[allow(dead_code)]
mod virtio_keyboard;

kmod_rt::module! {
    name: "virtio_keyboard",
    init: init,
    exit: exit,
    ids: [kmod_rt::pci(0x1af4, 0x1052)]
}

extern "C" fn tick(_: usize) { virtio_keyboard::poll_events() }

fn init() -> i32 {
    virtio_keyboard::init();
    if virtio_keyboard::VIRTIO_INPUTS.lock().is_empty() { return -19; }
    if kmod_kapi::tick::register(tick, 0).is_err() { return -12; }
    0
}

/// Unload: what protects this exit() is the Lower-gate drain (no tick/IRQ is inside the module any more);
/// the kernel's release_all (IRQ/tick registrations, node claims) runs after exit(). Reset the devices and free the DMA.
fn exit() {
    let n = virtio_keyboard::free_all();
    kmod_rt::kinfo!("virtio_keyboard: freed {} DMA pages", n);
}
