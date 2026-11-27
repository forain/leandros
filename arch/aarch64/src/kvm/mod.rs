//! EL2 support for the KVM-compatible hypervisor (see kvm-plan).
//! KVM host support: world switch between the VHE host at EL2
//! and an AArch64 guest at EL1/EL0. See `switch.s` / `vectors.s` / `ctx.rs`.

#[cfg(feature = "kvm")]
core::arch::global_asm!(include_str!("switch.s"));
#[cfg(feature = "kvm")]
core::arch::global_asm!(include_str!("vectors.s"));

pub mod tlb;
#[cfg(feature = "kvm")]
pub mod vgic_hw;
#[cfg(feature = "kvm")]
pub mod ctx;
#[cfg(feature = "kvm")]
pub use ctx::{enter, RawExit, VcpuHw};
#[cfg(feature = "kvm")]
pub use kvm_hw::{esr, hw, mmio};
