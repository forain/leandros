//! KVM (Linux UAPI, arm64) ABI shared by the kvm-* lanes and userland kvmtest.
#![no_std]

pub mod abi;
pub use abi::*;

pub mod ioevent;
pub mod memslot;
pub mod s2;
pub mod vmid;
