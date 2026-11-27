//! KVM host support (feature `kvm`): world switch between the VHE host at EL2
//! and an AArch64 guest at EL1/EL0. See `switch.s` / `vectors.s` / `ctx.rs`.

core::arch::global_asm!(include_str!("switch.s"));
core::arch::global_asm!(include_str!("vectors.s"));

pub mod ctx;
pub use ctx::{enter, RawExit, VcpuHw};
pub use kvm_hw::{esr, hw, mmio};
