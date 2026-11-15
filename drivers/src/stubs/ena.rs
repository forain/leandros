//! `CONFIG_ENA=n`: the entry points other code calls, all inert.
use alloc::string::String;
pub fn shutdown() -> Option<String> { None }
