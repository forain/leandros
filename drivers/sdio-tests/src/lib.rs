//! Host tests for the SDIO host core. `bus.rs` and `sdio_host.rs` are included
//! verbatim (the `rpi5` hardware half is compiled out), so these exercise the
//! exact CMD52/CMD53 code the kernel runs, against `mock::MockHost`.

#[path = "../../src/brcmf/bus.rs"]
pub mod bus;
#[path = "../../src/brcmf/sdio_host.rs"]
pub mod sdio_host;

pub mod mock;

#[cfg(test)]
mod tests;
