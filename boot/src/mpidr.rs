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

/// PSCI `ALREADY_ON`: CPU_ON of a CPU the firmware still counts as on.
pub const PSCI_ALREADY_ON: i64 = -4;

/// What a CPU_ON return code means for AP bring-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApStart {
    /// The firmware starts the CPU at the entry point.
    Started,
    /// The CPU is still on, spinning in a kexec'ing kernel's park loop: it
    /// takes the entry point from the mailbox posted before CPU_ON.
    Mailbox,
    /// The CPU will not run the entry point.
    Failed,
}

/// Classify CPU_ON's `rc`; `mailbox` = a kexec AP mailbox was posted for it.
pub const fn ap_start(rc: i64, mailbox: bool) -> ApStart {
    match rc {
        0 => ApStart::Started,
        PSCI_ALREADY_ON if mailbox => ApStart::Mailbox,
        _ => ApStart::Failed,
    }
}

/// Does a kexec'ing kernel power its secondaries off with PSCI CPU_OFF
/// (`true`), or leave them in the park page's mailbox loop (`false`)?
/// The Pi 5's TF-A accepts CPU_ON for a CPU it powered off but never
/// restarts it, so the rpi5 build parks by default. `kexec.cpuoff=1|0` on
/// the kexec'ing kernel's command line overrides either way.
pub fn kexec_cpu_off(rpi5: bool, cmdline: &str) -> bool {
    let mut off = !rpi5;
    for t in cmdline.split_ascii_whitespace() {
        match t {
            "kexec.cpuoff=1" => off = true,
            "kexec.cpuoff=0" => off = false,
            _ => {}
        }
    }
    off
}
