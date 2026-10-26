//! Host tests for the brcmf chip/protocol layer (lane pibrcmf-core).
extern crate alloc;

#[allow(dead_code)]
pub mod brcmf;
/// Shared 802.11 parsing the brcmf scan results are checked against (lane pibrcmf-net).
#[path = "../../src/wifi_frame.rs"]
pub mod wifi_frame;
#[path = "../../src/wifi_crypto.rs"]
pub mod wifi_crypto;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod net_tests;
