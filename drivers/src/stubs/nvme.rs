//! `CONFIG_NVME=n` (or `=m`, where the module owns the driver): the entry points other code calls.
use alloc::string::String;
use alloc::vec::Vec;
pub fn init(_cmdline: &str) -> usize { 0 }
pub fn shutdown_all() {}
pub fn kexec_disable() -> Vec<String> { Vec::new() }
pub fn set_writable(_name: &str) -> bool { false }
pub fn set_read_only(_name: &str) -> bool { false }
