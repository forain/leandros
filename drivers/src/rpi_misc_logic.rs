//! Pure (hardware-free) half of the Raspberry Pi 5 SoC housekeeping drivers in
//! `rpi_misc.rs`: the iproc-rng200 hardware RNG, the BCM2712 PM watchdog and
//! reset/halt sequences, the AVS temperature conversion, the firmware property
//! tags for the RTC / temperature / ARM clock, and the cpufreq governor.
//! No MMIO and no `alloc` here: the host tests (drivers/rpi-misc-tests)
//! include this file verbatim and drive it against mocks.

// ── Register access seam ─────────────────────────────────────────────────────

/// 32-bit register window (one device block). Offsets are bytes.
pub trait Regs32 {
    fn r(&mut self, off: usize) -> u32;
    fn w(&mut self, off: usize, v: u32);
}

// ── iproc-rng200 (`brcm,bcm2711-rng200`, Linux drivers/char/hw_random/iproc-rng200.c) ──

pub mod rng200 {
    use super::Regs32;

    pub const CTRL: usize = 0x00;
    pub const SOFT_RESET: usize = 0x04;
    pub const RBG_SOFT_RESET: usize = 0x08;
    pub const INT_STATUS: usize = 0x18;
    pub const FIFO_DATA: usize = 0x20;
    pub const FIFO_COUNT: usize = 0x24;

    pub const CTRL_RBGEN_MASK: u32 = 0x1FFF;
    pub const CTRL_RBGEN_ENABLE: u32 = 0x1;
    pub const INT_MASTER_FAIL_LOCKUP: u32 = 0x8000_0000;
    pub const INT_NIST_FAIL: u32 = 0x20;
    pub const FIFO_COUNT_MASK: u32 = 0xFF;

    pub fn enable_set(r: &mut dyn Regs32, on: bool) {
        let mut v = r.r(CTRL) & !CTRL_RBGEN_MASK;
        if on { v |= CTRL_RBGEN_ENABLE; }
        r.w(CTRL, v);
    }

    /// `iproc_rng200_restart`: disable, clear the lockup, pulse both soft
    /// resets, re-enable.
    pub fn restart(r: &mut dyn Regs32) {
        enable_set(r, false);
        let s = r.r(INT_STATUS);
        r.w(INT_STATUS, s | INT_MASTER_FAIL_LOCKUP);
        for off in [SOFT_RESET, RBG_SOFT_RESET] {
            let v = r.r(off);
            r.w(off, v | 1);
            r.w(off, v & !1);
        }
        enable_set(r, true);
    }

    /// True when the generator reports a fatal condition and needs `restart`.
    pub fn failed(r: &mut dyn Regs32) -> bool {
        r.r(INT_STATUS) & (INT_MASTER_FAIL_LOCKUP | INT_NIST_FAIL) != 0
    }

    /// Words currently waiting in the output FIFO.
    pub fn fifo_count(r: &mut dyn Regs32) -> u32 { r.r(FIFO_COUNT) & FIFO_COUNT_MASK }

    /// Fill a prefix of `out` from the FIFO without waiting. Returns the number
    /// of bytes written (whole or partial words; 0 = FIFO empty or the
    /// generator failed, in which case it is restarted for next time).
    pub fn read(r: &mut dyn Regs32, out: &mut [u8]) -> usize {
        if failed(r) {
            restart(r);
            return 0;
        }
        let mut done = 0;
        while done < out.len() {
            if fifo_count(r) == 0 { break; }
            let w = r.r(FIFO_DATA).to_le_bytes();
            let n = (out.len() - done).min(4);
            out[done..done + n].copy_from_slice(&w[..n]);
            done += n;
        }
        done
    }
}

// ── BCM2712 PM block (`brcm,bcm2712-pm`, Linux bcm2835_wdt.c) ────────────────

pub mod pm {
    use super::Regs32;

    pub const RSTC: usize = 0x1c;
    pub const RSTS: usize = 0x20;
    pub const WDOG: usize = 0x24;

    pub const PASSWORD: u32 = 0x5a00_0000;
    pub const WDOG_TIME_SET: u32 = 0x000f_ffff;
    pub const RSTC_WRCFG_CLR: u32 = 0xffff_ffcf;
    pub const RSTC_WRCFG_FULL_RESET: u32 = 0x0000_0020;
    pub const RSTC_RESET: u32 = 0x0000_0102;
    /// Boot partition 63: the firmware's "halt, wait for the power button".
    pub const RSTS_RASPBERRYPI_HALT: u32 = 0x555;
    /// The watchdog counts 1/65536 s ticks; 20 bits ⇒ just under 16 s.
    pub const MAX_SECS: u32 = WDOG_TIME_SET >> 16;

    pub fn secs_to_ticks(secs: u32) -> u32 { (secs.min(MAX_SECS).max(1) << 16) & WDOG_TIME_SET }

    /// Arm (or re-arm = kick) the watchdog to fire a full reset in `ticks`.
    pub fn arm(r: &mut dyn Regs32, ticks: u32) {
        r.w(WDOG, PASSWORD | (ticks & WDOG_TIME_SET));
        let c = r.r(RSTC);
        r.w(RSTC, PASSWORD | (c & RSTC_WRCFG_CLR) | RSTC_WRCFG_FULL_RESET);
    }

    pub fn disarm(r: &mut dyn Regs32) { r.w(RSTC, PASSWORD | RSTC_RESET); }

    pub fn is_running(r: &mut dyn Regs32) -> bool { r.r(RSTC) & RSTC_WRCFG_FULL_RESET != 0 }

    /// `__bcm2835_restart`: full reset ~150 µs from now.
    pub fn restart(r: &mut dyn Regs32) { arm(r, 10); }

    /// The boot-partition field of PM_RSTS: partition bits 0..5 live in RSTS
    /// bits 0, 2, 4, 6, 8, 10 (`__bcm2835_restart`, raspberrypi/linux
    /// drivers/watchdog/bcm2835_wdt.c). 0 = the firmware's default partition.
    pub const RSTS_PARTITION_CLR: u32 = 0xffff_faaa;

    pub fn partition_bits(p: u8) -> u32 {
        (0..6).fold(0u32, |a, i| a | (((p as u32) >> i) & 1) << (2 * i))
    }

    pub fn partition_of(rsts: u32) -> u8 {
        (0..6).fold(0u8, |a, i| a | ((((rsts >> (2 * i)) & 1) as u8) << i))
    }

    /// Request boot partition `p` for the next reset (read-modify-write, all
    /// other RSTS bits kept, as Linux does).
    pub fn set_partition(r: &mut dyn Regs32, p: u8) {
        let v = r.r(RSTS) & RSTS_PARTITION_CLR & 0x00ff_ffff;
        r.w(RSTS, PASSWORD | v | partition_bits(p));
    }

    /// `bcm2835_power_off`: boot partition 63 (firmware halt) then reset.
    pub fn power_off(r: &mut dyn Regs32) {
        let v = r.r(RSTS);
        r.w(RSTS, PASSWORD | v | RSTS_RASPBERRYPI_HALT);
        restart(r);
    }
}

// ── AVS temperature monitor (`brcm,bcm2711-thermal`) ─────────────────────────

pub mod avs {
    /// `AVS_RO_TEMP_STATUS` inside the avs-monitor syscon.
    pub const TEMP_STATUS: usize = 0x200;
    pub const VALID_MASK: u32 = (1 << 16) | (1 << 10);
    pub const DATA_MASK: u32 = 0x3ff;
    /// The D0 dts `cpu-thermal` coefficients: <0xfffffdda 0x6ddd0>.
    pub const SLOPE: i32 = -550;
    pub const OFFSET: i32 = 450_000;

    /// Raw status word → millidegrees Celsius (None: not valid yet).
    pub fn millicelsius(raw: u32) -> Option<i32> {
        if raw & VALID_MASK == 0 { return None; }
        Some(SLOPE * (raw & DATA_MASK) as i32 + OFFSET)
    }
}

// ── Firmware property tags ───────────────────────────────────────────────────

pub mod fw {
    pub const GET_CLOCK_RATE: u32 = 0x0003_0002;
    pub const GET_MAX_CLOCK_RATE: u32 = 0x0003_0004;
    pub const GET_MIN_CLOCK_RATE: u32 = 0x0003_0007;
    pub const SET_CLOCK_RATE: u32 = 0x0003_8002;
    pub const GET_TEMPERATURE: u32 = 0x0003_0006;
    pub const GET_RTC_REG: u32 = 0x0003_0087;
    pub const SET_RTC_REG: u32 = 0x0003_8087;

    pub const CLK_ARM: u32 = 3;
    pub const RTC_TIME: u32 = 0;

    pub const RESP_OK: u32 = 0x8000_0000;
    /// Value words reserved per tag (requests and replies here need ≤ 3).
    pub const VAL_WORDS: usize = 3;
    pub const MSG_WORDS: usize = 5 + VAL_WORDS + 1;

    /// One-tag property message: size, request code, tag, value buffer size,
    /// request/response code, `VAL_WORDS` value words, end tag.
    pub fn build(tag: u32, vals: &[u32], msg: &mut [u32; MSG_WORDS]) {
        *msg = [0; MSG_WORDS];
        msg[0] = (MSG_WORDS * 4) as u32;
        msg[2] = tag;
        msg[3] = (VAL_WORDS * 4) as u32;
        let n = vals.len().min(VAL_WORDS);
        msg[5..5 + n].copy_from_slice(&vals[..n]);
    }

    /// The tag's value words from a reply, if the firmware handled the tag.
    pub fn parse(reply: &[u32]) -> Option<[u32; VAL_WORDS]> {
        if reply.len() < 5 + VAL_WORDS || reply[1] != RESP_OK || reply[4] & RESP_OK == 0 {
            return None;
        }
        let mut out = [0u32; VAL_WORDS];
        out.copy_from_slice(&reply[5..5 + VAL_WORDS]);
        Some(out)
    }

    /// 2020-01-01T00:00:00Z. A firmware RTC below this was never set.
    pub const RTC_SANE_MIN: u32 = 1_577_836_800;
    pub fn rtc_plausible(secs: u32) -> bool { secs >= RTC_SANE_MIN }
}

// ── cpufreq governor (ondemand-style, two-level) ─────────────────────────────

/// Decides the ARM clock from periodic load samples. A sample is "busy" when
/// any CPU had runnable work besides the governor itself. Like ondemand:
/// jump straight to `max` on load, step down to `min` only after the whole
/// window has stayed mostly idle.
pub struct Governor {
    pub min_hz: u32,
    pub max_hz: u32,
    pub cur_hz: u32,
    /// Busy flags of the last `WINDOW` samples, newest in bit 0.
    hist: u32,
}

impl Governor {
    pub const WINDOW: u32 = 10;
    /// Step down when at most this many of the last `WINDOW` samples were busy.
    pub const DOWN_BUSY_MAX: u32 = 1;

    pub fn new(min_hz: u32, max_hz: u32, cur_hz: u32) -> Self {
        Governor { min_hz, max_hz: max_hz.max(min_hz), cur_hz, hist: 0 }
    }

    /// Feed one sample; returns the new target if the clock should change.
    pub fn sample(&mut self, busy: bool) -> Option<u32> {
        self.hist = ((self.hist << 1) | busy as u32) & ((1 << Self::WINDOW) - 1);
        let target = if busy {
            self.max_hz
        } else if self.hist.count_ones() <= Self::DOWN_BUSY_MAX {
            self.min_hz
        } else {
            self.cur_hz
        };
        if target != self.cur_hz && self.min_hz != 0 {
            self.cur_hz = target;
            Some(target)
        } else {
            None
        }
    }
}

/// `nr_running` includes the sampling thread itself on its own CPU.
pub fn busy_from_nr_running(nr: u32) -> bool { nr > 1 }

// ── Small formatting helper (no alloc) ───────────────────────────────────────

pub struct Buf<'a> { pub b: &'a mut [u8], pub n: usize }

impl<'a> Buf<'a> {
    pub fn new(b: &'a mut [u8]) -> Self { Buf { b, n: 0 } }
    pub fn s(&mut self, s: &str) -> &mut Self {
        for &c in s.as_bytes() { if self.n < self.b.len() { self.b[self.n] = c; self.n += 1; } }
        self
    }
    pub fn u(&mut self, mut v: u64) -> &mut Self {
        let mut t = [0u8; 20];
        let mut i = 0;
        loop { t[i] = b'0' + (v % 10) as u8; i += 1; v /= 10; if v == 0 { break; } }
        while i > 0 { i -= 1; let c = t[i]; if self.n < self.b.len() { self.b[self.n] = c; self.n += 1; } }
        self
    }
    pub fn i(&mut self, v: i64) -> &mut Self {
        if v < 0 { self.s("-"); }
        self.u(v.unsigned_abs())
    }
    /// "0x" + lowercase hex, no leading zeros.
    pub fn x(&mut self, v: u64) -> &mut Self {
        self.s("0x");
        let mut started = false;
        for k in (0..16).rev() {
            let d = ((v >> (k * 4)) & 0xf) as u8;
            if d == 0 && !started && k != 0 { continue; }
            started = true;
            let c = if d < 10 { b'0' + d } else { b'a' + d - 10 };
            if self.n < self.b.len() { self.b[self.n] = c; self.n += 1; }
        }
        self
    }
    /// Millidegrees as "52.3".
    pub fn mc(&mut self, m: i32) -> &mut Self {
        if m < 0 { self.s("-"); }
        let a = m.unsigned_abs();
        self.u((a / 1000) as u64).s(".").u(((a % 1000) / 100) as u64)
    }
}

// ── Wedge-recovery watchdog policy (lane piwdt) ──────────────────────────────

/// When to stop feeding the PM watchdog, and the serial "reset now" sequence.
/// The Pi 5 sits on a USB-C port that cannot be power-switched, so a wedged
/// kernel must take the board down itself; the firmware then boots the
/// card's kernel.img again.
pub mod wdt {
    /// Hardware timeout re-armed on every feed (the PM WDOG maximum).
    pub const HW_SECS: u32 = 15;
    /// A CPU without a timer tick this long stops the feeding; the hardware
    /// fires `HW_SECS` later, so the board resets ~60 s into the stall.
    pub const STUCK_SECS: u32 = 45;
    /// Serial sequence: this many Ctrl-] (0x1d) in a row ...
    pub const MAGIC_BYTE: u8 = 0x1d;
    pub const MAGIC_COUNT: u32 = 5;
    /// ... within this window; also the BREAK -> 'b' window.
    pub const MAGIC_WINDOW_MS: u64 = 2000;

    /// The feed decision. `cpus` yields (cpu, seconds since it last took a
    /// timer tick, idle) for every online CPU other than the feeder. Returns
    /// the first CPU stuck for `stuck_secs` or more (feeding must stop), or
    /// None (every busy CPU ticked recently: feed). Idle CPUs are skipped: a
    /// parked CPU's timer may legitimately sleep.
    pub fn stuck_cpu(cpus: impl Iterator<Item = (usize, u32, bool)>, stuck_secs: u32) -> Option<(usize, u32)> {
        let mut worst: Option<(usize, u32)> = None;
        for (cpu, age, idle) in cpus {
            if idle || age < stuck_secs { continue; }
            if worst.map_or(true, |(_, a)| age > a) { worst = Some((cpu, age)); }
        }
        worst
    }

    /// Serial magic matcher: `MAGIC_COUNT` consecutive Ctrl-] within
    /// `MAGIC_WINDOW_MS`, or a BREAK followed by 'b' within the window.
    /// Pure state machine; the caller supplies a monotonic millisecond clock.
    #[derive(Clone, Copy, Default)]
    pub struct Magic { count: u32, first_ms: u64, brk_ms: Option<u64> }

    impl Magic {
        pub const fn new() -> Self { Magic { count: 0, first_ms: 0, brk_ms: None } }
        /// One received byte (`brk`: the PL011 flagged it as a BREAK).
        /// True = reset now.
        pub fn feed(&mut self, b: u8, brk: bool, now_ms: u64) -> bool {
            if brk { self.brk_ms = Some(now_ms); self.count = 0; return false; }
            if let Some(t) = self.brk_ms.take() {
                if b == b'b' && now_ms.wrapping_sub(t) <= MAGIC_WINDOW_MS { return true; }
            }
            if b != MAGIC_BYTE { self.count = 0; return false; }
            if self.count == 0 || now_ms.wrapping_sub(self.first_ms) > MAGIC_WINDOW_MS {
                self.count = 0;
                self.first_ms = now_ms;
            }
            self.count += 1;
            if self.count >= MAGIC_COUNT { self.count = 0; return true; }
            false
        }
    }

    /// True if the DTB carries a BCM2835-style PM / watchdog node.
    pub fn dtb_has_pm(dtb: &[u8]) -> bool {
        let has = |pat: &[u8]| dtb.windows(pat.len()).any(|w| w == pat);
        has(b"brcm,bcm2712-pm\0") || has(b"brcm,bcm2835-pm\0") || has(b"brcm,bcm2835-pm-wdt\0")
    }

    /// Do PM_RSTC / PM_RSTS / PM_WDOG read like a live BCM2835-style PM block?
    /// Used when the DTB has no PM node (the FDT a kexec builds is a minimal
    /// `linux,dummy-virt` tree) but the SoC id says BCM2712. The password
    /// byte reads back as 0 and PM_WDOG holds only its 20-bit counter, so a
    /// floating / absent / error bus (all-ones, or junk in the high bits) fails.
    pub fn pm_regs_plausible(rstc: u32, rsts: u32, wdog: u32) -> bool {
        let hi = 0xff00_0000;
        rstc & hi == 0 && rsts & hi == 0 && wdog & !super::pm::WDOG_TIME_SET == 0
    }
}

// ── reboot(RESTART2, arg) on the Pi: "[N] [tryboot]" ─────────────────────────
pub mod restart2 {
    /// Parse the RESTART2 argument the way raspberrypi/linux does: a leading
    /// decimal number is the one-shot boot partition (`bcm2835_restart` /
    /// `psci_sys_reset`: `sscanf("%lu")`, values >= 63 ignored, 0 = firmware
    /// default), the `tryboot` token sets the firmware's one-shot tryboot flag
    /// (`rpi_firmware_notify_reboot`). LeandrOS's tryboot.txt lives on MBR
    /// partition 1, so a tryboot without an explicit partition ("tryboot",
    /// "0 tryboot") targets partition 1: on a dual-boot card whose
    /// autoboot.txt defaults to another partition the trial still boots
    /// LeandrOS, and on a single-boot card p1 is the default anyway.
    /// Returns (partition, tryboot).
    pub fn parse(arg: &str) -> (u8, bool) {
        let t = arg.trim_start();
        let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
        let mut part = 0u8;
        if digits > 0 {
            if let Ok(v) = t[..digits].parse::<u64>() {
                if v < 63 { part = v as u8; }
            }
        }
        let tryboot = arg.split_ascii_whitespace().any(|w| w == "tryboot");
        if tryboot && part == 0 { part = 1; }
        (part, tryboot)
    }
}

// ── SD card MBR: which 0x83 partition is the F2FS root ───────────────────────
pub mod sdmbr {
    pub const F2FS_MAGIC: u32 = 0xF2F5_2010;
    /// Byte offset of the first F2FS superblock inside the partition.
    pub const F2FS_SB_OFFSET: u64 = 1024;

    /// Start LBA (512-byte sectors) of the F2FS root among the MBR's type
    /// 0x83 partitions: the first whose superblock magic (read through
    /// `magic_at(lba)`) is F2FS. A dual-boot card also carries an ext4 0x83
    /// partition (Raspberry Pi OS root, p4), so the type alone is not enough.
    /// No 0x83 with the magic (unreadable / unformatted): the first 0x83, as
    /// before. `None` without a 55AA signature or any 0x83 entry.
    pub fn pick_f2fs(mbr: &[u8], mut magic_at: impl FnMut(u64) -> Option<u32>) -> Option<u64> {
        if mbr.len() < 512 || mbr[510] != 0x55 || mbr[511] != 0xAA { return None; }
        let mut first = None;
        for i in 0..4usize {
            let e = &mbr[446 + i * 16..446 + i * 16 + 16];
            if e[4] != 0x83 { continue; }
            let lba = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
            if first.is_none() { first = Some(lba); }
            if magic_at(lba) == Some(F2FS_MAGIC) { return Some(lba); }
        }
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake { rsts: u32, writes: u32 }
    impl Regs32 for Fake {
        fn r(&mut self, off: usize) -> u32 { assert_eq!(off, pm::RSTS); self.rsts }
        fn w(&mut self, off: usize, v: u32) { assert_eq!(off, pm::RSTS); self.writes += 1; self.rsts = v & 0x00ff_ffff; assert_eq!(v & 0xff00_0000, pm::PASSWORD); }
    }

    #[test]
    fn partition_bits_match_linux() {
        assert_eq!(pm::partition_bits(0), 0);
        assert_eq!(pm::partition_bits(1), 0x001);
        assert_eq!(pm::partition_bits(2), 0x004);
        assert_eq!(pm::partition_bits(3), 0x005);
        assert_eq!(pm::partition_bits(63), pm::RSTS_RASPBERRYPI_HALT);
        for p in 0..64u8 { assert_eq!(pm::partition_of(pm::partition_bits(p)), p); }
    }

    #[test]
    fn set_partition_keeps_other_bits() {
        let mut f = Fake { rsts: 0x1a0a | 0x555, writes: 0 };
        pm::set_partition(&mut f, 3);
        assert_eq!(f.rsts, 0x1a0a | 0x005);
        pm::set_partition(&mut f, 0);
        assert_eq!(f.rsts, 0x1a0a);
        assert_eq!(f.writes, 2);
    }

    #[test]
    fn restart2_args() {
        assert_eq!(restart2::parse(""), (0, false));
        assert_eq!(restart2::parse("3"), (3, false));
        assert_eq!(restart2::parse("1"), (1, false));
        assert_eq!(restart2::parse("tryboot"), (1, true));
        assert_eq!(restart2::parse("0 tryboot"), (1, true));
        assert_eq!(restart2::parse("3 tryboot"), (3, true));
        assert_eq!(restart2::parse("63"), (0, false));
        assert_eq!(restart2::parse("99 tryboot"), (1, true));
        assert_eq!(restart2::parse("  2  "), (2, false));
        assert_eq!(restart2::parse("recovery"), (0, false));
        assert_eq!(restart2::parse("0 trybootx"), (0, false));
    }

    fn mbr(entries: &[(u8, u32)]) -> [u8; 512] {
        let mut m = [0u8; 512];
        for (i, &(ty, lba)) in entries.iter().enumerate() {
            let e = &mut m[446 + i * 16..446 + i * 16 + 16];
            e[4] = ty;
            e[8..12].copy_from_slice(&lba.to_le_bytes());
        }
        m[510] = 0x55; m[511] = 0xAA;
        m
    }

    #[test]
    fn f2fs_root_on_dual_boot_card() {
        // p1 FAT32 LeandrOS boot, p2 F2FS root, p3 FAT32 RPi OS boot, p4 ext4.
        let m = mbr(&[(0x0c, 8192), (0x83, 1_056_768), (0x0c, 40_000_000), (0x83, 41_000_000)]);
        let magic = |lba| if lba == 1_056_768 { Some(sdmbr::F2FS_MAGIC) } else { Some(0xEF53) };
        assert_eq!(sdmbr::pick_f2fs(&m, magic), Some(1_056_768));
        // ext4 ahead of the F2FS partition in the table: still the F2FS one.
        let m = mbr(&[(0x0c, 8192), (0x83, 41_000_000), (0x0c, 40_000_000), (0x83, 1_056_768)]);
        assert_eq!(sdmbr::pick_f2fs(&m, magic), Some(1_056_768));
        // Single-boot card, magic unreadable: first 0x83 as before.
        let m = mbr(&[(0x0c, 8192), (0x83, 1_056_768)]);
        assert_eq!(sdmbr::pick_f2fs(&m, |_| None), Some(1_056_768));
        // No signature / no Linux partition.
        let mut bad = m; bad[511] = 0;
        assert_eq!(sdmbr::pick_f2fs(&bad, |_| None), None);
        assert_eq!(sdmbr::pick_f2fs(&mbr(&[(0x0c, 8192)]), |_| None), None);
    }
}
