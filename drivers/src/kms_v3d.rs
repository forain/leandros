//! Raspberry Pi 5 scanout of v3d BOs (phase E of the Pi V3D plan).
//!
//! Mesa's v3d is a single-device driver: a GBM scanout surface is a v3d BO, linear XRGB8888 with
//! a tightly packed pitch (`stride = width * 4`, `v3d_resource.c`), and its GEM handle goes
//! straight to ADDFB2. There is no HVS/KMS driver yet; the screen is the firmware framebuffer
//! the mailbox allocated, whose pitch the firmware chooses (`[MBOX] ... v3d_linear_match=`). So a
//! present copies the BO into the firmware framebuffer row by row, converting the stride.
//!
//! Cache maintenance: the GPU writes the BO through the V3D MMU, which is not coherent with the
//! CPU caches, and userspace writes it through a Normal-NC mmap. The kernel reads it through the
//! cacheable HHDM alias, so that range is invalidated (`dc ivac`) first; nothing ever dirties a
//! line of that alias after `mmu::map_bo`'s clean, so invalidating discards no data. The
//! destination is cleaned (`dc cvac`) afterwards so the display engine, which reads memory, sees
//! the copy whatever attribute the framebuffer's HHDM mapping has.
//!
//! The pure part (`stride_copy`, `clip`) is host-tested in `drivers/v3d-tests`.

/// Copies `rows` rows of `row_bytes` bytes from `src` (rows `src_pitch` apart) to `dst` (rows
/// `dst_pitch` apart). Returns the number of rows copied: fewer than `rows` only if a buffer
/// is too short, in which case no partial row is written.
pub fn stride_copy(src: &[u8], src_pitch: usize, dst: &mut [u8], dst_pitch: usize,
                   row_bytes: usize, rows: usize) -> usize {
    if row_bytes > src_pitch || row_bytes > dst_pitch {
        return 0;
    }
    // Same pitch on both sides: one contiguous copy of the whole block.
    if src_pitch == dst_pitch && row_bytes == src_pitch {
        let n = rows.min(src.len() / src_pitch).min(dst.len() / dst_pitch);
        let bytes = n * row_bytes;
        dst[..bytes].copy_from_slice(&src[..bytes]);
        return n;
    }
    let mut done = 0;
    while done < rows {
        let s = done * src_pitch;
        let d = done * dst_pitch;
        if s + row_bytes > src.len() || d + row_bytes > dst.len() {
            break;
        }
        dst[d..d + row_bytes].copy_from_slice(&src[s..s + row_bytes]);
        done += 1;
    }
    done
}

/// The visible part of a `fb_w`x`fb_h` framebuffer on a `scr_w`x`scr_h` screen, top-left
/// aligned (no scaling: the panel shows the BO pixel for pixel). Returns (width, height).
pub fn clip(fb_w: u32, fb_h: u32, scr_w: u32, scr_h: u32) -> (u32, u32) {
    (fb_w.min(scr_w), fb_h.min(scr_h))
}

/// Whether `n` is a flip count worth a log line: the first eight, then powers of two.
pub fn flip_log_due(n: u64) -> bool {
    n <= 8 || n.is_power_of_two()
}

/// DRM fourcc XR24 (XRGB8888) and AR24 (ARGB8888).
pub const FOURCC_XR24: u32 = 0x3432_5258;
pub const FOURCC_AR24: u32 = 0x3432_5241;

/// May a v3d BO of `w`x`h`, `pitch` bytes per row and `size` bytes be wrapped in a scanout
/// framebuffer? Linear XR24/AR24 only; the pitch may exceed `w * 4` (Mesa packs it tightly,
/// other clients align it) but must hold a row and the BO must hold every row.
pub fn fb_acceptable(fourcc: u32, w: u32, h: u32, pitch: u32, offset: u32, size: u64) -> bool {
    if fourcc != FOURCC_XR24 && fourcc != FOURCC_AR24 { return false; }
    if w == 0 || h == 0 || offset != 0 { return false; }
    let Some(row) = w.checked_mul(4) else { return false };
    if pitch < row || pitch % 4 != 0 { return false; }
    (pitch as u64) * (h as u64 - 1) + row as u64 <= size
}

/// `v3d.headless=WxH` on the kernel command line: the opt-in RAM scanout surface size.
pub fn parse_headless(cmdline: &str) -> Option<(u32, u32)> {
    let v = cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix("v3d.headless="))?;
    let (w, h) = v.split_once('x')?;
    let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
    if w < 16 || h < 16 || w > 4096 || h > 4096 { return None; }
    Some((w, h))
}

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// CRC-32 (IEEE, reflected) continuing from `crc` (start with 0).
pub fn crc32(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use core::sync::atomic::{AtomicU64, Ordering};

    static V3D_FLIPS: AtomicU64 = AtomicU64::new(0);

    /// Number of v3d BO presents so far.
    pub fn flips() -> u64 {
        V3D_FLIPS.load(Ordering::Relaxed)
    }

    pub fn dc_range(va: usize, len: usize, clean: bool) {
        let mut p = va & !63;
        let end = va + len;
        while p < end {
            unsafe {
                if clean {
                    core::arch::asm!("dc cvac, {}", in(reg) p, options(nostack, preserves_flags));
                } else {
                    core::arch::asm!("dc ivac, {}", in(reg) p, options(nostack, preserves_flags));
                }
            }
            p += 64;
        }
        unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)); }
    }

    fn log_dec(mut v: u64) {
        let mut b = [0u8; 20];
        let mut i = b.len();
        loop {
            i -= 1;
            b[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 { break; }
        }
        crate::pci::serial_debug(core::str::from_utf8(&b[i..]).unwrap_or("?"));
    }

    /// Copy a v3d BO framebuffer (`src_phys`, `w`x`h`, `src_pitch`) into the firmware
    /// framebuffer at the firmware's pitch, top-left aligned and clipped to the screen.
    pub fn present(src_phys: u64, w: u32, h: u32, src_pitch: u32) -> Result<(), crate::DriverError> {
        let (hw_base, hw_w, hw_h, hw_pitch) =
            crate::framebuffer::get_hardware_fb_info().ok_or(crate::DriverError::NotFound)?;
        if src_phys == 0 || hw_base == 0 || (src_pitch as u64) < (w as u64) * 4 {
            return Err(crate::DriverError::InvalidParameter);
        }
        let (cw, ch) = super::clip(w, h, hw_w, hw_h);
        let row_bytes = cw as usize * 4;
        let src_len = (ch as usize).saturating_sub(1) * src_pitch as usize + row_bytes;
        let dst_len = (ch as usize).saturating_sub(1) * hw_pitch as usize + row_bytes;
        let src_va = mm::phys_to_virt(src_phys as usize) as usize;
        let dst_va = mm::phys_to_virt(hw_base as usize) as usize;
        dc_range(src_va, src_len, false);
        let copied = unsafe {
            let src = core::slice::from_raw_parts(src_va as *const u8, src_len);
            let dst = core::slice::from_raw_parts_mut(dst_va as *mut u8, dst_len);
            super::stride_copy(src, src_pitch as usize, dst, hw_pitch as usize, row_bytes, ch as usize)
        };
        dc_range(dst_va, dst_len, true);
        let n = V3D_FLIPS.fetch_add(1, Ordering::Relaxed) + 1;
        if super::flip_log_due(n) {
            crate::pci::serial_debug("[kms] v3d flip ");
            log_dec(n);
            crate::pci::serial_debug(" ");
            log_dec(w as u64);
            crate::pci::serial_debug("x");
            log_dec(h as u64);
            crate::pci::serial_debug(" pitch=");
            log_dec(src_pitch as u64);
            crate::pci::serial_debug("->");
            log_dec(hw_pitch as u64);
            crate::pci::serial_debug(" rows=");
            log_dec(copied as u64);
            crate::pci::serial_debug("\n");
        }
        Ok(())
    }
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::{dc_range, flips, present};

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(w: usize, h: usize, pitch: usize) -> Vec<u8> {
        let mut v = vec![0xEEu8; pitch * h];
        for y in 0..h {
            for x in 0..w * 4 {
                v[y * pitch + x] = (y * 7 + x) as u8;
            }
        }
        v
    }

    #[test]
    fn tight_to_wider_firmware_pitch() {
        // 1920x4 XRGB tight (7680) into a firmware pitch of 8192.
        let (w, h) = (1920, 4);
        let src = pattern(w, h, w * 4);
        let mut dst = vec![0x55u8; 8192 * h];
        assert_eq!(stride_copy(&src, w * 4, &mut dst, 8192, w * 4, h), h);
        for y in 0..h {
            assert_eq!(&dst[y * 8192..y * 8192 + w * 4], &src[y * w * 4..(y + 1) * w * 4]);
            // Padding past the visible row is untouched.
            assert!(dst[y * 8192 + w * 4..(y + 1) * 8192].iter().all(|&b| b == 0x55));
        }
    }

    #[test]
    fn equal_pitch_is_one_block() {
        let src = pattern(16, 8, 64);
        let mut dst = vec![0u8; 64 * 8];
        assert_eq!(stride_copy(&src, 64, &mut dst, 64, 64, 8), 8);
        assert_eq!(dst, src);
    }

    #[test]
    fn padded_source_to_tight_destination() {
        let src = pattern(10, 5, 48);
        let mut dst = vec![0u8; 40 * 5];
        assert_eq!(stride_copy(&src, 48, &mut dst, 40, 40, 5), 5);
        for y in 0..5 {
            assert_eq!(&dst[y * 40..(y + 1) * 40], &src[y * 48..y * 48 + 40]);
        }
    }

    #[test]
    fn short_buffers_stop_at_whole_rows() {
        let src = pattern(4, 4, 16);
        let mut dst = vec![0u8; 32 * 2 + 8]; // room for two rows and a fragment
        assert_eq!(stride_copy(&src, 16, &mut dst, 32, 16, 4), 2);
        assert!(dst[64..].iter().all(|&b| b == 0));
        // A row wider than either pitch copies nothing.
        assert_eq!(stride_copy(&src, 16, &mut dst, 32, 20, 4), 0);
    }

    #[test]
    fn acceptance_headless_and_crc() {
        assert!(fb_acceptable(FOURCC_XR24, 1920, 1080, 7680, 0, 7680 * 1080));
        assert!(fb_acceptable(FOURCC_AR24, 1920, 1080, 8192, 0, 8192 * 1080));
        assert!(!fb_acceptable(0x3231_564e, 64, 64, 256, 0, 1 << 20)); // NV12
        assert!(!fb_acceptable(FOURCC_XR24, 64, 64, 255, 0, 1 << 20)); // pitch < w*4
        assert!(!fb_acceptable(FOURCC_XR24, 64, 64, 256, 4, 1 << 20)); // offset
        assert!(!fb_acceptable(FOURCC_XR24, 64, 64, 256, 0, 256 * 63)); // BO too small
        assert_eq!(parse_headless("a v3d.headless=1280x720 b"), Some((1280, 720)));
        assert_eq!(parse_headless("v3d.headless=1x1"), None);
        assert_eq!(parse_headless("v3d.probe=1"), None);
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(crc32(0, b"1234"), b"56789"), 0xCBF4_3926);
    }

    #[test]
    fn clip_to_screen_and_log_cadence() {
        assert_eq!(clip(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(clip(640, 480, 1920, 1080), (640, 480));
        let due: Vec<u64> = (1..=40).filter(|&n| flip_log_due(n)).collect();
        assert_eq!(due, vec![1, 2, 3, 4, 5, 6, 7, 8, 16, 32]);
    }
}
