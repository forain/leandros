//! Host build of the hardware-independent brcmf modules. They sit at the
//! crate root here, so their `super::` paths resolve as in the kernel.
extern crate alloc;

#[path = "../../src/wifi_frame.rs"]
pub mod wifi_frame;
#[path = "../../src/wifi_crypto.rs"]
pub mod wifi_crypto;
#[path = "../../src/brcmf/bus.rs"]
pub mod bus;
#[path = "../../src/brcmf/fwil.rs"]
pub mod fwil;
#[path = "../../src/brcmf/event.rs"]
pub mod event;
#[path = "../../src/brcmf/cfg.rs"]
pub mod cfg;

#[cfg(test)]
mod tests;
