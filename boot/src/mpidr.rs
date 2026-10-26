//! Boot-CPU election of the direct-boot entry (kernel/src/entry_aarch64.s).
//!
//! A machine that releases every core into `_start` at once (QEMU `raspi4b`;
//! possibly firmware without PSCI) must let exactly one through. The boot CPU
//! is the one whose whole affinity, Aff3:Aff2:Aff1:Aff0, is zero. Aff0 alone
//! is not enough: Cortex-A76/A78 (Pi 5) number cores in Aff1 with Aff0 = 0
//! on every core (MT = 1), and multi-cluster parts repeat Aff0 per cluster.

/// `MPIDR_EL1` affinity bits: Aff3 [39:32], Aff2..Aff0 [23:0] (not MT/U/RES1).
pub const AFF_MASK: u64 = 0xFF_00FF_FFFF;

/// Does `_start` let the CPU with this `MPIDR_EL1` through?
pub const fn is_boot_cpu(mpidr: u64) -> bool { mpidr & AFF_MASK == 0 }

/// The gate as written in entry_aarch64.s (checked by the host tests).
pub const ASM_GATE: [&str; 5] = [
    "mrs     x4, mpidr_el1",
    "and     x5, x4, #0xffffff           // Aff2:Aff1:Aff0",
    "ubfx    x6, x4, #32, #8             // Aff3",
    "orr     x4, x5, x6",
    "cbz     x4, _start_primary",
];
