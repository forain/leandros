//! BCM2712 stepping (C0/C1 vs D0) read from the chip, with the device tree as
//! a fallback.
//!
//! The live DT cannot be trusted for this: the firmware loads the base
//! `bcm2712-rpi-5-b.dtb` (C1 layout, `brcm,bcm2712c0-pinctrl`) on every Pi 5
//! and only turns it into a D0 tree by applying `overlays/bcm2712d0.dtbo`. A
//! card without that overlay boots a D0 chip with a C1 tree (seen on the real
//! board 2026-10-26: uart10 on SPI 120, sdio2 pin-muxed with the C0 table,
//! CMD5 timeout).
//!
//! Hardware source: SUN_TOP_CTRL, the brcmstb chip-id block. Linux
//! `drivers/soc/bcm/brcmstb/common.c` reads `CHIP_FAMILY_ID` at +0 and
//! `PRODUCT_ID` at +4 and names the revision `((id & 0xf0) >> 4) + 'A'`,
//! `id & 0xf` (so C1 = 0x21, D0 = 0x30). The stock BCM2712 tree has no
//! sun-top-ctrl node, but its `brcm,brcmstb-reset` controller sits at
//! `reset-controller@1504318`, which is SUN_TOP_CTRL + 0x318 on every brcmstb
//! part; with the soc `ranges` (+0x10_0000_0000) the block is at
//! 0x10_0150_4000. Linux uses that page from EL1, so reading it is safe.
//! Anything that does not decode as a 0x2712 C/D part falls back to the DT.

/// Pin-mux layout family. C0 and C1 share `brcm,bcm2712c0-pinctrl`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stepping {
    C0,
    D0,
    Unknown,
}

/// Where the answer came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Hw,
    Dt,
    None,
}

pub const SUN_TOP_CTRL_PHYS: u64 = 0x10_0150_4000;
pub const CHIP_FAMILY_ID: usize = 0x0;
pub const PRODUCT_ID: usize = 0x4;

/// Decode the two SUN_TOP_CTRL id words. Returns the layout and the revision
/// byte (major nibble: 2 = C, 3 = D), or `None` if neither word is a BCM2712.
pub fn decode_chip_id(family: u32, product: u32) -> Option<(Stepping, u8)> {
    let rev = if product >> 16 == 0x2712 {
        product as u8
    } else if family >> 16 == 0x2712 {
        family as u8
    } else {
        return None;
    };
    let st = match rev >> 4 {
        2 => Stepping::C0,
        3 => Stepping::D0,
        _ => Stepping::Unknown,
    };
    Some((st, rev))
}

/// Hardware first; the DT only when the hardware gave no C/D answer.
pub fn select(hw: Option<Stepping>, dt: Stepping) -> (Stepping, Source) {
    match hw {
        Some(s @ (Stepping::C0 | Stepping::D0)) => (s, Source::Hw),
        _ if dt != Stepping::Unknown => (dt, Source::Dt),
        _ => (Stepping::Unknown, Source::None),
    }
}

/// sdio2 / WL_REG_ON pin rows: (pin, mux reg, mux nibble, pad reg, pad slot,
/// fsel, pull), per Linux `pinctrl-bcm2712.c` `GPIO_REGS(n, mr, mb, pr, pb)`.
/// fsel 0 = GPIO, fsel k = the k-th function in the `PIN(n, f1..f8)` row
/// (`bcm2712_pinctrl_fsel_set`); sd2 is f4/f3 on C0 and f1 on D0. pull: 0
/// none, 2 up (dts `brcm,pull`).
pub type PinRow = (u32, usize, u32, usize, u32, u32, u32);
pub const C0_PINS: [PinRow; 7] = [
    (28, 3, 4, 9, 5, 0, 0),
    (30, 3, 6, 9, 7, 4, 0),
    (31, 3, 7, 9, 8, 4, 2),
    (32, 4, 0, 9, 9, 4, 2),
    (33, 4, 1, 9, 10, 3, 2),
    (34, 4, 2, 9, 11, 4, 2),
    (35, 4, 3, 9, 12, 3, 2),
];
pub const D0_PINS: [PinRow; 7] = [
    (28, 2, 4, 5, 10, 0, 0),
    (30, 2, 6, 5, 12, 1, 0),
    (31, 2, 7, 5, 13, 1, 2),
    (32, 3, 0, 5, 14, 1, 2),
    (33, 3, 1, 6, 0, 1, 2),
    (34, 3, 2, 6, 1, 1, 2),
    (35, 3, 3, 6, 2, 1, 2),
];

pub fn pin_rows(st: Stepping) -> Option<&'static [PinRow; 7]> {
    match st {
        Stepping::C0 => Some(&C0_PINS),
        Stepping::D0 => Some(&D0_PINS),
        Stepping::Unknown => None,
    }
}

#[cfg(feature = "rpi5")]
pub use hw::{stepping, StepInfo};

#[cfg(feature = "rpi5")]
mod hw {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};
    use leandros_lib::mmio::MmioRd;

    #[derive(Clone, Copy, Debug)]
    pub struct StepInfo {
        pub stepping: Stepping,
        pub source: Source,
        pub family: u32,
        pub product: u32,
    }

    /// Cached: bits 0..1 stepping, 2..3 source, bit 4 valid; ids in 32..63 (product).
    static CACHE: AtomicU64 = AtomicU64::new(0);
    static FAMILY: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

    extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }

    fn put_hex(buf: &mut [u8], at: &mut usize, v: u32) {
        for i in (0..8).rev() {
            let n = ((v >> (i * 4)) & 0xF) as u8;
            buf[*at] = if n < 10 { b'0' + n } else { b'a' + n - 10 };
            *at += 1;
        }
    }
    fn put(buf: &mut [u8], at: &mut usize, s: &[u8]) {
        buf[*at..*at + s.len()].copy_from_slice(s);
        *at += s.len();
    }

    fn dt_stepping() -> Stepping {
        unsafe {
            if boot::device_tree::struct_contains(b"brcm,bcm2712d0-pinctrl") == Some(true) { return Stepping::D0; }
            if boot::device_tree::struct_contains(b"brcm,bcm2712c0-pinctrl") == Some(true) { return Stepping::C0; }
        }
        Stepping::Unknown
    }

    /// BCM2712 stepping, hardware first. Evaluated once; the first call prints
    /// `[bcm2712] stepping=D0 (hw) chipid=<family>/<product> dt=<C0|D0|Unknown>`.
    /// Needs only the boot-time Device mapping of 0x10_0000_0000..0x10_8000_0000.
    pub fn stepping() -> StepInfo {
        let c = CACHE.load(Ordering::Acquire);
        let unpack_st = |v: u64| match v & 3 { 0 => Stepping::C0, 1 => Stepping::D0, _ => Stepping::Unknown };
        let unpack_src = |v: u64| match (v >> 2) & 3 { 0 => Source::Hw, 1 => Source::Dt, _ => Source::None };
        if c & 0x10 != 0 {
            return StepInfo { stepping: unpack_st(c), source: unpack_src(c),
                              family: FAMILY.load(Ordering::Relaxed), product: (c >> 32) as u32 };
        }
        let base = mm::phys_to_virt(SUN_TOP_CTRL_PHYS as usize);
        let (family, product) = unsafe {
            (((base + CHIP_FAMILY_ID) as *const u32).mmio_read(), ((base + PRODUCT_ID) as *const u32).mmio_read())
        };
        let hw = decode_chip_id(family, product).map(|(s, _)| s);
        let dt = dt_stepping();
        let (st, src) = select(hw, dt);
        let sv = match st { Stepping::C0 => 0u64, Stepping::D0 => 1, Stepping::Unknown => 2 };
        let srcv = match src { Source::Hw => 0u64, Source::Dt => 1, Source::None => 2 };
        FAMILY.store(family, Ordering::Relaxed);
        CACHE.store(sv | (srcv << 2) | 0x10 | ((product as u64) << 32), Ordering::Release);

        let mut b = [0u8; 96];
        let mut n = 0;
        put(&mut b, &mut n, b"[bcm2712] stepping=");
        put(&mut b, &mut n, match st { Stepping::C0 => b"C0", Stepping::D0 => b"D0", Stepping::Unknown => b"Unknown" });
        put(&mut b, &mut n, match src { Source::Hw => b" (hw)", Source::Dt => b" (dt)", Source::None => b" (none)" });
        put(&mut b, &mut n, b" chipid=");
        put_hex(&mut b, &mut n, family);
        put(&mut b, &mut n, b"/");
        put_hex(&mut b, &mut n, product);
        put(&mut b, &mut n, b" dt=");
        put(&mut b, &mut n, match dt { Stepping::C0 => b"C0", Stepping::D0 => b"D0", Stepping::Unknown => b"Unknown" });
        put(&mut b, &mut n, b"\n");
        unsafe { serial_print_str_raw(b.as_ptr(), n); }
        StepInfo { stepping: st, source: src, family, product }
    }
}
