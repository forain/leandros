//! virtio-rng as a loadable module: the SAME driver source as the built-in
//! (`drivers/src/virtio_rng.rs`), with `kenv` pointing at the kmod-kapi shim.
//! It registers through `kapi_hwrng_register`, which also reseeds the CSPRNG.
#![no_std]

mod kenv {
    pub use kmod_kapi::{mm, pci, sched};
}

#[path = "../../../drivers/src/virtio_rng.rs"]
mod virtio_rng;

kmod_rt::module! {
    name: "virtio_rng",
    init: init,
    exit: exit,
    ids: [kmod_rt::pci(0x1af4, 0x1044), kmod_rt::pci(0x1af4, 0x1005)]
}

fn init() -> i32 {
    virtio_rng::init();
    if virtio_rng::is_bound() {
        kmod_rt::kinfo!("virtio-rng bound");
        0
    } else {
        -19
    }
}

fn exit() {
    virtio_rng::shutdown();
}
