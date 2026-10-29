//! `CONFIG_E1000E=n`: the entry points other code calls, all inert.
use alloc::string::String;
pub fn shutdown() -> Option<String> { None }
pub fn pm_suspend() -> Result<String, String> { Ok(String::from("e1000e: not built")) }
pub fn pm_resume() -> Result<String, String> { Ok(String::from("e1000e: not built")) }
