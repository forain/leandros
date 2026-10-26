//! Host tests for `drivers/src/v3d`. The modules are included verbatim, so these exercise
//! exactly the code the kernel runs (the `rpi5` hardware glue is compiled out).
#![allow(unexpected_cfgs)]

// At the crate root so the modules' own `super::regs` / `super::mmu` paths resolve.
#[path = "../../src/v3d/regs.rs"]
pub mod regs;
#[path = "../../src/v3d/mmu.rs"]
pub mod mmu;
#[path = "../../src/v3d/irq.rs"]
pub mod irq;

/// Same paths as in the kernel (`crate::v3d::mmu`, ...).
pub mod v3d {
    pub use super::{irq, mmu, regs};
}

#[cfg(test)]
mod mock;
#[cfg(test)]
mod mmu_tests;
#[cfg(test)]
mod irq_tests;
