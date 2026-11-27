//! `CONFIG_NVME=n` (or `=m`, where the module owns the driver): the entry points other code calls.
use alloc::string::String;
use alloc::vec::Vec;
pub fn init(_cmdline: &str) -> usize { 0 }
pub fn shutdown_all() {}
pub fn kexec_disable() -> Vec<String> { Vec::new() }
pub fn set_writable(_name: &str) -> bool { false }
pub fn set_read_only(_name: &str) -> bool { false }
/// Shape of the real driver's identity record (the kernel names it when it builds a disk's identity).
#[derive(Clone, Copy)]
pub struct Ident {
    pub model: [u8; 40],
    pub mlen: u8,
    pub serial: [u8; 20],
    pub slen: u8,
    pub ebs: [u8; 32],
    pub elen: u8,
    pub amzn: bool,
}
pub fn ident(_name: &str) -> Option<(Ident, u32)> { None }
pub fn amazon_root_writable(_is_boot: &dyn Fn(&str) -> bool) {}
