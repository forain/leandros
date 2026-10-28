//! Gen9 DDI buffer translation tables, pure (host-tested in
//! drivers/i915-tests). COPIED from Linux intel_ddi_buf_trans.c
//! (`_skl_trans_hdmi`, `_kbl_u_trans_dp`, `_skl_trans_dp`); do not edit
//! values without the Linux source in hand.

/// One DDI_BUF_TRANS entry: the two dwords (LO, HI) and the I_boost
/// (balance leg) level (0: DISPIO balance leg disabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry { pub lo: u32, pub hi: u32, pub iboost: u32 }

const fn e(lo: u32, hi: u32, iboost: u32) -> Entry { Entry { lo, hi, iboost } }

/// Skylake/Kabylake U, H and S HDMI (`_skl_trans_hdmi`).
pub const SKL_HDMI: [Entry; 11] = [
    e(0x0000_0018, 0x0000_00AC, 0x0),
    e(0x0000_5012, 0x0000_009D, 0x0),
    e(0x0000_7011, 0x0000_0088, 0x0),
    e(0x0000_0018, 0x0000_00A1, 0x0),
    e(0x0000_0018, 0x0000_0098, 0x0),
    e(0x0000_4013, 0x0000_0088, 0x0),
    e(0x8000_6012, 0x0000_00CD, 0x1),
    e(0x0000_0018, 0x0000_00DF, 0x0),
    e(0x8000_3015, 0x0000_00CD, 0x1), // default
    e(0x8000_3015, 0x0000_00C0, 0x1),
    e(0x8000_0018, 0x0000_00C0, 0x1),
];
/// `.hdmi_default_entry` of `skl_trans_hdmi`.
pub const SKL_HDMI_DEFAULT: usize = 8;

/// Kabylake U DP (`_kbl_u_trans_dp`).
pub const KBL_U_DP: [Entry; 9] = [
    e(0x0000_201B, 0x0000_00A1, 0x0),
    e(0x0000_5012, 0x0000_0088, 0x0),
    e(0x8000_7011, 0x0000_00CD, 0x3),
    e(0x8000_9010, 0x0000_00C0, 0x3),
    e(0x0000_201B, 0x0000_009D, 0x0),
    e(0x8000_5012, 0x0000_00C0, 0x3),
    e(0x8000_7011, 0x0000_00C0, 0x3),
    e(0x0000_2016, 0x0000_004F, 0x0),
    e(0x8000_5012, 0x0000_00C0, 0x3),
];

/// Skylake H and S DP (`_skl_trans_dp`).
pub const SKL_DP: [Entry; 9] = [
    e(0x0000_2016, 0x0000_00A0, 0x0),
    e(0x0000_5012, 0x0000_009B, 0x0),
    e(0x0000_7011, 0x0000_0088, 0x0),
    e(0x8000_9010, 0x0000_00C0, 0x1),
    e(0x0000_2016, 0x0000_009B, 0x0),
    e(0x0000_5012, 0x0000_0088, 0x0),
    e(0x8000_7011, 0x0000_00C0, 0x1),
    e(0x0000_2016, 0x0000_00DF, 0x0),
    e(0x8000_5012, 0x0000_00C0, 0x1),
];

/// The hardware slot HDMI uses on gen9 (Linux writes the chosen level there).
pub const HDMI_HW_ENTRY: u32 = 9;

/// DDI_BUF_TRANS(port) LO/HI register offsets for hardware entry `n`.
pub const fn lo_reg(port: u32, n: u32) -> u32 { 0x64E00 + 0x60 * port + 8 * n }
pub const fn hi_reg(port: u32, n: u32) -> u32 { lo_reg(port, n) + 4 }

/// The HDMI entry for a VBT hdmi_level_shifter (`None` / out of range:
/// Linux's default entry).
pub fn hdmi_entry(vbt_level: Option<u32>) -> (usize, Entry) {
    let i = match vbt_level { Some(l) if (l as usize) < SKL_HDMI.len() => l as usize, _ => SKL_HDMI_DEFAULT };
    (i, SKL_HDMI[i])
}

/// DISPIO_CR_TX_BMU_CR0 (skl_ddi_set_iboost): per-port balance-leg field
/// (3 bits at 8 + 3 * port) and disable bit (31 - port). Returns the new
/// register value given the old one.
pub const DISPIO_CR_TX_BMU_CR0: u32 = 0x6C00C;
pub fn iboost_value(old: u32, port: u32, iboost: u32) -> u32 {
    let shift = 8 + 3 * port;
    let mut v = old & !(7 << shift) & !(1 << (31 - port));
    if iboost != 0 { v |= (iboost & 7) << shift; } else { v |= 1 << (31 - port); }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdmi_default_is_entry_8() {
        let (i, en) = hdmi_entry(None);
        assert_eq!(i, 8);
        assert_eq!((en.lo, en.hi, en.iboost), (0x8000_3015, 0xCD, 1));
        assert_eq!(hdmi_entry(Some(0)).1, e(0x18, 0xAC, 0));
        assert_eq!(hdmi_entry(Some(10)).1, e(0x8000_0018, 0xC0, 1));
        assert_eq!(hdmi_entry(Some(11)).0, 8);
    }

    #[test]
    fn linux_table_shapes() {
        assert_eq!(SKL_HDMI.len(), 11);
        assert_eq!(KBL_U_DP.len(), 9);
        // Every iboost entry has the swing-select bit 31 set in LO, and none without it does.
        for t in [&SKL_HDMI[..], &KBL_U_DP[..], &SKL_DP[..]] {
            for en in t { assert!(en.lo >> 31 == 1 || en.iboost == 0, "{:x?}", en); }
        }
        assert_eq!(KBL_U_DP[2], e(0x8000_7011, 0xCD, 3));
        assert_eq!(SKL_DP[3], e(0x8000_9010, 0xC0, 1));
    }

    #[test]
    fn register_offsets() {
        assert_eq!(lo_reg(0, 0), 0x64E00);
        assert_eq!(lo_reg(1, HDMI_HW_ENTRY), 0x64E00 + 0x60 + 72);
        assert_eq!(hi_reg(1, HDMI_HW_ENTRY), lo_reg(1, 9) + 4);
        assert_eq!(lo_reg(2, 0), 0x64EC0);
    }

    #[test]
    fn iboost_field() {
        // port B (1): field at bit 11, disable at bit 30.
        assert_eq!(iboost_value(0, 1, 1), 1 << 11);
        assert_eq!(iboost_value(0xFFFF_FFFF, 1, 1), 0xFFFF_FFFF & !(7 << 11) & !(1 << 30) | 1 << 11);
        assert_eq!(iboost_value(0, 1, 0), 1 << 30);
        // other ports untouched
        assert_eq!(iboost_value(1 << 14, 1, 3) & (7 << 14), 1 << 14);
    }
}
