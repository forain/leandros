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
#[cfg(test)]
mod teardown_tests;

#[path = "../../src/iwl_rs.rs"]
pub mod iwl_rs;
#[cfg(test)]
mod rs_tests;
#[cfg(test)]
mod col_tests;

#[path = "../../src/iwl_agg.rs"]
pub mod iwl_agg;
#[cfg(test)]
mod agg_tests;
#[cfg(test)]
mod ht40_tests;
#[cfg(test)]
mod vht_tests;
#[cfg(test)]
mod ant_tests;

#[path = "../../src/wifi_pref.rs"]
pub mod wifi_pref;
#[cfg(test)]
mod pref_tests;

#[path = "../../src/wifi_crypto.rs"]
pub mod wifi_crypto;
#[cfg(test)]
mod crypto_tests;
#[cfg(test)]
mod akm6_tests;
#[cfg(test)]
mod tkip_tests;

#[path = "../../src/wifi_sae.rs"]
pub mod wifi_sae;
#[cfg(test)]
mod sae_tests;

#[path = "../../src/wifi_frame.rs"]
pub mod wifi_frame;
#[cfg(test)]
mod frame_tests;

#[path = "../../src/wifi_ht.rs"]
pub mod wifi_ht;
#[cfg(test)]
mod ht_tests;
#[cfg(test)]
mod vhtrs_tests;
