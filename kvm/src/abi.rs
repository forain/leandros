//! Linux KVM UAPI: ioctl numbers, structures, capability numbers, exit
//! reasons and ONE_REG register-id encodings (arm64).
//!
//! Source of truth: include/uapi/linux/kvm.h and
//! arch/arm64/include/uapi/asm/kvm.h. Where kvm-plan.md section 2 differs from
//! Linux, Linux wins: KVM_CAP_IOEVENTFD_ANY_LENGTH = 122 (plan said 167),
//! KVM_CAP_ARM_USER_IRQ = 144 (plan said 148), KVM_CAP_ARM_PSCI = 87 and
//! KVM_CAP_ARM_PSCI_0_2 = 102 (plan listed 102 for both).
#![allow(clippy::upper_case_acronyms)]

use core::mem::{offset_of, size_of};

pub const KVM_API_VERSION: i32 = 12;
pub const KVM_VCPU_MMAP_SIZE: usize = 4096;
pub const KVM_MAX_VCPUS: u32 = 8;
pub const KVM_NR_MEMSLOTS: u32 = 32;

// ---------------------------------------------------------------- ioctl
pub mod ioctl {
    pub const KVMIO: usize = 0xAE;
    pub const DIR_NONE: usize = 0;
    pub const DIR_WRITE: usize = 1;
    pub const DIR_READ: usize = 2;

    /// Linux `_IOC(dir, type, nr, size)` (asm-generic layout, used on arm64).
    pub const fn ioc(dir: usize, ty: usize, nr: usize, size: usize) -> usize {
        (dir << 30) | (size << 16) | (ty << 8) | nr
    }
    pub const fn io(nr: usize) -> usize {
        ioc(DIR_NONE, KVMIO, nr, 0)
    }
    pub const fn iow(nr: usize, size: usize) -> usize {
        ioc(DIR_WRITE, KVMIO, nr, size)
    }
    pub const fn ior(nr: usize, size: usize) -> usize {
        ioc(DIR_READ, KVMIO, nr, size)
    }
    pub const fn iowr(nr: usize, size: usize) -> usize {
        ioc(DIR_READ | DIR_WRITE, KVMIO, nr, size)
    }
    pub const fn dir(req: usize) -> usize {
        (req >> 30) & 3
    }
    pub const fn size(req: usize) -> usize {
        (req >> 16) & 0x3fff
    }
    pub const fn ty(req: usize) -> usize {
        (req >> 8) & 0xff
    }
    pub const fn nr(req: usize) -> usize {
        req & 0xff
    }

    use super::*;

    // system fd
    pub const KVM_GET_API_VERSION: usize = io(0x00);
    pub const KVM_CREATE_VM: usize = io(0x01);
    pub const KVM_CHECK_EXTENSION: usize = io(0x03);
    pub const KVM_GET_VCPU_MMAP_SIZE: usize = io(0x04);
    // vm fd
    pub const KVM_CREATE_VCPU: usize = io(0x41);
    pub const KVM_SET_USER_MEMORY_REGION: usize = iow(0x46, size_of::<KvmUserspaceMemoryRegion>());
    pub const KVM_IRQ_LINE: usize = iow(0x61, size_of::<KvmIrqLevel>());
    pub const KVM_IRQFD: usize = iow(0x76, size_of::<KvmIrqfd>());
    pub const KVM_IOEVENTFD: usize = iow(0x79, size_of::<KvmIoeventfd>());
    pub const KVM_ENABLE_CAP: usize = iow(0xa3, size_of::<KvmEnableCap>());
    pub const KVM_ARM_PREFERRED_TARGET: usize = ior(0xaf, size_of::<KvmVcpuInit>());
    pub const KVM_CREATE_DEVICE: usize = iowr(0xe0, size_of::<KvmCreateDevice>());
    // device fd (Linux defines GET/HAS as _IOW too)
    pub const KVM_SET_DEVICE_ATTR: usize = iow(0xe1, size_of::<KvmDeviceAttr>());
    pub const KVM_GET_DEVICE_ATTR: usize = iow(0xe2, size_of::<KvmDeviceAttr>());
    pub const KVM_HAS_DEVICE_ATTR: usize = iow(0xe3, size_of::<KvmDeviceAttr>());
    // vcpu fd
    pub const KVM_RUN: usize = io(0x80);
    pub const KVM_SET_SIGNAL_MASK: usize = iow(0x8b, 4); // struct kvm_signal_mask {u32 len; u8 sigset[]}
    pub const KVM_GET_MP_STATE: usize = ior(0x98, size_of::<KvmMpState>());
    pub const KVM_SET_MP_STATE: usize = iow(0x99, size_of::<KvmMpState>());
    pub const KVM_GET_ONE_REG: usize = iow(0xab, size_of::<KvmOneReg>());
    pub const KVM_SET_ONE_REG: usize = iow(0xac, size_of::<KvmOneReg>());
    pub const KVM_ARM_VCPU_INIT: usize = iow(0xae, size_of::<KvmVcpuInit>());
    pub const KVM_GET_REG_LIST: usize = iowr(0xb0, size_of::<KvmRegList>());
}

// ---------------------------------------------------------------- structs
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvmUserspaceMemoryRegion {
    pub slot: u32,
    pub flags: u32,
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
}
pub const KVM_MEM_LOG_DIRTY_PAGES: u32 = 1 << 0;
pub const KVM_MEM_READONLY: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvmOneReg {
    pub id: u64,
    pub addr: u64,
}

/// Header of `struct kvm_reg_list`; `reg[n]` (u64) follows in memory.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvmRegList {
    pub n: u64,
}

pub const KVM_ARM_TARGET_GENERIC_V8: u32 = 5;
pub const KVM_ARM_VCPU_POWER_OFF: u32 = 0;
pub const KVM_ARM_VCPU_EL1_32BIT: u32 = 1;
pub const KVM_ARM_VCPU_PSCI_0_2: u32 = 2;
/// Feature *bit numbers* (index into the 224-bit `features` bitmap).
pub const fn vcpu_feature_set(features: &[u32; 7], bit: u32) -> bool {
    features[(bit / 32) as usize] & (1 << (bit % 32)) != 0
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvmVcpuInit {
    pub target: u32,
    pub features: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvmIrqLevel {
    pub irq: u32,
    pub level: u32,
}
pub const KVM_ARM_IRQ_TYPE_SHIFT: u32 = 24;
pub const KVM_ARM_IRQ_TYPE_MASK: u32 = 0xff;
pub const KVM_ARM_IRQ_VCPU_SHIFT: u32 = 16;
pub const KVM_ARM_IRQ_VCPU_MASK: u32 = 0xff;
pub const KVM_ARM_IRQ_NUM_SHIFT: u32 = 0;
pub const KVM_ARM_IRQ_NUM_MASK: u32 = 0xffff;
pub const KVM_ARM_IRQ_TYPE_CPU: u32 = 0;
pub const KVM_ARM_IRQ_TYPE_SPI: u32 = 1;
pub const KVM_ARM_IRQ_TYPE_PPI: u32 = 2;
pub const KVM_ARM_IRQ_CPU_IRQ: u32 = 0;
pub const KVM_ARM_IRQ_CPU_FIQ: u32 = 1;
pub const KVM_ARM_IRQ_GIC_MAX: u32 = 127;

pub const fn irq_line(ty: u32, vcpu: u32, num: u32) -> u32 {
    ((ty & KVM_ARM_IRQ_TYPE_MASK) << KVM_ARM_IRQ_TYPE_SHIFT)
        | ((vcpu & KVM_ARM_IRQ_VCPU_MASK) << KVM_ARM_IRQ_VCPU_SHIFT)
        | (num & KVM_ARM_IRQ_NUM_MASK)
}
/// (type, vcpu, num)
pub const fn irq_line_decode(irq: u32) -> (u32, u32, u32) {
    (
        (irq >> KVM_ARM_IRQ_TYPE_SHIFT) & KVM_ARM_IRQ_TYPE_MASK,
        (irq >> KVM_ARM_IRQ_VCPU_SHIFT) & KVM_ARM_IRQ_VCPU_MASK,
        irq & KVM_ARM_IRQ_NUM_MASK,
    )
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvmCreateDevice {
    pub type_: u32,
    pub fd: u32,
    pub flags: u32,
}
pub const KVM_CREATE_DEVICE_TEST: u32 = 1;
pub const KVM_DEV_TYPE_ARM_VGIC_V2: u32 = 5;
pub const KVM_DEV_TYPE_ARM_VGIC_V3: u32 = 7;
pub const KVM_DEV_TYPE_ARM_VGIC_ITS: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvmDeviceAttr {
    pub flags: u32,
    pub group: u32,
    pub attr: u64,
    pub addr: u64,
}

/// vGIC device attribute groups (KVM_DEV_ARM_VGIC_GRP_*).
pub mod vgic {
    pub const GRP_ADDR: u32 = 0;
    pub const GRP_DIST_REGS: u32 = 1;
    /// v2: CPU interface registers.
    pub const GRP_CPU_REGS: u32 = 2;
    pub const GRP_NR_IRQS: u32 = 3;
    pub const GRP_CTRL: u32 = 4;
    /// v3: redistributor registers.
    pub const GRP_REDIST_REGS: u32 = 5;
    /// v3: ICC_* system registers.
    pub const GRP_CPU_SYSREGS: u32 = 6;
    pub const GRP_LEVEL_INFO: u32 = 7;
    pub const GRP_ITS_REGS: u32 = 8;
    pub const CTRL_INIT: u64 = 0;
    pub const CTRL_ITS_SAVE_TABLES: u64 = 1;
    pub const CTRL_ITS_RESTORE_TABLES: u64 = 2;
    pub const CTRL_ITS_RESET: u64 = 3;
    // GRP_ADDR attr values
    pub const V2_ADDR_TYPE_DIST: u64 = 0;
    pub const V2_ADDR_TYPE_CPU: u64 = 1;
    pub const V3_ADDR_TYPE_DIST: u64 = 2;
    pub const V3_ADDR_TYPE_REDIST: u64 = 3;
    pub const V3_ADDR_TYPE_ITS: u64 = 4;
    pub const V3_ADDR_TYPE_REDIST_REGION: u64 = 5;
    // GRP_DIST_REGS / GRP_CPU_REGS / GRP_REDIST_REGS attr encoding
    pub const ATTR_OFFSET_MASK: u64 = 0xffff_ffff;
    pub const V2_ATTR_MPIDR_SHIFT: u32 = 32;
    pub const V2_ATTR_MPIDR_MASK: u64 = 0xff << 32;
    /// v3 MPIDR affinity in attr[63:32] (Aff3:Aff2:Aff1:Aff0).
    pub const V3_ATTR_MPIDR_SHIFT: u32 = 32;
    pub const V3_ATTR_MPIDR_MASK: u64 = 0xffff_ffff << 32;
    /// GRP_LEVEL_INFO: attr[63:32] MPIDR, [31:0] intid.
    pub const LEVEL_INFO_LINE_LEVEL_SHIFT: u32 = 0;
    // GRP_CPU_SYSREGS attr encoding: mpidr<<32 | op0<<14|op1<<11|crn<<7|crm<<3|op2
    pub const SYSREG_INSTR_MASK: u64 = 0xffff;
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KvmIoeventfd {
    pub datamatch: u64,
    pub addr: u64,
    pub len: u32,
    pub fd: i32,
    pub flags: u32,
    pub pad: [u8; 36],
}
impl Default for KvmIoeventfd {
    fn default() -> Self {
        Self { datamatch: 0, addr: 0, len: 0, fd: -1, flags: 0, pad: [0; 36] }
    }
}
pub const KVM_IOEVENTFD_FLAG_DATAMATCH: u32 = 1 << 0;
pub const KVM_IOEVENTFD_FLAG_PIO: u32 = 1 << 1;
pub const KVM_IOEVENTFD_FLAG_DEASSIGN: u32 = 1 << 2;
pub const KVM_IOEVENTFD_FLAG_VIRTIO_CCW_NOTIFY: u32 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KvmIrqfd {
    pub fd: u32,
    pub gsi: u32,
    pub flags: u32,
    pub resamplefd: u32,
    pub pad: [u8; 16],
}
pub const KVM_IRQFD_FLAG_DEASSIGN: u32 = 1 << 0;
pub const KVM_IRQFD_FLAG_RESAMPLE: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KvmEnableCap {
    pub cap: u32,
    pub flags: u32,
    pub args: [u64; 4],
    pub pad: [u8; 64],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvmMpState {
    pub mp_state: u32,
}
impl Default for KvmEnableCap {
    fn default() -> Self {
        Self { cap: 0, flags: 0, args: [0; 4], pad: [0; 64] }
    }
}

pub const KVM_MP_STATE_RUNNABLE: u32 = 0;
pub const KVM_MP_STATE_UNINITIALIZED: u32 = 1;
pub const KVM_MP_STATE_INIT_RECEIVED: u32 = 2;
pub const KVM_MP_STATE_HALTED: u32 = 3;
pub const KVM_MP_STATE_STOPPED: u32 = 5;

// ---------------------------------------------------------------- kvm_run
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KvmRunMmio {
    pub phys_addr: u64,
    pub data: [u8; 8],
    pub len: u32,
    pub is_write: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KvmRunSystemEvent {
    pub type_: u32,
    pub ndata: u32,
    pub data: [u64; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union KvmRunExit {
    pub mmio: KvmRunMmio,
    pub system_event: KvmRunSystemEvent,
    pub padding: [u8; 256],
}

/// arm64 `kvm_run`; the `s` (sync regs) union is 2048 bytes, first field
/// `device_irq_level` (u64).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KvmRun {
    pub request_interrupt_window: u8,
    pub immediate_exit: u8,
    pub padding1: [u8; 6],
    pub exit_reason: u32,
    pub ready_for_interrupt_injection: u8,
    pub if_flag: u8,
    pub flags: u16,
    pub cr8: u64,
    pub apic_base: u64,
    pub exit: KvmRunExit,
    pub kvm_valid_regs: u64,
    pub kvm_dirty_regs: u64,
    pub s: [u64; 256],
}
impl KvmRun {
    pub const fn zeroed() -> Self {
        Self {
            request_interrupt_window: 0,
            immediate_exit: 0,
            padding1: [0; 6],
            exit_reason: 0,
            ready_for_interrupt_injection: 0,
            if_flag: 0,
            flags: 0,
            cr8: 0,
            apic_base: 0,
            exit: KvmRunExit { padding: [0; 256] },
            kvm_valid_regs: 0,
            kvm_dirty_regs: 0,
            s: [0; 256],
        }
    }
}
/// kvm_sync_regs.device_irq_level bits (KVM_ARM_DEV_*).
pub const KVM_ARM_DEV_EL1_VTIMER: u64 = 1 << 0;
pub const KVM_ARM_DEV_EL1_PTIMER: u64 = 1 << 1;
pub const KVM_ARM_DEV_PMU: u64 = 1 << 2;

// ---------------------------------------------------------------- exits
pub mod exit {
    pub const KVM_EXIT_UNKNOWN: u32 = 0;
    pub const KVM_EXIT_EXCEPTION: u32 = 1;
    pub const KVM_EXIT_IO: u32 = 2;
    pub const KVM_EXIT_HYPERCALL: u32 = 3;
    pub const KVM_EXIT_DEBUG: u32 = 4;
    pub const KVM_EXIT_HLT: u32 = 5;
    pub const KVM_EXIT_MMIO: u32 = 6;
    pub const KVM_EXIT_IRQ_WINDOW_OPEN: u32 = 7;
    pub const KVM_EXIT_SHUTDOWN: u32 = 8;
    pub const KVM_EXIT_FAIL_ENTRY: u32 = 9;
    pub const KVM_EXIT_INTR: u32 = 10;
    pub const KVM_EXIT_INTERNAL_ERROR: u32 = 17;
    pub const KVM_EXIT_SYSTEM_EVENT: u32 = 24;
    pub const KVM_EXIT_ARM_NISV: u32 = 28;

    pub const KVM_SYSTEM_EVENT_SHUTDOWN: u32 = 1;
    pub const KVM_SYSTEM_EVENT_RESET: u32 = 2;
    pub const KVM_SYSTEM_EVENT_CRASH: u32 = 3;
    pub const KVM_SYSTEM_EVENT_WAKEUP: u32 = 4;
    pub const KVM_SYSTEM_EVENT_SUSPEND: u32 = 5;

    // KVM_EXIT_INTERNAL_ERROR suberrors
    pub const KVM_INTERNAL_ERROR_EMULATION: u32 = 1;
    pub const KVM_INTERNAL_ERROR_SIMUL_EX: u32 = 2;
    pub const KVM_INTERNAL_ERROR_DELIVERY_EV: u32 = 3;
    pub const KVM_INTERNAL_ERROR_UNEXPECTED_EXIT_REASON: u32 = 4;
}

// ---------------------------------------------------------------- caps
pub mod cap {
    pub const KVM_CAP_IRQCHIP: u32 = 0;
    pub const KVM_CAP_USER_MEMORY: u32 = 3;
    pub const KVM_CAP_NR_VCPUS: u32 = 9;
    pub const KVM_CAP_NR_MEMSLOTS: u32 = 10;
    pub const KVM_CAP_MP_STATE: u32 = 14;
    pub const KVM_CAP_COALESCED_MMIO: u32 = 15;
    pub const KVM_CAP_SYNC_MMU: u32 = 16;
    pub const KVM_CAP_DESTROY_MEMORY_REGION_WORKS: u32 = 21;
    pub const KVM_CAP_SET_GUEST_DEBUG: u32 = 23;
    pub const KVM_CAP_IRQ_ROUTING: u32 = 25;
    pub const KVM_CAP_JOIN_MEMORY_REGIONS_WORKS: u32 = 30;
    pub const KVM_CAP_IRQFD: u32 = 32;
    pub const KVM_CAP_IOEVENTFD: u32 = 36;
    pub const KVM_CAP_INTERNAL_ERROR_DATA: u32 = 40;
    pub const KVM_CAP_VCPU_EVENTS: u32 = 41;
    pub const KVM_CAP_MAX_VCPUS: u32 = 66;
    pub const KVM_CAP_ONE_REG: u32 = 70;
    pub const KVM_CAP_READONLY_MEM: u32 = 81;
    pub const KVM_CAP_ARM_PSCI: u32 = 87;
    pub const KVM_CAP_DEVICE_CTRL: u32 = 89;
    pub const KVM_CAP_ARM_EL1_32BIT: u32 = 93;
    pub const KVM_CAP_ARM_PSCI_0_2: u32 = 102;
    pub const KVM_CAP_IOEVENTFD_ANY_LENGTH: u32 = 122;
    pub const KVM_CAP_ARM_PMU_V3: u32 = 126;
    pub const KVM_CAP_IMMEDIATE_EXIT: u32 = 136;
    pub const KVM_CAP_ARM_USER_IRQ: u32 = 144;
    pub const KVM_CAP_ARM_VM_IPA_SIZE: u32 = 165;
    pub const KVM_CAP_ARM_SVE: u32 = 170;
    pub const KVM_CAP_ARM_NISV_TO_USER: u32 = 177;
    pub const KVM_CAP_STEAL_TIME: u32 = 187;
    pub const KVM_CAP_DIRTY_LOG_RING: u32 = 192;
    pub const KVM_CAP_ARM_MTE: u32 = 205;
    pub const KVM_CAP_USER_MEMORY2: u32 = 231;
    pub const KVM_CAP_GUEST_MEMFD: u32 = 234;

    /// Value KVM_CHECK_EXTENSION reports for `cap` given `ipa_max`
    /// (the host max IPA bits, e.g. 40) and whether a vGIC exists.
    /// Caps not listed answer 0.
    pub const fn answer(cap: u32, ipa_max: u32, vgic: bool) -> i32 {
        match cap {
            KVM_CAP_USER_MEMORY
            | KVM_CAP_DESTROY_MEMORY_REGION_WORKS
            | KVM_CAP_JOIN_MEMORY_REGIONS_WORKS
            | KVM_CAP_INTERNAL_ERROR_DATA
            | KVM_CAP_IOEVENTFD
            | KVM_CAP_IOEVENTFD_ANY_LENGTH
            | KVM_CAP_DEVICE_CTRL
            | KVM_CAP_IMMEDIATE_EXIT
            | KVM_CAP_ONE_REG
            | KVM_CAP_ARM_PSCI
            | KVM_CAP_ARM_PSCI_0_2
            | KVM_CAP_ARM_USER_IRQ
            | KVM_CAP_READONLY_MEM
            | KVM_CAP_MP_STATE => 1,
            KVM_CAP_IRQCHIP | KVM_CAP_IRQFD => vgic as i32,
            KVM_CAP_NR_VCPUS | KVM_CAP_MAX_VCPUS => super::KVM_MAX_VCPUS as i32,
            KVM_CAP_NR_MEMSLOTS => super::KVM_NR_MEMSLOTS as i32,
            KVM_CAP_ARM_VM_IPA_SIZE => ipa_max as i32,
            _ => 0,
        }
    }
}

// ---------------------------------------------------------------- ONE_REG ids
pub const KVM_REG_ARM64: u64 = 0x6000_0000_0000_0000;
pub const KVM_REG_SIZE_SHIFT: u32 = 52;
pub const KVM_REG_SIZE_MASK: u64 = 0x00f0_0000_0000_0000;
pub const KVM_REG_ARM_COPROC_SHIFT: u32 = 16;
pub const KVM_REG_ARM_COPROC_MASK: u64 = 0x0fff_0000;
pub const KVM_REG_ARM_CORE: u64 = 0x0010 << 16;
pub const KVM_REG_ARM_DEMUX: u64 = 0x0011 << 16;
pub const KVM_REG_ARM64_SYSREG: u64 = 0x0013 << 16;
pub const KVM_REG_ARM_FW: u64 = 0x0014 << 16;
pub const KVM_REG_ARM_DEMUX_ID_SHIFT: u32 = 8;
pub const KVM_REG_ARM_DEMUX_ID_CCSIDR: u64 = 0x00 << 8;
pub const KVM_REG_ARM_DEMUX_VAL_MASK: u64 = 0xff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
/// Size code stored in id bits 55:52 (KVM_REG_SIZE_U8..U256 = code << 52).
pub enum RegSize {
    U8 = 0,
    U16 = 1,
    U32 = 2,
    U64 = 3,
    U128 = 4,
    U256 = 5,
}
impl RegSize {
    pub const fn from_bits(v: u64) -> Option<RegSize> {
        match v {
            0 => Some(RegSize::U8),
            1 => Some(RegSize::U16),
            2 => Some(RegSize::U32),
            3 => Some(RegSize::U64),
            4 => Some(RegSize::U128),
            5 => Some(RegSize::U256),
            _ => None,
        }
    }
    pub const fn bytes(self) -> usize {
        match self {
            RegSize::U8 => 1,
            RegSize::U16 => 2,
            RegSize::U32 => 4,
            RegSize::U64 => 8,
            RegSize::U128 => 16,
            RegSize::U256 => 32,
        }
    }
}

/// `KVM_REG_ARM64 | size | KVM_REG_ARM_CORE | off_words`, off_words =
/// offsetof(struct kvm_regs, field) / 4.
pub const fn core_reg(off_words: u64, size: RegSize) -> u64 {
    KVM_REG_ARM64 | ((size as u64) << KVM_REG_SIZE_SHIFT) | KVM_REG_ARM_CORE | off_words
}

/// Packed sysreg instruction encoding (op0<<14|op1<<11|crn<<7|crm<<3|op2).
pub const fn sysreg_enc(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}
/// Full ONE_REG id of a 64-bit system register.
pub const fn sysreg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    KVM_REG_ARM64
        | ((RegSize::U64 as u64) << KVM_REG_SIZE_SHIFT)
        | KVM_REG_ARM64_SYSREG
        | sysreg_enc(op0, op1, crn, crm, op2)
}
pub const fn fw_reg(n: u64) -> u64 {
    KVM_REG_ARM64 | ((RegSize::U64 as u64) << KVM_REG_SIZE_SHIFT) | KVM_REG_ARM_FW | n
}
/// `KVM_REG_ARM_DEMUX` CCSIDR entry (U32) for CSSELR value `val`.
pub const fn ccsidr_reg(val: u64) -> u64 {
    KVM_REG_ARM64
        | ((RegSize::U32 as u64) << KVM_REG_SIZE_SHIFT)
        | KVM_REG_ARM_DEMUX
        | KVM_REG_ARM_DEMUX_ID_CCSIDR
        | (val & KVM_REG_ARM_DEMUX_VAL_MASK)
}

/// kvm_regs layout in 32-bit words.
pub mod coreregs {
    use super::*;
    pub const fn x(i: u64) -> u64 {
        core_reg(2 * i, RegSize::U64)
    }
    pub const SP: u64 = core_reg(0x3e, RegSize::U64);
    pub const PC: u64 = core_reg(0x40, RegSize::U64);
    pub const PSTATE: u64 = core_reg(0x42, RegSize::U64);
    pub const SP_EL1: u64 = core_reg(0x44, RegSize::U64);
    pub const ELR_EL1: u64 = core_reg(0x46, RegSize::U64);
    pub const fn spsr(i: u64) -> u64 {
        core_reg(0x48 + 2 * i, RegSize::U64)
    }
    pub const fn vreg(i: u64) -> u64 {
        core_reg(0x54 + 4 * i, RegSize::U128)
    }
    pub const FPSR: u64 = core_reg(0xd4, RegSize::U32);
    pub const FPCR: u64 = core_reg(0xd5, RegSize::U32);
    /// Highest valid word offset + 1 (sizeof kvm_regs / 4).
    pub const WORDS: u64 = 0xd6;
}

/// Named system registers exposed through KVM_GET_REG_LIST.
pub mod sys {
    use super::sysreg;
    pub const SCTLR_EL1: u64 = sysreg(3, 0, 1, 0, 0);
    pub const CPACR_EL1: u64 = sysreg(3, 0, 1, 0, 2);
    pub const TTBR0_EL1: u64 = sysreg(3, 0, 2, 0, 0);
    pub const TTBR1_EL1: u64 = sysreg(3, 0, 2, 0, 1);
    pub const TCR_EL1: u64 = sysreg(3, 0, 2, 0, 2);
    pub const ESR_EL1: u64 = sysreg(3, 0, 5, 2, 0);
    pub const AFSR0_EL1: u64 = sysreg(3, 0, 5, 1, 0);
    pub const AFSR1_EL1: u64 = sysreg(3, 0, 5, 1, 1);
    pub const FAR_EL1: u64 = sysreg(3, 0, 6, 0, 0);
    pub const PAR_EL1: u64 = sysreg(3, 0, 7, 4, 0);
    pub const MAIR_EL1: u64 = sysreg(3, 0, 10, 2, 0);
    pub const AMAIR_EL1: u64 = sysreg(3, 0, 10, 3, 0);
    pub const VBAR_EL1: u64 = sysreg(3, 0, 12, 0, 0);
    pub const CONTEXTIDR_EL1: u64 = sysreg(3, 0, 13, 0, 1);
    pub const TPIDR_EL1: u64 = sysreg(3, 0, 13, 0, 4);
    pub const TPIDR_EL0: u64 = sysreg(3, 3, 13, 0, 2);
    pub const TPIDRRO_EL0: u64 = sysreg(3, 3, 13, 0, 3);
    pub const CSSELR_EL1: u64 = sysreg(3, 2, 0, 0, 0);
    pub const CNTKCTL_EL1: u64 = sysreg(3, 0, 14, 1, 0);
    pub const MDSCR_EL1: u64 = sysreg(2, 0, 0, 2, 2);
    pub const MPIDR_EL1: u64 = sysreg(3, 0, 0, 0, 5);
    // Timer registers. NOTE: the UAPI encodes CNT and CVAL swapped relative to
    // the architectural CNTV_*/CNTP_* names; reproduced verbatim.
    pub const TIMER_CTL: u64 = sysreg(3, 3, 14, 3, 1);
    pub const TIMER_CNT: u64 = sysreg(3, 3, 14, 3, 2);
    pub const TIMER_CVAL: u64 = sysreg(3, 3, 14, 0, 2);
    pub const PTIMER_CTL: u64 = sysreg(3, 3, 14, 2, 1);
    pub const PTIMER_CVAL: u64 = sysreg(3, 3, 14, 2, 2);
    pub const PTIMER_CNT: u64 = sysreg(3, 3, 14, 0, 1);
    // ID registers probed by QEMU (GET-only).
    pub const ID_AA64PFR0_EL1: u64 = sysreg(3, 0, 0, 4, 0);
    pub const ID_AA64PFR1_EL1: u64 = sysreg(3, 0, 0, 4, 1);
    pub const ID_AA64ZFR0_EL1: u64 = sysreg(3, 0, 0, 4, 4);
    pub const ID_AA64DFR0_EL1: u64 = sysreg(3, 0, 0, 5, 0);
    pub const ID_AA64DFR1_EL1: u64 = sysreg(3, 0, 0, 5, 1);
    pub const ID_AA64ISAR0_EL1: u64 = sysreg(3, 0, 0, 6, 0);
    pub const ID_AA64ISAR1_EL1: u64 = sysreg(3, 0, 0, 6, 1);
    pub const ID_AA64MMFR0_EL1: u64 = sysreg(3, 0, 0, 7, 0);
    pub const ID_AA64MMFR1_EL1: u64 = sysreg(3, 0, 0, 7, 1);
    pub const ID_AA64MMFR2_EL1: u64 = sysreg(3, 0, 0, 7, 2);
}

/// Firmware pseudo registers (KVM_REG_ARM_FW).
pub const KVM_REG_ARM_PSCI_VERSION: u64 = fw_reg(0);

/// Registers returned by KVM_GET_REG_LIST (excluding core regs and CCSIDR
/// demux, which the lane that implements the list adds).
pub const SYSREG_LIST: [u64; 24] = [
    sys::SCTLR_EL1, sys::CPACR_EL1, sys::TTBR0_EL1, sys::TTBR1_EL1, sys::TCR_EL1,
    sys::ESR_EL1, sys::AFSR0_EL1, sys::AFSR1_EL1, sys::FAR_EL1, sys::MAIR_EL1,
    sys::AMAIR_EL1, sys::VBAR_EL1, sys::CONTEXTIDR_EL1, sys::TPIDR_EL1,
    sys::TPIDR_EL0, sys::TPIDRRO_EL0, sys::PAR_EL1, sys::CSSELR_EL1,
    sys::CNTKCTL_EL1, sys::MDSCR_EL1, sys::MPIDR_EL1, sys::TIMER_CTL,
    sys::TIMER_CNT, sys::TIMER_CVAL,
];

/// A decoded ONE_REG id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegId {
    /// kvm_regs member at `off_words` (offsetof/4).
    Core { off_words: u64, size: RegSize },
    Sysreg { op0: u8, op1: u8, crn: u8, crm: u8, op2: u8 },
    /// CCSIDR demux entry for CSSELR value `val`.
    Ccsidr { val: u8 },
    Fw { n: u64 },
}

/// Decode an arm64 ONE_REG id. None for non-arm64 arch, bad size, reserved
/// bits set, or an unknown coproc class.
pub const fn decode_reg(id: u64) -> Option<RegId> {
    if id & 0xff00_0000_0000_0000 != KVM_REG_ARM64 {
        return None;
    }
    let size = match RegSize::from_bits((id & KVM_REG_SIZE_MASK) >> KVM_REG_SIZE_SHIFT) {
        Some(s) => s,
        None => return None,
    };
    // bits 51:28 are reserved, bits 27:16 coproc, 15:0 payload
    if id & 0x000f_ffff_f000_0000 != 0 {
        return None;
    }
    let low = id & 0xffff;
    match id & KVM_REG_ARM_COPROC_MASK {
        KVM_REG_ARM_CORE => Some(RegId::Core { off_words: low, size }),
        KVM_REG_ARM64_SYSREG => Some(RegId::Sysreg {
            op0: ((low >> 14) & 3) as u8,
            op1: ((low >> 11) & 7) as u8,
            crn: ((low >> 7) & 0xf) as u8,
            crm: ((low >> 3) & 0xf) as u8,
            op2: (low & 7) as u8,
        }),
        KVM_REG_ARM_DEMUX => {
            if low >> 8 != 0 {
                return None; // only ID 0 (CCSIDR)
            }
            Some(RegId::Ccsidr { val: (low & 0xff) as u8 })
        }
        KVM_REG_ARM_FW => Some(RegId::Fw { n: low }),
        _ => None,
    }
}

// ---------------------------------------------------------------- layout asserts (Linux sizes/offsets)
const _: () = {
    assert!(size_of::<KvmUserspaceMemoryRegion>() == 32);
    assert!(size_of::<KvmOneReg>() == 16);
    assert!(size_of::<KvmRegList>() == 8);
    assert!(size_of::<KvmVcpuInit>() == 32);
    assert!(size_of::<KvmIrqLevel>() == 8);
    assert!(size_of::<KvmCreateDevice>() == 12);
    assert!(size_of::<KvmDeviceAttr>() == 24);
    assert!(size_of::<KvmIoeventfd>() == 64);
    assert!(offset_of!(KvmIoeventfd, len) == 16);
    assert!(offset_of!(KvmIoeventfd, fd) == 20);
    assert!(offset_of!(KvmIoeventfd, flags) == 24);
    assert!(size_of::<KvmIrqfd>() == 32);
    assert!(size_of::<KvmEnableCap>() == 104);
    assert!(size_of::<KvmMpState>() == 4);
    assert!(size_of::<KvmRunMmio>() == 24);
    assert!(offset_of!(KvmRunMmio, data) == 8);
    assert!(offset_of!(KvmRunMmio, len) == 16);
    assert!(offset_of!(KvmRunMmio, is_write) == 20);
    assert!(size_of::<KvmRunSystemEvent>() == 136);
    assert!(size_of::<KvmRunExit>() == 256);
    assert!(offset_of!(KvmRun, request_interrupt_window) == 0);
    assert!(offset_of!(KvmRun, immediate_exit) == 1);
    assert!(offset_of!(KvmRun, exit_reason) == 8);
    assert!(offset_of!(KvmRun, ready_for_interrupt_injection) == 12);
    assert!(offset_of!(KvmRun, flags) == 14);
    assert!(offset_of!(KvmRun, cr8) == 16);
    assert!(offset_of!(KvmRun, apic_base) == 24);
    assert!(offset_of!(KvmRun, exit) == 32);
    assert!(offset_of!(KvmRun, kvm_valid_regs) == 288);
    assert!(offset_of!(KvmRun, kvm_dirty_regs) == 296);
    assert!(offset_of!(KvmRun, s) == 304);
    assert!(size_of::<KvmRun>() == 2352);
    assert!(size_of::<KvmRun>() <= KVM_VCPU_MMAP_SIZE);
};
