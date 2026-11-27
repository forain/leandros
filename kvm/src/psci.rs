//! In-kernel PSCI 1.0 (plus SMCCC 1.1 version query) for HVC/SMC traps.
//!
//! `handle` gets the function id in `x[0]` and arguments in `x[1..4]`, and
//! leaves the result in `x[0]` (x[1..3] are preserved: PSCI returns only w0/x0).


pub const PSCI_VERSION: u64 = 0x8400_0000;
pub const PSCI_CPU_SUSPEND: u64 = 0xC400_0001; // 32-bit alias 0x84000001
pub const PSCI_CPU_OFF: u64 = 0x8400_0002;
pub const PSCI_CPU_ON: u64 = 0xC400_0003; // 32-bit alias 0x84000003
pub const PSCI_AFFINITY_INFO: u64 = 0xC400_0004; // 32-bit alias 0x84000004
pub const PSCI_MIGRATE_INFO_TYPE: u64 = 0x8400_0006;
pub const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
pub const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;
pub const PSCI_FEATURES: u64 = 0x8400_000A;
pub const SMCCC_VERSION: u64 = 0x8000_0000;
pub const SMCCC_ARCH_FEATURES: u64 = 0x8000_0001;

pub const PSCI_VERSION_1_0: u64 = 0x0001_0000;
pub const SMCCC_VERSION_1_1: u64 = 0x0001_0001;

pub const SUCCESS: i64 = 0;
pub const NOT_SUPPORTED: i64 = -1;
pub const INVALID_PARAMS: i64 = -2;
pub const DENIED: i64 = -3;
pub const ALREADY_ON: i64 = -4;
pub const ON_PENDING: i64 = -5;
pub const INTERNAL_FAILURE: i64 = -6;
pub const NOT_PRESENT: i64 = -7;
pub const DISABLED: i64 = -8;
pub const INVALID_ADDRESS: i64 = -9;

/// Power state of one vcpu as PSCI sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuState {
    On,
    Off,
    OnPending,
}

/// What the VM layer provides to PSCI.
pub trait PsciVm {
    fn nr_vcpus(&self) -> u32;
    fn vcpu_mpidr(&self, vcpu: u32) -> u64;
    fn vcpu_state(&self, vcpu: u32) -> CpuState;
    /// Power on `target` at `entry` with x0 = `ctx`. Err is a PSCI error code
    /// (e.g. ALREADY_ON when it lost a race).
    fn cpu_on(&self, target: u32, entry: u64, ctx: u64) -> Result<(), i64>;
}

/// What the run loop must do after the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsciOut {
    /// Resume the guest; x[0] holds the result.
    Resume,
    /// CPU_SUSPEND: act as WFI, then resume (x[0] = SUCCESS).
    Wfi,
    /// CPU_OFF: this vcpu goes to the Off state; does not return to the guest.
    CpuOff,
    /// Exit to userspace with KVM_SYSTEM_EVENT_SHUTDOWN.
    SystemOff,
    /// Exit to userspace with KVM_SYSTEM_EVENT_RESET.
    SystemReset,
}

/// Affinity fields compared by CPU_ON / AFFINITY_INFO (Aff3:Aff2:Aff1:Aff0).
pub const MPIDR_AFF_MASK: u64 = 0x0000_00ff_00ff_ffff;

fn aff_mask(level: u64) -> u64 {
    // level 0 = all four fields, level 1 = Aff3:Aff2:Aff1, 2 = Aff3:Aff2, 3 = Aff3
    match level {
        0 => MPIDR_AFF_MASK,
        1 => MPIDR_AFF_MASK & !0xff,
        2 => MPIDR_AFF_MASK & !0xffff,
        _ => MPIDR_AFF_MASK & !0xff_ffff,
    }
}

fn find(vm: &dyn PsciVm, mpidr: u64) -> Option<u32> {
    (0..vm.nr_vcpus()).find(|&i| vm.vcpu_mpidr(i) & MPIDR_AFF_MASK == mpidr & MPIDR_AFF_MASK)
}

fn supported(fid: u64) -> bool {
    matches!(
        fid,
        0x8400_0000 | 0x8400_0001 | 0xC400_0001 | 0x8400_0002 | 0x8400_0003 | 0xC400_0003
            | 0x8400_0004 | 0xC400_0004 | 0x8400_0006 | 0x8400_0008 | 0x8400_0009 | 0x8400_000A
            | 0x8000_0000 | 0x8000_0001
    )
}

/// Handle one PSCI/SMCCC call. `vcpu` is the calling vcpu index.
pub fn handle(vm: &dyn PsciVm, vcpu: u32, x: &mut [u64; 4]) -> PsciOut {
    let fid = x[0] & 0xffff_ffff;
    // SMC32 calling convention: arguments are the low 32 bits.
    let is64 = fid & 0x4000_0000 != 0;
    let arg = |i: usize| if is64 { x[i] } else { x[i] & 0xffff_ffff };
    let (a1, a2, a3) = (arg(1), arg(2), arg(3));
    let base = fid & !0x4000_0000; // fold the 32/64 aliases together

    let mut out = PsciOut::Resume;
    // The SMC64 alias exists only for CPU_SUSPEND, CPU_ON and AFFINITY_INFO.
    let base = if is64 && !matches!(base, 0x8400_0001 | 0x8400_0003 | 0x8400_0004) { 0 } else { base };
    let ret: i64 = match base {
        0x8400_0000 => PSCI_VERSION_1_0 as i64,
        0x8400_0001 => {
            out = PsciOut::Wfi;
            SUCCESS
        }
        0x8400_0002 => {
            out = PsciOut::CpuOff;
            SUCCESS // never observed by the guest
        }
        0x8400_0003 => match find(vm, a1) {
            None => INVALID_PARAMS,
            Some(t) => match vm.vcpu_state(t) {
                CpuState::On => ALREADY_ON,
                CpuState::OnPending => ON_PENDING,
                CpuState::Off => match vm.cpu_on(t, a2, a3) {
                    Ok(()) => SUCCESS,
                    Err(e) => e,
                },
            },
        },
        0x8400_0004 => {
            if a2 > 3 {
                INVALID_PARAMS
            } else {
                let m = aff_mask(a2);
                let mut any = false;
                let mut state = 1; // OFF
                for i in 0..vm.nr_vcpus() {
                    if vm.vcpu_mpidr(i) & m == a1 & m {
                        any = true;
                        match vm.vcpu_state(i) {
                            CpuState::On => {
                                state = 0;
                                break;
                            }
                            CpuState::OnPending => state = 2,
                            CpuState::Off => {}
                        }
                    }
                }
                if any { state } else { INVALID_PARAMS }
            }
        }
        0x8400_0006 => 2, // Trusted OS not present / not migrate capable
        0x8400_0008 => {
            out = PsciOut::SystemOff;
            SUCCESS
        }
        0x8400_0009 => {
            out = PsciOut::SystemReset;
            SUCCESS
        }
        0x8400_000A => {
            if supported(a1 & 0xffff_ffff) { SUCCESS } else { NOT_SUPPORTED }
        }
        0x8000_0000 => SMCCC_VERSION_1_1 as i64,
        0x8000_0001 => {
            // ARCH_FEATURES: only the two functions we implement; the
            // ARCH_WORKAROUND_* calls are not provided.
            if matches!(a1 & 0xffff_ffff, 0x8000_0000 | 0x8000_0001) { SUCCESS } else { NOT_SUPPORTED }
        }
        _ => NOT_SUPPORTED,
    };
    let _ = vcpu;
    x[0] = if is64 { ret as u64 } else { (ret as i32) as u32 as u64 };
    out
}
