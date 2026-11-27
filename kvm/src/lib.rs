//! KVM (Linux UAPI, arm64) ABI shared by the kvm-* lanes and userland kvmtest, plus the pure vGIC.
#![no_std]

extern crate alloc;

pub mod abi;
pub use abi::*;
pub mod vgic;
