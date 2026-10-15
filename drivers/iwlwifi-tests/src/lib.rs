//! Host tests for the kernel's iwlwifi hardware-independent modules. The
//! modules under test are included verbatim, so these exercise exactly the
//! code the kernel runs.

extern crate alloc;

#[path = "../../src/iwl_fw.rs"]
pub mod iwl_fw;

#[cfg(test)]
mod fw_tests;

#[path = "../../src/iwl_cmd.rs"]
pub mod iwl_cmd;
#[cfg(test)]
mod cmd_tests;

#[path = "../../src/iwl_assoc.rs"]
pub mod iwl_assoc;
#[cfg(test)]
mod assoc_tests;

#[path = "../../src/wifi_crypto.rs"]
pub mod wifi_crypto;
#[cfg(test)]
mod crypto_tests;

#[path = "../../src/wifi_frame.rs"]
pub mod wifi_frame;
#[cfg(test)]
mod frame_tests;
