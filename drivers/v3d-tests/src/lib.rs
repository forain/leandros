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
#[path = "../../src/v3d/queue.rs"]
pub mod queue;
#[path = "../../src/v3d/submit.rs"]
pub mod submit;
#[path = "../../src/v3d/selftest_cl.rs"]
pub mod selftest_cl;

extern crate alloc;
#[path = "../../src/kms_v3d.rs"]
pub mod kms_v3d;
#[path = "../../src/hvs_plane.rs"]
pub mod hvs_plane;

/// Same paths as in the kernel (`crate::v3d::mmu`, ...).
pub mod v3d {
    pub use super::{irq, mmu, queue, regs, selftest_cl, submit};
}

#[cfg(test)]
mod mock;
#[cfg(test)]
mod mmu_tests;
#[cfg(test)]
mod irq_tests;
#[cfg(test)]
mod queue_tests;
#[cfg(test)]
mod submit_tests;
