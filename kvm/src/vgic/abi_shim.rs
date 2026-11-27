//! Single seam to kvm-abi (`crate::abi`): everything the vGIC needs from the UAPI is named here.
use crate::abi;

pub use abi::vgic::{
    CTRL_INIT, GRP_ADDR, GRP_CPU_REGS, GRP_CPU_SYSREGS, GRP_CTRL, GRP_DIST_REGS, GRP_LEVEL_INFO, GRP_NR_IRQS,
    GRP_REDIST_REGS, V2_ADDR_TYPE_CPU as V2_ADDR_CPU, V2_ADDR_TYPE_DIST as V2_ADDR_DIST,
    V3_ADDR_TYPE_DIST as V3_ADDR_DIST, V3_ADDR_TYPE_REDIST as V3_ADDR_REDIST,
    V3_ADDR_TYPE_REDIST_REGION as V3_ADDR_REDIST_REGION,
};
pub use abi::{KVM_ARM_IRQ_TYPE_PPI as IRQ_TYPE_PPI, KVM_ARM_IRQ_TYPE_SPI as IRQ_TYPE_SPI};
pub use abi::{KVM_DEV_TYPE_ARM_VGIC_V2, KVM_DEV_TYPE_ARM_VGIC_V3};

pub const EINVAL: i32 = 22;
pub const ENXIO: i32 = 6;
pub const EEXIST: i32 = 17;
pub const EBUSY: i32 = 16;

/// (type, vcpu, id)
pub fn irq_line_decode(irq: u32) -> (u32, usize, u32) {
    let (t, v, n) = abi::irq_line_decode(irq);
    (t, v as usize, n)
}
