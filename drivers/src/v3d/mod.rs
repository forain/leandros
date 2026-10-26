//! Broadcom V3D 7.1 GPU (BCM2712, Raspberry Pi 5) kernel driver.
//! Design: artifacts/notes/lane-piv3d-design-2026-10-26.md.
pub mod regs;
pub mod power;
pub mod mmu;
pub mod irq;
