//! MMIO completion: finish a trapped guest load once userspace supplied the data.

use crate::hw::VcpuHw;

/// Value a load of `1<<sas` bytes leaves in the target register: truncate to
/// the access size, optionally sign-extend to 64 bits, then to 32 bits (zero
/// upper half) when the destination is a Wt register (`sf` false).
pub const fn load_value(sse: bool, sf: bool, sas: u8, data: u64) -> u64 {
    let bits = 8u32 << (sas & 3);
    let mut v = if bits == 64 { data } else { data & ((1u64 << bits) - 1) };
    if sse && bits < 64 && v & (1u64 << (bits - 1)) != 0 {
        v |= !0u64 << bits;
    }
    if !sf { v &= 0xFFFF_FFFF; }
    v
}

/// Complete an MMIO read: write the result into guest register `rt` (31 = XZR, discarded).
pub fn finish_read(hw: &mut VcpuHw, rt: u8, sse: bool, sf: bool, sas: u8, data: u64) {
    if rt < 31 {
        hw.gp[rt as usize] = load_value(sse, sf, sas, data);
    }
}

/// Step the guest PC over the trapped instruction (`il` = ESR.IL instruction length in bytes).
pub fn advance_pc(hw: &mut VcpuHw, insn_len: u64) {
    hw.pc = hw.pc.wrapping_add(insn_len);
}
