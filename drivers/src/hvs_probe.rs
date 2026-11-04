//! Read-only boot probe of the BCM2712 HVS (Raspberry Pi 5, rpi5 only), opt-in with `hvs.probe=1`.
//!
//! No display register is written. Register layout from Linux rpi-6.12.y `vc4_regs.h`
//! (SCALER6D_*, the D0 stepping, VERSION 0x54): per-channel block at 0x100 + 0x40 * n
//! (the C0 SCALER6_* layout is 0x30 + 0x20 * n and would hit wrong registers).
//! Output: one `[hvs] ...` summary line, one line per channel, the display-list words of each
//! enabled channel, and the hdmi hpd / DT simple-framebuffer facts. Every read block is
//! followed by `serror_checkpoint()`; a bus error is logged and the probe stops.

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::probe_boot;

pub fn probe_enabled(cmdline: &str) -> bool {
    cmdline.split_ascii_whitespace().any(|t| t == "hvs.probe=1")
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use super::*;
    use crate::pci::serial_debug;
    use alloc::format;
    use leandros_lib::mmio::MmioRd;
    use mm::kva::{ioremap, iounmap, CacheMode};

    /// `hvs@107c580000` under `soc@107c000000`.
    const HVS_PHYS: u64 = 0x10_7c58_0000;
    /// Registers (0x100 + 4 channels * 0x40) plus the display-list RAM at 0x4000 (4096 words).
    const HVS_LEN: usize = 0x8000;
    const DLIST_OFF: usize = 0x4000;
    const DLIST_WORDS: usize = 4096;
    const DUMP_WORDS: usize = 32;

    const VERSION: usize = 0x00;
    const CXM_SIZE: usize = 0x04;
    const COB_SIZE: usize = 0x14;
    const CONTROL: usize = 0x20;
    const FETCHER_STATUS: usize = 0x24;
    const DL_STATUS: usize = 0x34;
    const HVS_ID: usize = 0xfc;
    const CH_BASE: usize = 0x100;
    const CH_STRIDE: usize = 0x40;
    const CH_COUNT: usize = 3;
    const CH_CTRL0: usize = 0x00;
    const CH_CTRL1: usize = 0x04;
    const CH_LPTRS: usize = 0x10;
    const CH_COB: usize = 0x14;
    const CH_STATUS: usize = 0x18;
    const CH_DL: usize = 0x1c;
    const CH_RUN: usize = 0x20;

    pub type SerrorCheck = fn() -> Option<u64>;

    #[inline]
    fn rd(base: usize, off: usize) -> u32 {
        unsafe { ((base + off) as *const u32).mmio_read() }
    }

    fn dt_has_simple_fb() -> (bool, bool) {
        let mut found = false;
        let ok = unsafe {
            boot::device_tree::for_each_compatible(&mut |n| {
                if n.compat.windows(18).any(|w| w == b"simple-framebuffer") {
                    found = true;
                }
            })
        };
        (ok, found)
    }

    pub fn probe_boot(cmdline: &str, serror_check: SerrorCheck) {
        if !probe_enabled(cmdline) {
            return;
        }
        let (dt_ok, fb) = dt_has_simple_fb();
        let hpd: [u8; 2] = core::array::from_fn(|i| {
            (crate::hdmi_probe::PORT_FLAGS[i].load(core::sync::atomic::Ordering::Relaxed) & crate::hdmi_probe::PORT_HPD != 0) as u8
        });
        serial_debug(&format!(
            "[hvs] dt simple-framebuffer={} (dt_parsed={}) hdmi_probe_state={} hpd0={} hpd1={}\n",
            fb as u8, dt_ok as u8, crate::hdmi_probe::state(), hpd[0], hpd[1]
        ));
        let Some(va) = ioremap(HVS_PHYS, HVS_LEN, CacheMode::Uncached) else {
            serial_debug("[hvs] fail ioremap\n");
            return;
        };
        // The VERSION read alone first: if the block is unclocked the SError shows up here.
        let version = rd(va, VERSION);
        if let Some(syn) = serror_check() {
            serial_debug(&format!("[hvs] serror syn=0x{:016x} on VERSION read; probe stopped\n", syn));
            iounmap(va);
            return;
        }
        let ver = version & 0xff;
        let cxm = rd(va, CXM_SIZE);
        let cob = rd(va, COB_SIZE);
        let ctl = rd(va, CONTROL);
        let fetcher = rd(va, FETCHER_STATUS);
        let dls = rd(va, DL_STATUS);
        let id = rd(va, HVS_ID);
        if let Some(syn) = serror_check() {
            serial_debug(&format!("[hvs] serror syn=0x{:016x} on global block; probe stopped\n", syn));
            iounmap(va);
            return;
        }
        serial_debug(&format!(
            "[hvs] version=0x{:08x} ({}) cxm_size=0x{:x} cob_size=0x{:x} control=0x{:08x} fetcher=0x{:08x} dl_status=0x{:08x} hvs_id=0x{:08x}\n",
            version,
            if ver == 0x54 { "D0 layout" } else if ver == 0x53 { "C0 layout, this probe uses D0 offsets: ignore channel lines" } else { "UNKNOWN" },
            cxm, cob, ctl, fetcher, dls, id
        ));
        if ver != 0x54 {
            iounmap(va);
            return;
        }

        let mut raw = [[0u32; 7]; CH_COUNT];
        for (c, r) in raw.iter_mut().enumerate() {
            let b = CH_BASE + c * CH_STRIDE;
            *r = [rd(va, b + CH_CTRL0), rd(va, b + CH_CTRL1), rd(va, b + CH_LPTRS), rd(va, b + CH_COB), rd(va, b + CH_STATUS), rd(va, b + CH_DL), rd(va, b + CH_RUN)];
        }
        // FRCNT delta over ~100 ms (STATUS[21:16] in the C0 layout; raw words are also logged).
        let t0 = sched::monotonic_ns();
        while sched::monotonic_ns().wrapping_sub(t0) < 100_000_000 {
            core::hint::spin_loop();
        }
        let after: [u32; CH_COUNT] = core::array::from_fn(|c| rd(va, CH_BASE + c * CH_STRIDE + CH_STATUS));
        if let Some(syn) = serror_check() {
            serial_debug(&format!("[hvs] serror syn=0x{:016x} on channel block; probe stopped\n", syn));
            iounmap(va);
            return;
        }
        for c in 0..CH_COUNT {
            let [c0, c1, lp, cb, st, dl, run] = raw[c];
            let enb = c0 >> 31;
            let mode = (st >> 13) & 3;
            let frcnt = (st >> 16) & 0x3f;
            let d = (((after[c] >> 16) & 0x3f).wrapping_sub(frcnt)) & 0x3f;
            serial_debug(&format!(
                "[hvs] ch{} ctrl0=0x{:08x} ctrl1=0x{:08x} lptrs=0x{:08x} cob=0x{:08x} status=0x{:08x} dl=0x{:08x} run=0x{:08x} enb={} mode={} frcnt+{}/100ms\n",
                c, c0, c1, lp, cb, st, dl, run, enb, mode, d
            ));
        }
        // Display-list words of each enabled channel, from DL.LACT (low 12 bits) and from LPTRS head.
        for c in 0..CH_COUNT {
            let [c0, _, lp, _, _, dl, _] = raw[c];
            if c0 >> 31 == 0 {
                continue;
            }
            for (name, start) in [("lact", (dl & 0xfff) as usize), ("lptrs", (lp & 0xfff) as usize)] {
                if start + DUMP_WORDS > DLIST_WORDS {
                    serial_debug(&format!("[hvs] ch{} {} start {} out of range\n", c, name, start));
                    continue;
                }
                let w: [u32; DUMP_WORDS] = core::array::from_fn(|i| rd(va, DLIST_OFF + (start + i) * 4));
                serial_debug(&format!("[hvs] ch{} dlist@{}({}):", c, start, name));
                for chunk in w.chunks(8) {
                    let mut s = alloc::string::String::new();
                    for x in chunk {
                        s.push_str(&format!(" {:08x}", x));
                    }
                    serial_debug(&format!("{}\n[hvs]  ", s));
                }
                serial_debug("\n");
            }
        }
        if let Some(syn) = serror_check() {
            serial_debug(&format!("[hvs] serror syn=0x{:016x} on dlist read\n", syn));
        }
        iounmap(va);
    }
}
