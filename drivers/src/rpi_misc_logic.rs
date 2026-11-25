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
    /// No 0x83 with the magic (another OS's card, unreadable, unformatted):
    /// `None`, so the caller never treats a foreign ext4 as ours. `None` as
    /// well without a 55AA signature or any 0x83 entry.
    pub fn pick_f2fs(mbr: &[u8], mut magic_at: impl FnMut(u64) -> Option<u32>) -> Option<u64> {
        if mbr.len() < 512 || mbr[510] != 0x55 || mbr[511] != 0xAA { return None; }
        for i in 0..4usize {
            let e = &mbr[446 + i * 16..446 + i * 16 + 16];
            if e[4] != 0x83 { continue; }
            let lba = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
            if lba == 0 || lba % 8 != 0 { continue; } // 4 KiB blocks: the offset must be block aligned
            if magic_at(lba) == Some(F2FS_MAGIC) { return Some(lba); }
        }
        None
    }

    /// Superblock volume label: UTF-16LE at byte 124 of the superblock.
    pub const SB_LABEL_OFFSET: usize = 124;

    /// An ASCII copy of the superblock label (non-ASCII units become '?').
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Label { pub b: [u8; 64], pub n: usize }
    impl Label {
        pub fn as_str(&self) -> &str { core::str::from_utf8(&self.b[..self.n]).unwrap_or("") }
    }

    /// The label of the superblock slice `sb` (the 1024.. bytes of the partition).
    pub fn sb_label(sb: &[u8]) -> Label {
        let mut l = Label { b: [0; 64], n: 0 };
        for i in 0..64usize {
            let at = SB_LABEL_OFFSET + 2 * i;
            if at + 2 > sb.len() { break; }
            let u = u16::from_le_bytes([sb[at], sb[at + 1]]);
            if u == 0 { break; }
            l.b[l.n] = if u < 128 && u >= 0x20 { u as u8 } else { b'?' };
            l.n += 1;
        }
        l
    }

    /// One tri-state probe of a candidate partition's superblock.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Probe {
        /// The read failed (not "no": the caller retries and then falls back).
        Err,
        NotF2fs,
        F2fs { labelled: bool },
    }

    /// Classify a superblock slice that was read successfully.
    pub fn probe_sb(sb: &[u8]) -> Probe {
        if sb.len() < 4 || u32::from_le_bytes([sb[0], sb[1], sb[2], sb[3]]) != F2FS_MAGIC { return Probe::NotF2fs; }
        Probe::F2fs { labelled: sb_label(sb).as_str() == super::sdgrant::LEANDROS_LABEL }
    }

    /// MBR disk signature (bytes 440..444), Linux's PARTUUID prefix.
    pub fn disk_id(mbr: &[u8]) -> u32 {
        if mbr.len() < 444 { return 0; }
        u32::from_le_bytes([mbr[440], mbr[441], mbr[442], mbr[443]])
    }

    /// The picked root partition.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Pick { pub lba: u64, pub pno: u8, pub labelled: bool }

    /// Pick the F2FS root among the MBR's block-aligned 0x83 partitions,
    /// preferring one labelled `leandros-root` over the first F2FS. `Err(())`
    /// when any probe read failed (never "not found" on a transient error).
    pub fn pick_root(mbr: &[u8], mut probe_at: impl FnMut(u64) -> Probe) -> Result<Option<Pick>, ()> {
        if mbr.len() < 512 || mbr[510] != 0x55 || mbr[511] != 0xAA { return Ok(None); }
        let mut best: Option<Pick> = None;
        for i in 0..4usize {
            let e = &mbr[446 + i * 16..446 + i * 16 + 16];
            if e[4] != 0x83 { continue; }
            let lba = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
            if lba == 0 || lba % 8 != 0 { continue; }
            match probe_at(lba) {
                Probe::Err => return Err(()),
                Probe::NotF2fs => {}
                Probe::F2fs { labelled } => {
                    if best.map_or(true, |b| labelled && !b.labelled) {
                        best = Some(Pick { lba, pno: (i + 1) as u8, labelled });
                    }
                }
            }
        }
        Ok(best)
    }

    /// Whether the sector window `[first, end)` is free: a valid MBR and no
    /// partition entry (any type) overlapping it.
    pub fn window_free(mbr: &[u8], first: u64, end: u64) -> bool {
        if mbr.len() < 512 || mbr[510] != 0x55 || mbr[511] != 0xAA { return false; }
        for i in 0..4usize {
            let e = &mbr[446 + i * 16..446 + i * 16 + 16];
            let ty = e[4];
            let lba = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
            let n = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
            if ty == 0 && lba == 0 && n == 0 { continue; }
            if lba < end && first < lba.saturating_add(n.max(1)) { return false; }
        }
        true
    }

    /// The bytes of the card the mounted root F2FS owns: raw writes that
    /// overlap them are EBUSY on every SD node (whole card, partitions).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct RootGuard {
        /// Start of the root partition, absolute 512-byte sector.
        pub start_lba: u64,
        /// Protected length in bytes from `start_lba`.
        pub bytes: u64,
        /// True when `bytes` is the superblock size (block_count <<
        /// log_blocksize); false when it is the MBR entry length (or, with a
        /// zero entry length, "rest of the card": `u64::MAX / 2`).
        pub from_sb: bool,
    }

    impl RootGuard {
        /// Absolute sector just past the protected bytes.
        pub fn end_lba(&self) -> u64 { self.start_lba.saturating_add(self.bytes.div_ceil(512)) }
        /// Whether a write of `len` bytes at absolute card byte `at` overlaps.
        pub fn overlaps(&self, at: u64, len: u64) -> bool {
            let s = self.start_lba * 512;
            len > 0 && at < s.saturating_add(self.bytes) && s < at.saturating_add(len)
        }
    }

    /// F2FS size in bytes from a superblock slice (the 1024..1024+x bytes of
    /// the partition): `None` without the magic or with a bogus block size.
    pub fn f2fs_bytes(sb: &[u8]) -> Option<u64> {
        if sb.len() < 44 || u32::from_le_bytes(sb[0..4].try_into().ok()?) != F2FS_MAGIC { return None; }
        let log_bs = u32::from_le_bytes(sb[16..20].try_into().ok()?);
        let blocks = u64::from_le_bytes(sb[36..44].try_into().ok()?);
        if (9..=16).contains(&log_bs) { Some(blocks << log_bs) } else { None }
    }

    /// Root partition and its guard extent. `read_blk(abs_blk, buf)` reads
    /// the 4096-byte block `abs_blk` of the WHOLE card (absolute: block 0 is
    /// the MBR); it must never be a reader that already adds the root
    /// partition offset (the 2026-11-20 bug: the superblock was looked up at
    /// twice the partition offset, the size stayed unknown and the guard
    /// covered everything from p2 to the end of the card).
    pub fn root_guard(mbr: &[u8], read_blk: impl FnMut(u64, &mut [u8; 4096]) -> bool) -> Option<RootGuard> {
        root_guard_t(mbr, read_blk).ok().flatten().map(|(g, _)| g)
    }

    /// `root_guard` with the picked partition and a tri-state result: `Err`
    /// when a candidate's superblock could not be read.
    pub fn root_guard_t(mbr: &[u8], mut read_blk: impl FnMut(u64, &mut [u8; 4096]) -> bool) -> Result<Option<(RootGuard, Pick)>, ()> {
        let mut buf = [0u8; 4096];
        let sb_at = |lba: u64| (lba * 512 + F2FS_SB_OFFSET) / 4096;
        let sb_off = |lba: u64| ((lba * 512 + F2FS_SB_OFFSET) % 4096) as usize;
        let pick = pick_root(mbr, |lba| {
            let o = sb_off(lba);
            if o + 4 > 4096 || !read_blk(sb_at(lba), &mut buf) { return Probe::Err; }
            probe_sb(&buf[o..])
        })?;
        let Some(pick) = pick else { return Ok(None) };
        let start = pick.lba;
        let o = sb_off(start);
        let fs = if o + 44 <= 4096 && read_blk(sb_at(start), &mut buf) { f2fs_bytes(&buf[o..]) } else { None };
        if let Some(bytes) = fs {
            return Ok(Some((RootGuard { start_lba: start, bytes, from_sb: true }, pick)));
        }
        // Unknown size: the partition's MBR entry, else the rest of the card.
        let len = (0..4usize).map(|i| &mbr[446 + i * 16..462 + i * 16])
            .find(|e| e[4] == 0x83 && u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64 == start)
            .map_or(0, |e| u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64 * 512);
        Ok(Some((RootGuard { start_lba: start, bytes: if len == 0 { u64::MAX / 2 } else { len }, from_sb: false }, pick)))
    }
}

// ── SD write grant: LeandrOS writes only a card that is its own ──────────────
pub mod sdgrant {
    use super::Regs32;

    pub const R_OK: u8 = 0;
    pub const R_NO_F2FS: u8 = 1;
    pub const R_RW_OFF: u8 = 2;
    pub const R_OTHER_ROOT: u8 = 3;
    pub const R_NO_LABEL: u8 = 4;
    pub const R_BAD_CMDLINE: u8 = 5;
    pub const R_READ_ERROR: u8 = 6;

    /// The F2FS volume label LeandrOS's installer and image builders write.
    pub const LEANDROS_LABEL: &str = "leandros-root";
    /// Kernel command line capacity (boot::COMMAND_LINE_SIZE) minus the NUL:
    /// a line this long was cut.
    pub const CMDLINE_TRUNCATED_AT: usize = 2047;

    pub fn reason_str(r: u8) -> &'static str {
        match r {
            R_OK => "ok",
            R_NO_F2FS => "no-f2fs-magic",
            R_RW_OFF => "sdhci.rw=0",
            R_OTHER_ROOT => "root=-names-another-device",
            R_NO_LABEL => "label-is-not-leandros-root",
            R_BAD_CMDLINE => "cmdline-unreadable-or-without-root",
            R_READ_ERROR => "read-error",
            _ => "unknown",
        }
    }

    /// What the grant decision knows about the picked partition.
    #[derive(Clone, Copy, Debug)]
    pub struct Ctx<'a> {
        /// The F2FS magic was read (tri-state read, through the final offset).
        pub found: bool,
        /// The superblock volume label of the picked partition.
        pub label: &'a str,
        /// MBR disk signature (bytes 440..444, little endian).
        pub disk_id: u32,
        /// MBR partition number (1-4) of the picked partition; 0 = none (whole device).
        pub pno: u8,
        pub rw: bool,
        /// `None`: the command line was never delivered / unreadable.
        pub cmdline: Option<&'a str>,
        /// A real board (rpi5): its firmware always passes root=, so a line
        /// without one is not evidence the card is ours.
        pub require_root: bool,
    }

    /// Printable ASCII (and blanks), short of the truncation point.
    pub fn cmdline_valid(c: &str) -> bool {
        c.len() < CMDLINE_TRUNCATED_AT && c.bytes().all(|b| (0x20..0x7f).contains(&b) || b == b'\t' || b == b'\n')
    }

    fn hex(s: &str) -> Option<u32> {
        if s.is_empty() || s.len() > 8 || !s.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
        u32::from_str_radix(s, 16).ok()
    }

    /// `PARTUUID=` in Linux's MBR form `<disk id %08x>-<partition %02x>`.
    pub fn partuuid_matches(v: &str, disk_id: u32, pno: u8) -> bool {
        if pno == 0 { return false; }
        let Some((a, b)) = v.split_once('-') else { return false };
        a.len() == 8 && b.len() <= 2 && hex(a) == Some(disk_id) && hex(b) == Some(pno as u32)
    }

    /// Whether the kernel `root=` value (`/dev/vdb`, `/dev/mmcblk0pN` with N
    /// the picked partition, `LABEL=<this partition's label>`,
    /// `PARTUUID=<disk id>-<N>`) names this card. Another label / partition
    /// number / device: no.
    pub fn root_names_this_card(root: &str, c: &Ctx) -> bool {
        if let Some(l) = root.strip_prefix("LABEL=") { return !c.label.is_empty() && l == c.label; }
        if let Some(p) = root.strip_prefix("PARTUUID=") { return partuuid_matches(p, c.disk_id, c.pno); }
        if root == "/dev/vdb" { return true; }
        match root.strip_prefix("/dev/mmcblk0p") {
            Some(n) => n.len() <= 3 && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                && c.pno != 0 && n.parse::<u32>().ok() == Some(c.pno as u32),
            None => false,
        }
    }

    /// The write-grant decision, evaluated once at the end of SD init:
    /// `(granted, reason)`. Grant iff the F2FS magic was found AND `rw` AND a
    /// readable command line AND the volume label is `leandros-root` (or
    /// `sdhci.trust=<PARTUUID of the picked partition>`) AND (no `root=` on a
    /// board that need not have one | it names this card).
    pub fn evaluate(c: &Ctx) -> (bool, u8) {
        if !c.found { return (false, R_NO_F2FS); }
        if !c.rw { return (false, R_RW_OFF); }
        let cl = match c.cmdline {
            Some(s) if cmdline_valid(s) => s,
            // QEMU raspi4b boots with no command line at all; a real board never does.
            None if !c.require_root => "",
            _ => return (false, R_BAD_CMDLINE),
        };
        let trusted = cl.split_ascii_whitespace().filter_map(|t| t.strip_prefix("sdhci.trust=")).last()
            .map_or(false, |v| partuuid_matches(v, c.disk_id, c.pno));
        if !trusted && c.label != LEANDROS_LABEL { return (false, R_NO_LABEL); }
        let root = cl.split_ascii_whitespace().filter_map(|t| t.strip_prefix("root=")).last();
        match root {
            Some(r) if r.is_empty() || !root_names_this_card(r, c) => (false, R_OTHER_ROOT),
            Some(_) => (true, R_OK),
            None if c.require_root => (false, R_BAD_CMDLINE),
            None => (true, R_OK),
        }
    }

    /// Commands that change card contents or state: CMD24/25 (write), 27 (CSD
    /// program), 28/29 (write protect), 32/33/38 (erase), 42 (lock/unlock),
    /// 56 (general command, write direction), and ACMD23 (pre-erase count).
    /// Used for the issued-writes counter only; the gate is `locked_allows`.
    pub fn is_write_cmd(index: u8, app: bool) -> bool {
        if app { index == 23 } else { matches!(index, 24 | 25 | 27 | 28 | 29 | 32 | 33 | 38 | 42 | 56) }
    }

    /// The only commands a LOCKED card ever sees: identification, bus setup,
    /// status, reads, tuning and CMD55 (CMD 0,2,3,6,7,8,9,11,12,13,16,17,18,19,55;
    /// ACMD 6,13,41,51). Everything else is denied whatever `app` says.
    pub fn locked_allows(index: u8, app: bool) -> bool {
        if app { matches!(index, 6 | 13 | 41 | 51) }
        else { matches!(index, 0 | 2 | 3 | 6 | 7 | 8 | 9 | 11 | 12 | 13 | 16 | 17 | 18 | 19 | 55) }
    }

    /// May this command go out? A granted card: all. Locked: the allowlist.
    pub fn allows(granted: bool, index: u8, app: bool) -> bool { granted || locked_allows(index, app) }

    /// Whether a CMD55 response (R1) means the next command is an ACMD: the
    /// card did not flag ILLEGAL_COMMAND (bit 22).
    pub fn app_cmd_accepted(r1: u32) -> bool { r1 & (1 << 22) == 0 }

    /// SDHCI Argument 1 / Command register offsets (the 16-bit Command
    /// register is written through its 32-bit-aligned window in the mock).
    pub const REG_ARGUMENT1: usize = 0x08;
    pub const REG_COMMAND: usize = 0x0E;

    /// The command-issue step of the driver in seam form: refuse (no register
    /// access at all) or program Argument 1 then the Command register.
    pub fn issue(regs: &mut impl Regs32, granted: bool, index: u8, app: bool, arg: u32, word: u16) -> bool {
        if !allows(granted, index, app) { return false; }
        regs.w(REG_ARGUMENT1, arg);
        regs.w(REG_COMMAND, word as u32);
        true
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
        // Magic unreadable / foreign ext4 only (another OS card): no root, never the first 0x83.
        let m = mbr(&[(0x0c, 8192), (0x83, 1_056_768)]);
        assert_eq!(sdmbr::pick_f2fs(&m, |_| None), None);
        // No signature / no Linux partition.
        let mut bad = m; bad[511] = 0;
        assert_eq!(sdmbr::pick_f2fs(&bad, |_| None), None);
        assert_eq!(sdmbr::pick_f2fs(&mbr(&[(0x0c, 8192)]), |_| None), None);
    }

    /// The real Pi card after the dual-boot repartition (2026-11-20): p2 is
    /// exactly the F2FS (936960 x 4 KiB), p3 FAT32 at 8503296, p4 after it.
    fn real_card() -> ([u8; 512], impl FnMut(u64, &mut [u8; 4096]) -> bool) {
        let mut m = [0u8; 512];
        for (i, &(ty, lba, n)) in [(0x0bu8, 2048u32, 1_000_000u32), (0x83, 1_003_520, 7_495_680),
                                     (0x0c, 8_503_296, 1_048_576), (0x83, 9_551_872, 240_447_488)].iter().enumerate() {
            let e = &mut m[446 + i * 16..462 + i * 16];
            e[4] = ty; e[8..12].copy_from_slice(&lba.to_le_bytes()); e[12..16].copy_from_slice(&n.to_le_bytes());
        }
        m[510] = 0x55; m[511] = 0xAA;
        let mut sb = [0u8; 4096];
        sb[1024..1028].copy_from_slice(&sdmbr::F2FS_MAGIC.to_le_bytes());
        sb[1024 + 16..1024 + 20].copy_from_slice(&12u32.to_le_bytes());
        sb[1024 + 36..1024 + 44].copy_from_slice(&936_960u64.to_le_bytes());
        let p4 = { let mut b = [0u8; 4096]; b[1024 + 0x38..1024 + 0x3a].copy_from_slice(&0xEF53u16.to_le_bytes()); b };
        let mbr4k = { let mut b = [0u8; 4096]; b[..512].copy_from_slice(&m); b };
        (m, move |blk: u64, buf: &mut [u8; 4096]| {
            *buf = match blk { 0 => mbr4k, 125_440 => sb, 1_193_984 => p4, _ => [0u8; 4096] };
            true
        })
    }

    #[test]
    fn root_guard_real_card_extent() {
        let (m, rd) = real_card();
        let g = sdmbr::root_guard(&m, rd).unwrap();
        assert_eq!(g, sdmbr::RootGuard { start_lba: 1_003_520, bytes: 3_837_788_160, from_sb: true });
        assert_eq!(g.end_lba(), 8_499_200);
        // The writes that failed on the Pi with EBUSY must pass the guard:
        // p3 / p4 via their nodes (partition start + offset), and the whole
        // card at `bs=4M seek=1038`.
        assert!(!g.overlaps(8_503_296 * 512, 4 << 20));
        assert!(!g.overlaps(9_551_872 * 512, 4 << 20));
        assert!(!g.overlaps(1038 * (4 << 20), 4 << 20));
        assert!(!g.overlaps(8_499_200 * 512, 4096)); // first byte past the fs
        // sdraw's earlier safe test write (fs end + 64 MiB).
        assert!(!g.overlaps(4_418_699_264, 1 << 20));
        // Anything touching the fs stays refused.
        assert!(g.overlaps(8_499_200 * 512 - 1, 4096));
        assert!(g.overlaps(1_003_520 * 512, 1));
        assert!(g.overlaps(0, u64::MAX / 4));
        assert!(!g.overlaps(1_003_520 * 512 - 4096, 4096)); // p1 end / gap
    }

    #[test]
    fn root_guard_never_unbounded_with_an_mbr_size() {
        // The 2026-11-20 bug: the superblock read through a reader that
        // already adds the partition offset (block 2 x 125440) finds no
        // magic. The guard must then fall back to p2's MBR length, not
        // "everything from p2 to the end of the card".
        // (A card with no readable magic is no longer a root at all: pick_f2fs
        // returns None. The fallback is for a magic whose size fields are bogus.)
        let (m, _) = real_card();
        let bogus_sb = |b: u64, buf: &mut [u8; 4096]| {
            *buf = [0u8; 4096];
            if b == 125_440 { buf[1024..1028].copy_from_slice(&sdmbr::F2FS_MAGIC.to_le_bytes()); }
            true
        };
        let g = sdmbr::root_guard(&m, bogus_sb).unwrap();
        assert_eq!(g, sdmbr::RootGuard { start_lba: 1_003_520, bytes: 7_495_680 * 512, from_sb: false });
        assert!(!g.overlaps(8_503_296 * 512, 4 << 20));
        // The old premise: the reader that adds the partition offset finds no magic.
        let (m2, mut rd) = real_card();
        assert_eq!(sdmbr::root_guard(&m2, |b, buf| rd(if b == 0 { 0 } else { b + 125_440 }, buf)), None);
        // Zero-length MBR entry and a bogus superblock: rest of the card.
        let mut z = m; z[446 + 16 + 12..446 + 16 + 16].fill(0);
        let g = sdmbr::root_guard(&z, bogus_sb).unwrap();
        assert_eq!((g.start_lba, g.bytes, g.from_sb), (1_003_520, u64::MAX / 2, false));
    }
}
