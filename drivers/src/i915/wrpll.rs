//! Gen9 (SKL/KBL) HDMI WRPLL divider search, pure (host-tested in
//! drivers/i915-tests). Ported verbatim from Linux intel_dpll_mgr.c
//! skl_ddi_calculate_wrpll / skl_wrpll_get_multipliers /
//! skl_wrpll_params_populate, plus the CFGCR1/CFGCR2 packing of
//! skl_ddi_hdmi_pll_dividers.

/// DPLL reference clock of SKL/KBL, kHz.
pub const REF_KHZ: u32 = 24_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub dco_fraction: u32,
    pub dco_integer: u32,
    pub qdiv_ratio: u32,
    pub qdiv_mode: u32,
    pub kdiv: u32,
    pub pdiv: u32,
    pub central_freq: u32,
    /// The chosen overall divider and its split (p = p0 * p1 * p2).
    pub p: u32,
    pub p0: u32,
    pub p1: u32,
    pub p2: u32,
    /// DCO frequency, kHz.
    pub dco_khz: u64,
}

impl Params {
    /// DPLL_CFGCR1: FREQ_ENABLE | DCO_FRACTION << 9 | DCO_INTEGER.
    pub fn cfgcr1(&self) -> u32 { 1 << 31 | self.dco_fraction << 9 | self.dco_integer }
    /// DPLL_CFGCR2: QDIV_RATIO << 8 | QDIV_MODE << 7 | KDIV << 5 | PDIV << 2 | CENTRAL_FREQ.
    pub fn cfgcr2(&self) -> u32 {
        self.qdiv_ratio << 8 | self.qdiv_mode << 7 | self.kdiv << 5 | self.pdiv << 2 | self.central_freq
    }
}

/// DCO freq must be within +1%/-6% of the DCO central freq (units of 0.01%).
const MAX_PDEVIATION: u64 = 100;
const MAX_NDEVIATION: u64 = 600;

struct Ctx { min_deviation: u64, central_freq: u64, dco_freq: u64, p: u32 }

fn try_divider(ctx: &mut Ctx, central_freq: u64, dco_freq: u64, divider: u32) {
    let deviation = 10000 * dco_freq.abs_diff(central_freq) / central_freq;
    let ok = if dco_freq >= central_freq { deviation < MAX_PDEVIATION } else { deviation < MAX_NDEVIATION };
    if ok && deviation < ctx.min_deviation {
        ctx.min_deviation = deviation;
        ctx.central_freq = central_freq;
        ctx.dco_freq = dco_freq;
        ctx.p = divider;
    }
}

/// `(p0, p1, p2)` of a divider; `(0, 0, 0)` if none (never for the lists below).
pub fn multipliers(p: u32) -> (u32, u32, u32) {
    if p % 2 == 0 {
        let half = p / 2;
        if half == 1 || half == 2 || half == 3 || half == 5 { (2, 1, half) }
        else if half % 2 == 0 { (2, half / 2, 2) }
        else if half % 3 == 0 { (3, half / 3, 2) }
        else if half % 7 == 0 { (7, half / 7, 2) }
        else { (0, 0, 0) }
    } else if p == 3 || p == 9 { (3, 1, p / 3) }
    else if p == 5 || p == 7 { (p, 1, 1) }
    else if p == 15 { (3, 1, 5) }
    else if p == 21 { (7, 1, 3) }
    else if p == 35 { (7, 1, 5) }
    else { (0, 0, 0) }
}

/// Dividers for an AFE clock of 5 * `clock_khz` (the pixel clock, kHz).
/// `None`: no divider puts the DCO in a central frequency's window.
pub fn calculate(clock_khz: u32, ref_khz: u32) -> Option<Params> {
    const DCO_CENTRAL: [u64; 3] = [8_400_000_000, 9_000_000_000, 9_600_000_000];
    const EVEN: [u32; 36] = [4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 52, 54, 56, 60, 64, 66, 68,
        70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98];
    const ODD: [u32; 7] = [3, 5, 7, 9, 15, 21, 35];
    if clock_khz == 0 || ref_khz < 1000 { return None; }
    let mut ctx = Ctx { min_deviation: u64::MAX, central_freq: 0, dco_freq: 0, p: 0 };
    let afe_clock = clock_khz as u64 * 1000 * 5; // Hz
    'lists: for (d, list) in [&EVEN[..], &ODD[..]].iter().enumerate() {
        'dco: for &central in DCO_CENTRAL.iter() {
            for &p in list.iter() {
                try_divider(&mut ctx, central, p as u64 * afe_clock, p);
                if ctx.min_deviation == 0 { break 'dco; }
            }
        }
        // If a solution is found with an even divider, prefer this one.
        if d == 0 && ctx.p != 0 { break 'lists; }
    }
    if ctx.p == 0 { return None; }
    let (p0, p1, p2) = multipliers(ctx.p);
    let central_freq = match ctx.central_freq { 9_600_000_000 => 0, 9_000_000_000 => 1, _ => 3 };
    let pdiv = match p0 { 1 => 0, 2 => 1, 3 => 2, 7 => 4, _ => return None };
    let kdiv = match p2 { 5 => 0, 2 => 1, 3 => 2, 1 => 3, _ => return None };
    let dco_freq = (p0 * p1 * p2) as u64 * afe_clock;
    let dco_integer = (dco_freq / (ref_khz as u64 * 1000)) as u32;
    let frac_num = dco_freq / (ref_khz as u64 / 1000) - dco_integer as u64 * 1_000_000;
    let dco_fraction = (frac_num * 0x8000 / 1_000_000) as u32;
    Some(Params { dco_fraction, dco_integer, qdiv_ratio: p1, qdiv_mode: (p1 != 1) as u32, kdiv, pdiv, central_freq,
                  p: ctx.p, p0, p1, p2, dco_khz: dco_freq / 1000 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deviation_ok(p: &Params) -> bool {
        let central = match p.central_freq { 0 => 9_600_000u64, 1 => 9_000_000, _ => 8_400_000 };
        let d = 10000 * p.dco_khz.abs_diff(central) / central;
        if p.dco_khz >= central { d < 100 } else { d < 600 }
    }

    #[test]
    fn hdmi_1080p60_148500() {
        let p = calculate(148_500, REF_KHZ).unwrap();
        // 5 * 148.5 MHz * 12 = 8910 MHz, 1 % below the 9000 MHz central frequency.
        assert_eq!((p.p, p.p0, p.p1, p.p2), (12, 2, 3, 2));
        assert_eq!(p.dco_khz, 8_910_000);
        assert_eq!((p.central_freq, p.pdiv, p.kdiv, p.qdiv_ratio, p.qdiv_mode), (1, 1, 1, 3, 1));
        // 8910 / 24 = 371.25 -> integer 371, fraction .25 * 0x8000
        assert_eq!((p.dco_integer, p.dco_fraction), (371, 0x2000));
        assert_eq!(p.cfgcr1(), 0x8000_0000 | 0x2000 << 9 | 371);
        assert_eq!(p.cfgcr2(), 3 << 8 | 1 << 7 | 1 << 5 | 1 << 2 | 1);
    }

    #[test]
    fn hdmi_720p60_74250() {
        let p = calculate(74_250, REF_KHZ).unwrap();
        assert_eq!((p.p, p.dco_khz), (24, 8_910_000));
        assert_eq!((p.dco_integer, p.dco_fraction), (371, 0x2000));
    }

    #[test]
    fn hdmi_108000() {
        let p = calculate(108_000, REF_KHZ).unwrap();
        assert_eq!((p.p, p.dco_khz), (16, 8_640_000));
        assert_eq!(p.central_freq, 1);
        assert_eq!((p.p0, p.p1, p.p2), (2, 4, 2));
        assert_eq!((p.dco_integer, p.dco_fraction), (360, 0));
    }

    #[test]
    fn hdmi_65000_xga() {
        let p = calculate(65_000, REF_KHZ).unwrap();
        assert_eq!((p.p, p.dco_khz), (28, 9_100_000));
        assert_eq!(p.central_freq, 0);
        assert_eq!((p.p0, p.p1, p.p2), (2, 7, 2));
        assert_eq!((p.dco_integer, p.dco_fraction), (379, 0x1555));
    }

    #[test]
    fn vga_25175() {
        let p = calculate(25_175, REF_KHZ).unwrap();
        assert_eq!((p.p, p.central_freq), (76, 0));
        assert_eq!(p.dco_khz, 9_566_500);
        assert_eq!((p.p0, p.p1, p.p2), (2, 19, 2));
        assert!(deviation_ok(&p));
    }

    #[test]
    fn exact_match_stops_search() {
        // 8400 MHz / 5 / 12 = 140 MHz: deviation 0 against the first central frequency.
        let p = calculate(140_000, REF_KHZ).unwrap();
        assert_eq!((p.p, p.dco_khz, p.central_freq), (12, 8_400_000, 3));
        assert_eq!((p.dco_integer, p.dco_fraction), (350, 0));
    }

    #[test]
    fn multiplier_splits_multiply_back() {
        for p in [4u32, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 52, 54, 56, 60, 64, 66, 68,
                  70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98, 3, 5, 7, 9, 15, 21, 35] {
            let (a, b, c) = multipliers(p);
            assert_eq!(a * b * c, p, "p={p}");
            assert!(matches!(a, 1 | 2 | 3 | 7) || p == 5, "p0 of {p}");
            assert!(matches!(c, 1 | 2 | 3 | 5), "p2 of {p}");
        }
    }

    #[test]
    fn every_chosen_solution_is_in_window() {
        for khz in (25_000..=300_000).step_by(250) {
            if let Some(p) = calculate(khz, REF_KHZ) {
                assert!(deviation_ok(&p), "{khz}");
                assert_eq!(p.p0 * p.p1 * p.p2, p.p);
            }
        }
        assert!(calculate(0, REF_KHZ).is_none());
    }
}
