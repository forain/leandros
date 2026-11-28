//! KVM (Linux UAPI, arm64) ABI shared by the kvm-* lanes and userland kvmtest, plus the pure vGIC.
#![no_std]

extern crate alloc;

pub mod abi;
pub mod psci;
pub mod sysreg;
pub use abi::*;

pub mod ioevent;
pub mod irqfd;
pub mod memslot;
pub mod s2;
pub mod vmid;
pub mod hw;
pub mod esr;
pub mod mmio;
pub mod vgic;
