//! SD Host Controller (SDHCI Simplified Specification 3.0, polled PIO)
//! block device driver.
//!
//! Public API mirrors `virtio_blk.rs` exactly (`init`, `device_count`,
//! `read_block`, `write_block`, `has_f2fs`, 4096-byte block granularity) so
//! `drivers::blkdev` (see `lib.rs`) can select between the two at compile
//! time with no call-site changes anywhere else in the tree.
//!
//! ## Base address provenance
//!
//! **`rpi5` feature (real Raspberry Pi 5 / BCM2712)** — decompiled directly
//! from the vendored `target/rpi5-uefi/bcm2712-rpi-5-b.dtb` via `dtc`:
//! ```text
//! mmc@fff000 {
//!     compatible = "brcm,bcm2712-sdhci";
//!     reg = <0x10 0xfff000 0x00 0x260 ...>;   // "host" window
//!     interrupts = <0x00 0x111 0x04>;          // unused — this driver polls
//!     bus-width = <0x04>; status = "okay";
//! };
//! ```
//! i.e. physical base `0x1000_fff000`, register window `0x260` bytes — the
//! standard `0x100`-byte SDHCI block plus `0x160` bytes of Broadcom vendor
//! extensions this driver does not touch. Do not confuse this DTB node with
//! `mmc@1100000` (`non-removable`, has a `wifi@1` child — the onboard SDIO
//! wireless chip) or `mmc@1108000` (`compatible = "brcm,bcm2711-emmc2"`,
//! `status = "disabled"` — an unused legacy-compat node): neither is the
//! physical SD card slot.
//!
//! This path is compiled but **cannot be exercised in QEMU** under any
//! machine model available at the time this was written (no `raspi5`
//! machine exists; `qemu-system-aarch64 -machine help` tops out at
//! `raspi4b`, and `-machine virt` has no SD/eMMC peripheral at all) —
//! verification is review plus eventual physical-hardware boot only.
//!
//! **`raspi4b` feature (QEMU `-M raspi4b`, BCM2711)** — a *testable stepping
//! stone only*, not a hardware target, and not representative of real
//! BCM2712 register offsets. Verified live this session via QMP
//! `info qtree`/`info mtree`: two `generic-sdhci` instances exist (standard
//! SDHCI 3.0, `capareg = 0x52134b4`, no Broadcom quirks) at `0xfe300000`
//! (no card) and `0xfe340000`; a `-drive if=sd,...` attaches its `sd-card`
//! child to the **second**, `0xfe340000` — the same "SD card rides the
//! EMMC2-style controller" convention real BCM2712 uses.
//!
//! **The long-standing `raspi4b` "CMD8 always times out" issue was this
//! driver pointing at the wrong one of the two controllers, and is fixed.**
//! The earlier reading — that QEMU's `sd_do_command()` bails out because
//! `blk_is_inserted()` is false for the `sd-card` object on this bus — had the
//! symptom right and the cause wrong. `PRESENT_STATE`'s Card Inserted latch
//! does read 0, but only at `0xfe340000`; the card is on the *other* instance.
//! Read out of a live QEMU 11.0.2 with the guest stopped (`-S`) over QMP:
//!
//! ```text
//! (qemu) info qtree
//!   dev: generic-sdhci …  capareg = 0x052134b4   bus: sd-bus (empty)
//!   dev: generic-sdhci …  capareg = 0x052134b4   bus: sd-bus
//!     dev: sd-card …      drive = "sd0"          ← the -drive if=sd backend
//! (qemu) info mtree -f
//!   00000000fe300000-00000000fe3000ff (prio 0, i/o): sdhci
//!   00000000fe340000-00000000fe3400ff (prio 0, i/o): sdhci
//! (qemu) xp /1wx 0xfe300024 → 0x01ff0000   Present State bit 16 = 1
//! (qemu) xp /1wx 0xfe340024 → 0x01fa0000   Present State bit 16 = 0
//! ```
//!
//! The two instances are indistinguishable by capabilities — identical
//! `capareg`, identical Host Controller Version (`0x2402`: vendor 0x24, spec
//! 0x02 = SDHCI 3.0) — which is exactly why the empty one looked healthy right
//! up to the first command that needed a card to answer it. QEMU's clock tree
//! agrees with the Present State bits: the machine exposes `emmc-out` at
//! 200 MHz and `emmc2-out` at 0 Hz, i.e. only the legacy Arasan controller at
//! 0xfe300000 is wired up at all. Note this is a QEMU modelling choice and
//! *not* how real BCM2711 hardware is arranged (where the SD slot rides EMMC2
//! at 0x7e340000) — one more reason this path is a protocol test bench and not
//! a hardware target.

use spin::Mutex;
use mm;

/// Bring-up marker straight to the Pi's debug PL011, independent of the console
/// stack. This driver runs before userspace and had never touched real silicon;
/// when it stalls, the only question worth answering is *which* wait stalled.
#[cfg(feature = "rpi5")]
fn dbg(c: u8) {
    const BASE: usize = 0x107D_0010_00;
    unsafe {
        let mut spins = 0u32;
        while core::ptr::read_volatile((BASE + 0x18) as *const u32) & (1 << 5) != 0 {
            spins += 1;
            if spins > 5_000_000 { return; }
        }
        core::ptr::write_volatile(BASE as *mut u32, c as u32);
    }
}

/// The same markers on the QEMU `raspi4b` test path, through BCM2711's UART0
/// (peripheral base 0xFE000000 + offset 0x201000 — the address
/// `arch::aarch64::uart::BASE` uses for this feature).
///
/// This arm used to be a no-op, which made the stepping-stone machine useless
/// for anything that reports rather than hangs: the bus negotiation below is
/// almost entirely decisions, and a decision that prints nothing can only be
/// checked on hardware. Unlike the rpi5 arm this one goes through
/// `mm::phys_to_virt`, matching how `init` reaches the controller itself on
/// this path.
#[cfg(feature = "raspi4b")]
fn dbg(c: u8) {
    let base = mm::phys_to_virt(0xFE20_1000);
    unsafe {
        let mut spins = 0u32;
        while core::ptr::read_volatile((base + 0x18) as *const u32) & (1 << 5) != 0 {
            spins += 1;
            if spins > 5_000_000 { return; }
        }
        core::ptr::write_volatile(base as *mut u32, c as u32);
    }
}

#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
fn dbg(_c: u8) {}

/// `dbg` a value as `<hex>`, for the failure paths below.
fn dbg_hex(v: u64) {
    dbg(b'<');
    for i in (0..16).rev() {
        let nib = ((v >> (i * 4)) & 0xf) as u8;
        dbg(if nib < 10 { b'0' + nib } else { b'a' + nib - 10 });
    }
    dbg(b'>');
}

/// `dbg` a whole string. Only the bus-negotiation summary uses this — single
/// characters remain the convention for per-stage markers, because a marker has
/// to be cheap enough to sit inside a spin loop.
fn dbg_str(s: &str) {
    for b in s.as_bytes() {
        dbg(*b);
    }
}

/// `dbg` a value in decimal. Frequencies are the one thing in this driver a
/// human reads rather than decodes, so the summary line prints them base 10.
fn dbg_dec(mut v: u32) {
    if v == 0 {
        dbg(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut n = 0usize;
    while v > 0 {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        dbg(buf[n]);
    }
}

/// Iterations any hardware handshake gets before we call it dead.
///
/// Every wait in this driver used to be an unbounded `loop`. That is fine on a
/// controller that answers, and on QEMU it always did; on real BCM2712 the
/// first one that does not answer wedges the whole boot with no output, which
/// is exactly how this driver presented on its first hardware run. A bounded
/// wait turns "the machine is hung" into "this stage timed out", which is a
/// bug report rather than a mystery.
const SPIN_LIMIT: u32 = 20_000_000;

/// `(NORMAL_INT_STATUS << 16) | ERROR_INT_STATUS` as they stood at the moment
/// the last transfer gave up, latched before they are cleared.
static LAST_FAIL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
use core::sync::atomic::Ordering;

// ── Base address ─────────────────────────────────────────────────────────────

#[cfg(feature = "rpi5")]
const SDHCI_BASE: usize = 0x1000_fff000;

/// QEMU `-M raspi4b` (BCM2711). **0xfe300000, not 0xfe340000** — see the
/// top-of-file note: this is the controller QEMU actually plugs the `-drive
/// if=sd` card into. Established by reading both controllers' Present State
/// registers out of a live instance over QMP with the guest stopped:
///
/// ```text
/// xp /1wx 0xfe300024 → 0x01ff0000   bit 16 Card Inserted = 1
/// xp /1wx 0xfe340024 → 0x01fa0000   bit 16 Card Inserted = 0
/// ```
///
/// Both instances exist and both report the same `capareg` (0x052134b4), which
/// is why the wrong one looked entirely healthy right up until the first
/// command that needed a card to answer it.
#[cfg(feature = "raspi4b")]
const SDHCI_BASE: usize = 0xfe30_0000;

#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
compile_error!("drivers::sdhci requires either the rpi5 or raspi4b feature");

// ── Register offsets (SDHCI Simplified Spec 3.0) ─────────────────────────────

const REG_BLOCK_SIZE:         usize = 0x04; // u16
const REG_BLOCK_COUNT:        usize = 0x06; // u16
const REG_ARGUMENT1:          usize = 0x08; // u32
const REG_TRANSFER_MODE:      usize = 0x0C; // u16
const REG_COMMAND:            usize = 0x0E; // u16
const REG_RESPONSE0:          usize = 0x10; // u32
const REG_BUFFER_DATA:        usize = 0x20; // u32, PIO port
const REG_PRESENT_STATE:      usize = 0x24; // u32
const REG_HOST_CONTROL1:      usize = 0x28; // u8
const REG_POWER_CONTROL:      usize = 0x29; // u8
const REG_CLOCK_CONTROL:      usize = 0x2C; // u16
const REG_TIMEOUT_CONTROL:    usize = 0x2E; // u8
const REG_SOFTWARE_RESET:     usize = 0x2F; // u8
const REG_NORMAL_INT_STATUS:  usize = 0x30; // u16, W1C
const REG_ERROR_INT_STATUS:   usize = 0x32; // u16, W1C
const REG_NORMAL_INT_ENABLE:  usize = 0x34; // u16 (status-enable; signal/IRQ-enable left off — polled driver)
const REG_ERROR_INT_ENABLE:   usize = 0x36; // u16
const REG_CAPABILITIES:       usize = 0x40; // u32 (read-only)
const REG_HOST_VERSION:       usize = 0xFE; // u16, [7:0] = spec version

// Present State bits
const PSTATE_CMD_INHIBIT: u32 = 1 << 0;
const PSTATE_DAT_INHIBIT: u32 = 1 << 1;

// Normal Interrupt Status bits
const NI_CMD_COMPLETE:     u16 = 1 << 0;
const NI_XFER_COMPLETE:    u16 = 1 << 1;
const NI_BUF_WRITE_READY:  u16 = 1 << 4;
const NI_BUF_READ_READY:   u16 = 1 << 5;
const NI_ERROR:            u16 = 1 << 15;
const NI_ALL:              u16 = 0xFFFF;

const EI_ALL: u16 = 0xFFFF;

// Software Reset bits
const SWRST_ALL: u8 = 1 << 0;
/// Reset just the CMD and DAT circuits. SDHCI 3.0 §3.10: after an error the
/// host driver is *required* to reset these before issuing anything else,
/// otherwise Command/DAT Inhibit stay asserted and every later command is
/// refused. This driver never did, so the first failed transfer wedged the
/// controller permanently — visible as PRESENT_STATE stuck at 0x…0207
/// (CMD inhibit | DAT inhibit | DAT line active) for every subsequent access.
const SWRST_CMD: u8 = 1 << 1;
const SWRST_DAT: u8 = 1 << 2;

// Host Control 1 bits touched by the bus negotiation. Everything else in this
// register (LED, DMA select, card-detect override) is left at its reset value.
const HC1_DATA_WIDTH_4: u8 = 1 << 1;
const HC1_HIGH_SPEED:   u8 = 1 << 2;

// Clock Control bits
const CLK_INTERNAL_EN: u16 = 1 << 0;
const CLK_STABLE:      u16 = 1 << 1;
const CLK_SD_EN:       u16 = 1 << 2;
/// Slowest divided-clock setting, used for card identification and as the floor
/// the negotiation below can always retreat to. Read as SDHCI 3.0's 10-bit
/// field this is N = 255, i.e. base/510 — under 400 kHz for any base clock up
/// to 204 MHz, which is what the identification phase requires.
const CLK_DIV_MAX:     u16 = 0xFF << 8;

/// Base clock to assume when `CAPABILITIES[15:8]` reads zero. The spec treats a
/// zero there as "this register cannot answer, ask elsewhere", and that is a
/// real possibility on this part: Linux's `sdhci-iproc` installs its own
/// `get_max_clock` for the BCM271x family and takes the rate from the clock
/// tree rather than the register.
///
/// "Elsewhere" for this board is the device tree. The vendored
/// `target/rpi5-uefi/bcm2712-rpi-5-b.dtb` gives `mmc@fff000` a
/// `clocks = <&clk_emmc2>` phandle, and that node reads
/// `clk_emmc2 { compatible = "fixed-clock"; clock-frequency = <0x337f980>; }`
/// — 54 MHz. (The VideoCore mailbox `GET_CLOCK_RATE` tag for the EMMC clock is
/// the other conventional source, but this driver has no mailbox and adding one
/// to answer a question the capabilities register normally answers is not worth
/// the code.) The same DTB node masks only `CAPABILITIES_1`'s re-tuning bits
/// (`sdhci-caps-mask = <0xc000 0x00>`, a 64-bit value whose *upper* half lands
/// on 0x44) and nothing at all in `CAPABILITIES` — so Linux does trust the base
/// clock field on this board, and this constant should stay unused in practice.
///
/// Guessing wrong is survivable by construction: every clock rung below is
/// proved by a live read-back, so a base that is too low just means the top
/// rungs do not verify and the ladder stops further down.
#[cfg(feature = "rpi5")]
const BASE_CLOCK_FALLBACK_KHZ: u32 = 54_000;
/// QEMU's `generic-sdhci` reports `capareg = 0x052134b4` — a 52 MHz base — so
/// this arm is unreachable there; it exists only so the code below has no
/// `cfg`-dependent hole.
#[cfg(feature = "raspi4b")]
const BASE_CLOCK_FALLBACK_KHZ: u32 = 50_000;

// Response type select (Command register bits [1:0])
const RESP_NONE: u16 = 0b00;
const RESP_136:  u16 = 0b01; // R2
const RESP_48:   u16 = 0b10; // R1, R3, R6, R7
const RESP_48B:  u16 = 0b11; // R1b (busy)

const CMD_CRC_CHECK: u16 = 1 << 3;
const CMD_IDX_CHECK: u16 = 1 << 4;
const CMD_DATA_PRESENT: u16 = 1 << 5;

// Transfer Mode bits
const TM_BLOCK_COUNT_EN: u16 = 1 << 1;
const TM_DIR_READ:       u16 = 1 << 4;

// SD command indices used by this driver
const CMD_GO_IDLE_STATE:      u8 = 0;
const CMD_SEND_IF_COND:       u8 = 8;
const CMD_ALL_SEND_CID:       u8 = 2;
const CMD_SEND_RELATIVE_ADDR: u8 = 3;
const CMD_SWITCH_FUNC:        u8 = 6;
const CMD_SELECT_CARD:        u8 = 7;
const CMD_SEND_CSD:           u8 = 9;
const CMD_SET_BLOCKLEN:       u8 = 16;
const CMD_READ_SINGLE_BLOCK:  u8 = 17;
const CMD_WRITE_BLOCK:        u8 = 24;
const CMD_APP_CMD:            u8 = 55;
/// Same index as `CMD_SWITCH_FUNC` — an SD command is only an *application*
/// command by virtue of the CMD55 that precedes it, never by its number.
const ACMD_SET_BUS_WIDTH:     u8 = 6;
const ACMD_SD_SEND_OP_COND:   u8 = 41;
const ACMD_SEND_SCR:          u8 = 51;

// CMD6 (SWITCH_FUNC) arguments. Bit 31 is the mode (0 = check whether a
// function exists, 1 = actually switch to it); the remaining six nibbles select
// one function per function group, and 0xF means "no change" for that group.
// Group 1 is the access mode, and its function 1 is High Speed / SDR25.
const SWITCH_CHECK_HS: u32 = 0x00FF_FFF1;
const SWITCH_SET_HS:   u32 = 0x80FF_FFF1;
/// Group 1 function 0 — back to Default Speed. Only used to walk a card back
/// out of High Speed if the host turns out not to be able to hold it.
const SWITCH_SET_DS:   u32 = 0x80FF_FFF0;
/// Bytes of switch-function status the card returns on DAT for CMD6.
const SWITCH_STATUS_LEN: usize = 64;

const SECTOR_SIZE: usize = 512;
const BLOCK_SIZE: usize = 4096;
const SECTORS_PER_BLOCK: usize = BLOCK_SIZE / SECTOR_SIZE;

const F2FS_MAGIC: u32 = 0xF2F5_2010;
const F2FS_SB_OFFSET: usize = 1024;

const MAX_BLK_DEVICES: usize = 8;

// ── Register access ───────────────────────────────────────────────────────────

struct SdhciDevice {
    base: usize, // HHDM virtual address
    high_capacity: bool,
    /// 4096-byte-block offset of the F2FS partition on real hardware (real
    /// SD cards carry an MBR partition table — see `find_f2fs_partition`).
    /// Always 0 for the `raspi4b` QEMU test image, which has no partition
    /// table and starts F2FS directly at block 0.
    partition_offset: u64,
}

unsafe impl Send for SdhciDevice {}

impl SdhciDevice {
    unsafe fn r8(&self, off: usize) -> u8 { ((self.base + off) as *const u8).read_volatile() }
    unsafe fn w8(&self, off: usize, v: u8) { ((self.base + off) as *mut u8).write_volatile(v) }
    unsafe fn r16(&self, off: usize) -> u16 { ((self.base + off) as *const u16).read_volatile() }
    unsafe fn w16(&self, off: usize, v: u16) { ((self.base + off) as *mut u16).write_volatile(v) }
    unsafe fn r32(&self, off: usize) -> u32 { ((self.base + off) as *const u32).read_volatile() }
    unsafe fn w32(&self, off: usize, v: u32) { ((self.base + off) as *mut u32).write_volatile(v) }

    /// Wait for `bits` to be set in the Normal Interrupt Status register,
    /// clearing whichever of (bits | error) actually fired. Returns false on
    /// a genuine controller-reported error (e.g. command timeout — the
    /// realistic "no card inserted" case), never hangs forever on that path.
    unsafe fn wait_normal_int(&self, bits: u16) -> bool {
        let mut spins: u32 = 0;
        loop {
            let status = self.r16(REG_NORMAL_INT_STATUS);
            if status & NI_ERROR != 0 {
                // Latch both registers before clearing them. `do_io`'s failure
                // report used to read them afterwards and always printed zero,
                // destroying the only evidence of what actually went wrong.
                let err = self.r16(REG_ERROR_INT_STATUS);
                LAST_FAIL.store(((status as u32) << 16) | err as u32, Ordering::Relaxed);
                self.w16(REG_NORMAL_INT_STATUS, NI_ALL);
                self.w16(REG_ERROR_INT_STATUS, EI_ALL);
                return false;
            }
            if status & bits == bits {
                self.w16(REG_NORMAL_INT_STATUS, bits);
                return true;
            }
            spins += 1;
            if spins > SPIN_LIMIT {
                let err = self.r16(REG_ERROR_INT_STATUS);
                LAST_FAIL.store(((status as u32) << 16) | err as u32, Ordering::Relaxed);
                dbg(b'!');
                return false;
            }
            core::hint::spin_loop();
        }
    }

    /// SDHCI 3.0 §3.10 error recovery: reset the CMD and DAT circuits and clear
    /// both interrupt-status registers, so whatever comes next starts from a
    /// controller that is not still asserting inhibit from the failure.
    ///
    /// Deliberately *not* `SWRST_ALL`: that would also clear Clock Control and
    /// Host Control 1, silently undoing a negotiated bus width or clock and
    /// leaving the host and the card disagreeing about both.
    unsafe fn reset_cmd_dat(&self) {
        self.w8(REG_SOFTWARE_RESET, SWRST_CMD | SWRST_DAT);
        let mut spins: u32 = 0;
        while self.r8(REG_SOFTWARE_RESET) & (SWRST_CMD | SWRST_DAT) != 0 {
            spins += 1;
            if spins > SPIN_LIMIT { break; }
            core::hint::spin_loop();
        }
        self.w16(REG_NORMAL_INT_STATUS, NI_ALL);
        self.w16(REG_ERROR_INT_STATUS, EI_ALL);
    }

    unsafe fn wait_cmd_inhibit_clear(&self, also_dat: bool) {
        let mask = if also_dat { PSTATE_CMD_INHIBIT | PSTATE_DAT_INHIBIT } else { PSTATE_CMD_INHIBIT };
        let mut spins: u32 = 0;
        while self.r32(REG_PRESENT_STATE) & mask != 0 {
            spins += 1;
            if spins > SPIN_LIMIT { dbg(b'i'); return; }
            core::hint::spin_loop();
        }
    }

    /// Issue a command and wait for Command Complete. Returns the 4-word
    /// response (R2 uses all 4; shorter responses only populate [0]).
    unsafe fn send_command(
        &self,
        index: u8,
        arg: u32,
        resp: u16,
        data_present: bool,
        crc_check: bool,
        idx_check: bool,
    ) -> Option<[u32; 4]> {
        self.wait_cmd_inhibit_clear(data_present || resp == RESP_48B);

        // Clear both interrupt-status registers before issuing anything.
        //
        // `wait_normal_int` only ever clears the specific bits it was waiting
        // for, so any other bit the controller raised stays latched. A stale
        // Transfer Complete then satisfies the *next* transfer's completion
        // wait instantly and falsely, and host and card desynchronise: every
        // command after that returns Command Timeout (ERROR_INT_STATUS bit 0).
        // On hardware the first symptom was a write whose Buffer-Write-Ready
        // wait spun out with NORMAL_INT_STATUS already reading 0x0002.
        self.w16(REG_NORMAL_INT_STATUS, NI_ALL);
        self.w16(REG_ERROR_INT_STATUS, EI_ALL);

        self.w32(REG_ARGUMENT1, arg);

        let mut word = ((index as u16) << 8) | resp;
        if data_present { word |= CMD_DATA_PRESENT; }
        if crc_check { word |= CMD_CRC_CHECK; }
        if idx_check { word |= CMD_IDX_CHECK; }
        self.w16(REG_COMMAND, word);

        if !self.wait_normal_int(NI_CMD_COMPLETE) {
            return None;
        }

        Some([
            self.r32(REG_RESPONSE0),
            self.r32(REG_RESPONSE0 + 4),
            self.r32(REG_RESPONSE0 + 8),
            self.r32(REG_RESPONSE0 + 12),
        ])
    }

    /// Read or write one 512-byte sector via single-block PIO.
    unsafe fn do_io_sector(&self, is_write: bool, sector: u64, buf: *mut u8) -> bool {
        let arg = if self.high_capacity { sector as u32 } else { (sector * SECTOR_SIZE as u64) as u32 };
        let cmd = if is_write { CMD_WRITE_BLOCK } else { CMD_READ_SINGLE_BLOCK };

        self.w16(REG_BLOCK_SIZE, SECTOR_SIZE as u16);
        self.w16(REG_BLOCK_COUNT, 1);
        let tm = TM_BLOCK_COUNT_EN | if is_write { 0 } else { TM_DIR_READ };
        self.w16(REG_TRANSFER_MODE, tm);

        if self.send_command(cmd, arg, RESP_48, true, true, true).is_none() {
            return false;
        }

        let ready_bit = if is_write { NI_BUF_WRITE_READY } else { NI_BUF_READ_READY };
        if !self.wait_normal_int(ready_bit) {
            return false;
        }

        for i in 0..(SECTOR_SIZE / 4) {
            let word_ptr = buf.add(i * 4) as *mut u32;
            if is_write {
                self.w32(REG_BUFFER_DATA, word_ptr.read_unaligned());
            } else {
                word_ptr.write_unaligned(self.r32(REG_BUFFER_DATA));
            }
        }

        self.wait_normal_int(NI_XFER_COMPLETE)
    }

    /// Issue CMD55 (APP_CMD) so the *next* command is interpreted as an ACMD.
    /// Returns false if the card did not acknowledge it, in which case the
    /// caller must not send the ACMD — the card would decode it as the ordinary
    /// command sharing that index (CMD6 SWITCH_FUNC for ACMD6, for instance).
    unsafe fn app_cmd(&self, rca: u32) -> bool {
        self.send_command(CMD_APP_CMD, rca << 16, RESP_48, false, true, true).is_some()
    }

    /// Issue a command whose reply is a short block of data on DAT rather than
    /// a response-register field, and copy that block into `out`.
    ///
    /// Two commands in the bring-up sequence work this way and neither can be
    /// served by `do_io_sector`, which hard-codes a 512-byte sector and an
    /// address argument: ACMD51 (SEND_SCR) returns 8 bytes, and CMD6
    /// (SWITCH_FUNC) returns 64. The machinery is otherwise identical to a
    /// single-block read — set Block Size to the real length, Block Count to 1,
    /// a read-direction Transfer Mode, issue with Data Present set, wait for
    /// Buffer Read Ready, drain the PIO port, wait for Transfer Complete.
    ///
    /// `out.len()` must be a non-zero multiple of 4 and must match exactly what
    /// the card will send: the controller counts the block down itself, and a
    /// short drain leaves bytes in the buffer that the next transfer would
    /// read first.
    ///
    /// The card sends these structures MSB-first while the PIO port hands back
    /// bytes in arrival order packed little-endian, so `out` ends up in wire
    /// order — byte 0 is the most significant byte of the structure. Callers
    /// index it that way.
    unsafe fn read_data_command(&self, index: u8, arg: u32, out: &mut [u8]) -> bool {
        let len = out.len();
        if len == 0 || len % 4 != 0 { return false; }

        self.w16(REG_BLOCK_SIZE, len as u16);
        self.w16(REG_BLOCK_COUNT, 1);
        self.w16(REG_TRANSFER_MODE, TM_BLOCK_COUNT_EN | TM_DIR_READ);

        if self.send_command(index, arg, RESP_48, true, true, true).is_none() {
            return false;
        }
        if !self.wait_normal_int(NI_BUF_READ_READY) {
            return false;
        }
        for i in 0..(len / 4) {
            let word = self.r32(REG_BUFFER_DATA);
            (out.as_mut_ptr().add(i * 4) as *mut u32).write_unaligned(word);
        }
        self.wait_normal_int(NI_XFER_COMPLETE)
    }

    fn do_io(&self, is_write: bool, blk: u64, buf: *mut u8) -> bool {
        let abs_blk = blk + self.partition_offset;
        for i in 0..SECTORS_PER_BLOCK {
            let sector = abs_blk * SECTORS_PER_BLOCK as u64 + i as u64;
            let ok = unsafe { self.do_io_sector(is_write, sector, buf.add(i * SECTOR_SIZE)) };
            if !ok {
                // Report the sector and the controller's own account of what
                // went wrong. Reads demonstrably work (F2FS mounts and root
                // pivots) and then stop working, so the interesting question is
                // what distinguishes the failing transfer from the ones before.
                unsafe {
                    let latched = LAST_FAIL.load(Ordering::Relaxed);
                    dbg(b'\n');
                    dbg(if is_write { b'W' } else { b'R' });
                    dbg_hex(sector);
                    dbg(b'p'); dbg_hex(self.r32(REG_PRESENT_STATE) as u64);
                    dbg(b'N'); dbg_hex((latched >> 16) as u64);
                    dbg(b'E'); dbg_hex((latched & 0xffff) as u64);

                    // Recover the controller so the *next* transfer has a
                    // chance. Without this one bad sector poisons every access
                    // that follows, which is why a single failed write during
                    // mount turned into "execve /bin/login failed" much later.
                    self.reset_cmd_dat();
                    dbg(b'~');
                    dbg(b'\n');
                }
                return false;
            }
        }
        true
    }
}

impl SdhciDevice {
    unsafe fn reset_and_init_clock(&self) {
        self.w8(REG_SOFTWARE_RESET, SWRST_ALL);
        let mut spins: u32 = 0;
        while self.r8(REG_SOFTWARE_RESET) & SWRST_ALL != 0 {
            spins += 1;
            if spins > SPIN_LIMIT { dbg(b'r'); break; }
            core::hint::spin_loop();
        }

        // Divided clock mode, slowest divisor. This is the *identification*
        // clock and nothing more: the SD Physical Layer spec caps the bus at
        // 400 kHz until the card has been addressed, so bring-up has no choice.
        // `negotiate_bus` raises it once the card is in transfer state, and
        // this setting doubles as the floor that negotiation retreats to.
        // Internal clock only for now — enabling the external SD_CLK output
        // (below) before bus power is on left CMD8 timing out with no
        // response during bring-up (CMD0 "succeeded" regardless since it
        // expects no response either way, but CMD8 — the first command
        // that actually needs the bus alive — never got one). Real
        // controllers/QEMU's model expect power-on before the external
        // clock actually starts toggling.
        self.w16(REG_CLOCK_CONTROL, CLK_DIV_MAX | CLK_INTERNAL_EN);
        let mut spins: u32 = 0;
        while self.r16(REG_CLOCK_CONTROL) & CLK_STABLE == 0 {
            spins += 1;
            if spins > SPIN_LIMIT { dbg(b'k'); break; }
            core::hint::spin_loop();
        }

        self.w8(REG_TIMEOUT_CONTROL, 0x0E); // max timeout
        self.w8(REG_POWER_CONTROL, (0b111 << 1) | 1); // 3.3V, bus power on

        self.w16(REG_CLOCK_CONTROL, self.r16(REG_CLOCK_CONTROL) | CLK_SD_EN);

        // SD Physical Layer Simplified Spec: the host must supply at least
        // 74 SD clocks with the bus idle before the card is guaranteed ready
        // for its first real command. CMD0 (GO_IDLE_STATE) has no response
        // to wait for, so it always looks like it "succeeds" regardless of
        // whether the card actually saw it — CMD8 (SEND_IF_COND, the first
        // command that needs a genuine reply) is what actually exposed this
        // as a real Command Timeout Error during bring-up. A fixed spin
        // count substitutes for a real delay (no timer access this early).
        for _ in 0..100_000 { core::hint::spin_loop(); }

        // Stay in 1-bit bus mode (Host Control1 default) for now: a card comes
        // out of reset in 1-bit DAT0-only mode, and setting the host's width
        // bit before ACMD6 has told the card to follow would desync the two.
        // `negotiate_bus` does both, in that order, after identification.

        self.w16(REG_NORMAL_INT_ENABLE, NI_ALL);
        self.w16(REG_ERROR_INT_ENABLE, EI_ALL);
        self.w16(REG_NORMAL_INT_STATUS, NI_ALL);
        self.w16(REG_ERROR_INT_STATUS, EI_ALL);
    }
}

// ── Bus negotiation (width and clock) ────────────────────────────────────────
//
// Bring-up above leaves the bus where the spec requires it during card
// identification: one data line, under 400 kHz. Left there, a 4096-byte page
// costs about 85 ms of pure bus time, which is why demand-paging a shell took
// minutes on real hardware. Everything in this section exists to get off that
// setting afterwards, and to do it in a way that cannot cost a boot.
//
// The shape is a ladder over a known-good `BusState`. Each rung is programmed
// and then *proved*, by re-reading sector 0 and comparing it byte for byte
// against a copy taken while the slow configuration was still in force. A rung
// that does not compare equal is rolled back and the ladder stops. Proving
// rather than assuming matters because the failure mode here is silent: a
// capabilities register that misreports the base clock makes the host run at
// twice the frequency it believes it picked, and the controller reports nothing
// unusual — only the data is wrong. Note also that negotiation only ever
// *reads*, so a rung the bus cannot hold can corrupt nothing on the card.

/// The host-side bus settings this negotiation moves, as a value, so a rung
/// that fails verification can be put back exactly as it was.
#[derive(Clone, Copy)]
struct BusState {
    /// The `HC1_*` bits of Host Control 1. Other bits in that register are
    /// preserved by `apply_bus` rather than represented here.
    host_ctl: u8,
    /// Encoded `SDCLK Frequency Select` bits for Clock Control, ready to write.
    clk_sel: u16,
    /// What `clk_sel` actually works out to. Carried along only so the summary
    /// line can report a number rather than a register encoding.
    clk_khz: u32,
}

/// Encode `SDCLK Frequency Select` for Clock Control, picking the fastest
/// setting that does not *exceed* `target_khz`. Returns the bits to write and
/// the frequency they produce.
///
/// The two encodings are genuinely different, and conflating them is the
/// classic SDHCI clock bug:
///
/// * **Spec 3.0 — 10-bit divided clock.** The field is split across the
///   register: bits [15:8] hold N[7:0] and bits [7:6] hold N[9:8]. The divisor
///   is `2*N`, with `N == 0` meaning "base clock, undivided". Any N is legal,
///   not just powers of two — and a driver that writes only bits [15:8] keeps
///   whatever N[9:8] happened to be there, which is how a request for 25 MHz
///   quietly becomes a request for 25/256 MHz.
/// * **Spec 2.0 — 8-bit.** Bits [15:8] only, and the value must be a *single*
///   set bit: 0x00 = base, 0x01 = base/2, 0x02 = base/4, 0x04 = base/8, …,
///   0x80 = base/256. Writing 3.0's arbitrary N here selects a divisor nobody
///   asked for.
///
/// `base_khz` or `target_khz` of zero means the caller has nothing to go on;
/// the answer is the slow floor and a reported frequency of zero.
fn clock_divider(base_khz: u32, target_khz: u32, v3: bool) -> (u16, u32) {
    if base_khz == 0 || target_khz == 0 {
        return (CLK_DIV_MAX, 0);
    }
    if v3 {
        let n = if base_khz <= target_khz {
            0
        } else {
            // ceil(base / (2*target)) — the smallest divisor landing at or
            // below the target. Rounding down here would overclock the bus.
            let n = (base_khz + 2 * target_khz - 1) / (2 * target_khz);
            if n > 1023 { 1023 } else { n }
        };
        let actual = if n == 0 { base_khz } else { base_khz / (2 * n) };
        let n = n as u16;
        ((n & 0xFF) << 8 | ((n >> 8) & 0x03) << 6, actual)
    } else {
        let mut d: u32 = 1;
        while d < 256 && base_khz / d > target_khz {
            d <<= 1;
        }
        (((d as u16) >> 1) << 8, base_khz / d)
    }
}

/// Decode the CSD's `TRAN_SPEED` byte into kHz — the card's own statement of
/// the fastest clock it will accept in its current speed mode.
///
/// Bits [2:0] select a rate unit and bits [6:3] index a fixed multiplier table.
/// 0x32 is "10 Mbit/s × 2.5" = 25 MHz, which is what every SD card reports in
/// Default Speed; after a successful High Speed switch the same field reads
/// 0x5A = "10 Mbit/s × 5.0" = 50 MHz. Reserved unit codes return 0, which the
/// caller reads as "unknown" and replaces with the 25 MHz floor every SDHC card
/// is required to support.
fn tran_speed_to_khz(v: u8) -> u32 {
    // ×10 so the table stays integral; index 0 is reserved and yields 0.
    const MULT_X10: [u32; 16] = [0, 10, 12, 13, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 70, 80];
    let unit_khz: u32 = match v & 0x07 {
        0 => 100,
        1 => 1_000,
        2 => 10_000,
        3 => 100_000,
        _ => return 0,
    };
    unit_khz * MULT_X10[((v >> 3) & 0x0F) as usize] / 10
}

impl SdhciDevice {
    /// Program a `BusState` onto the controller.
    ///
    /// SDHCI 3.0 §3.2.3 is explicit that SD Clock Enable must be cleared before
    /// the frequency select field is touched; changing a divisor under a live
    /// clock can emit a runt pulse that the card counts as a bit. The same
    /// window is the right place to move Host Control 1's width and High Speed
    /// bits, which alter the edge the host drives and samples on.
    ///
    /// The frequency select is written as a whole word rather than merged into
    /// the existing one, because the 3.0 field is discontiguous and a
    /// read-modify-write of one half is exactly how half of an old divisor gets
    /// left behind.
    unsafe fn apply_bus(&self, s: BusState) {
        let cc = self.r16(REG_CLOCK_CONTROL);
        self.w16(REG_CLOCK_CONTROL, cc & !CLK_SD_EN);

        let hc = self.r8(REG_HOST_CONTROL1);
        self.w8(REG_HOST_CONTROL1, (hc & !(HC1_DATA_WIDTH_4 | HC1_HIGH_SPEED)) | s.host_ctl);

        // Clock Generator Select (bit 5) deliberately left at 0: divided clock
        // mode. The programmable generator is optional and vendor-defined.
        self.w16(REG_CLOCK_CONTROL, s.clk_sel | CLK_INTERNAL_EN);
        let mut spins: u32 = 0;
        while self.r16(REG_CLOCK_CONTROL) & CLK_STABLE == 0 {
            spins += 1;
            if spins > SPIN_LIMIT { dbg(b'k'); break; }
            core::hint::spin_loop();
        }
        self.w16(REG_CLOCK_CONTROL, self.r16(REG_CLOCK_CONTROL) | CLK_SD_EN);

        // Let the new clock reach the card before anything rides it. Same
        // no-timer-yet situation as the 74-clock wait in `reset_and_init_clock`,
        // so the same fixed spin count stands in for a real delay.
        for _ in 0..10_000 { core::hint::spin_loop(); }
    }

    /// Prove the bus as currently configured, by re-reading sector 0 and
    /// comparing it against `reference` — a copy taken at the slow, known-good
    /// setting. Returns false if the read errors *or* returns different bytes.
    ///
    /// Two attempts: the very first command after a clock or width change can
    /// trip on nothing worse than the controller settling, and losing a whole
    /// rung to that would be a silent performance bug. A rate the bus genuinely
    /// cannot hold fails both times.
    unsafe fn verify_bus(&self, reference: &[u8; SECTOR_SIZE]) -> bool {
        for _ in 0..2 {
            let mut probe = [0u8; SECTOR_SIZE];
            if self.do_io_sector(false, 0, probe.as_mut_ptr()) && probe == *reference {
                return true;
            }
            // `do_io_sector` leaves recovery to its caller; here that is us.
            self.reset_cmd_dat();
        }
        false
    }
}

/// Last resort: put both ends back where bring-up left them — the card on one
/// data line, the host on one data line at the identification clock — and adopt
/// that as the known-good state. Nothing here is checked, because by the time
/// this runs the checks are what failed; the `f` marker is the report.
///
/// ACMD6 travels on CMD alone, so it still gets through while the host and the
/// card disagree about how many DAT lines there are — which is exactly the
/// situation that brings us here.
unsafe fn fall_to_floor(dev: &SdhciDevice, rca: u32, floor: BusState, good: &mut BusState) {
    if dev.app_cmd(rca) {
        let _ = dev.send_command(ACMD_SET_BUS_WIDTH, 0b00, RESP_48, false, true, true);
    } else {
        dev.reset_cmd_dat();
    }
    dev.apply_bus(floor);
    *good = floor;
    dbg(b'f');
}

/// Apply `rung`, prove it, and either promote it to the new known-good state or
/// roll all the way back. Returns whether the rung stuck.
///
/// The rollback has two tiers because the first one can itself fail. Restoring
/// `*good` is the normal case. If even that no longer reads — which is what a
/// host and card that disagree about bus width looks like — the second tier
/// drops to `floor`, the configuration this driver used before any of this
/// existed.
unsafe fn try_rung(
    dev: &SdhciDevice,
    rca: u32,
    reference: &[u8; SECTOR_SIZE],
    floor: BusState,
    good: &mut BusState,
    rung: BusState,
) -> bool {
    dev.apply_bus(rung);
    if dev.verify_bus(reference) {
        *good = rung;
        return true;
    }

    dev.apply_bus(*good);
    if !dev.verify_bus(reference) {
        fall_to_floor(dev, rca, floor, good);
    }
    false
}

/// Move the card off the 1-bit identification clock that bring-up leaves it on,
/// and onto the widest bus and fastest clock that the card, the controller and
/// a live read-back all agree on.
///
/// Every stage is individually fallible and every failure degrades rather than
/// aborts: the worst case of this whole function is the configuration the
/// driver used before it was written. Stage markers, lower case as elsewhere in
/// this file, are `v` (no reference read — nothing to verify against, so
/// nothing is changed), `c` (SCR unreadable), `w` (bus width), `g` (clock
/// rung), `h` (High Speed) and `f` (fell back to the floor).
unsafe fn negotiate_bus(dev: &SdhciDevice, rca: u32, tran_speed_khz: u32) {
    // ── What the controller says it can do ───────────────────────────────
    //
    // Spec version first, because it decides how both the capabilities base
    // clock field and the clock divider are encoded. 0 = 1.00, 1 = 2.00,
    // 2 = 3.00; anything at or above 2 gets the wider field and the 10-bit
    // divider.
    let spec = (dev.r16(REG_HOST_VERSION) & 0xFF) as u32;
    let v3 = spec >= 2;
    let caps = dev.r32(REG_CAPABILITIES);
    // Base Clock Frequency For SD Clock, in MHz: bits [13:8] under spec 2.0
    // (6 bits, so 63 MHz is the most it can express), widened to bits [15:8]
    // under 3.0. A zero means the register cannot answer — see
    // `BASE_CLOCK_FALLBACK_KHZ` for where the answer comes from instead.
    let caps_base_mhz = if v3 { (caps >> 8) & 0xFF } else { (caps >> 8) & 0x3F };
    let base_khz = if caps_base_mhz != 0 { caps_base_mhz * 1000 } else { BASE_CLOCK_FALLBACK_KHZ };

    // The identification rate currently in force, for the summary line: bring-up
    // programmed CLK_DIV_MAX, which is N = 255 (divisor 510) under the 3.0
    // encoding. Under the 2.0 encoding 0xFF is not a legal single-bit value at
    // all; base/256 is the closest honest reading, and the summary prints the
    // spec version so a controller that reports 2.0 shows up rather than hiding.
    let floor = BusState {
        host_ctl: 0,
        clk_sel: CLK_DIV_MAX,
        clk_khz: if v3 { base_khz / 510 } else { base_khz / 256 },
    };
    let mut good = floor;

    // ── The reference every rung below is measured against ───────────────
    //
    // If sector 0 cannot be read at the configuration that demonstrably works,
    // there is no way to tell a bad clock from a bad card later, and no safe
    // basis for changing anything. Leave the bus exactly as it is.
    let mut reference = [0u8; SECTOR_SIZE];
    if !dev.do_io_sector(false, 0, reference.as_mut_ptr()) {
        dev.reset_cmd_dat();
        dbg(b'v');
        return;
    }

    // ── Bus width ────────────────────────────────────────────────────────
    //
    // ACMD51 (SEND_SCR) returns the 8-byte SD Configuration Register on DAT,
    // MSB first. SCR[59:56] is SD_SPEC (0 = 1.0/1.01, 1 = 1.10, 2 = 2.00+),
    // and SCR[51:48] is SD_BUS_WIDTHS, a bitmap whose bit 0 means 1-bit and
    // bit 2 means 4-bit. Both live in the top word.
    let mut sd_spec = 0u32;
    let mut four_bit = false;
    let mut scr = [0u8; 8];
    if dev.app_cmd(rca) && dev.read_data_command(ACMD_SEND_SCR, 0, &mut scr) {
        let hi = u32::from_be_bytes([scr[0], scr[1], scr[2], scr[3]]);
        sd_spec = (hi >> 24) & 0x0F;
        four_bit = (hi >> 16) & 0b0100 != 0;
    } else {
        // Not fatal — an unreadable SCR only costs the width switch. The clock
        // ladder below is independent of it and still runs.
        dev.reset_cmd_dat();
        dbg(b'c');
    }

    if four_bit {
        // Card first, host second. In between, the card is listening on
        // DAT[3:0] while the host still drives one line, which is harmless
        // precisely because nothing is issued in between.
        if dev.app_cmd(rca)
            && dev.send_command(ACMD_SET_BUS_WIDTH, 0b10, RESP_48, false, true, true).is_some()
        {
            let wide = BusState { host_ctl: good.host_ctl | HC1_DATA_WIDTH_4, ..good };
            if !try_rung(dev, rca, &reference, floor, &mut good, wide) {
                // `try_rung` has already put the host back to one line; put the
                // card back to match, or every read after this fails for a
                // reason that has nothing to do with the clock.
                if dev.app_cmd(rca) {
                    let _ = dev.send_command(ACMD_SET_BUS_WIDTH, 0b00, RESP_48, false, true, true);
                } else {
                    dev.reset_cmd_dat();
                }
                dbg(b'w');
            }
        } else {
            dev.reset_cmd_dat();
            dbg(b'w');
        }
    }

    // ── Clock, Default Speed ─────────────────────────────────────────────
    //
    // Ascending rungs, stopping at the first that does not verify, so a card or
    // a board that cannot hold the top rate still keeps everything below it.
    // The half-rate rung is not ceremony: it is the difference between "the
    // base clock was misreported and we got nothing" and "we got half".
    let ds_ceiling = if tran_speed_khz == 0 { 25_000 } else { tran_speed_khz.min(25_000) };
    let mut clock_raised = false;
    for target in [ds_ceiling / 2, ds_ceiling] {
        if target == 0 { continue; }
        let (clk_sel, clk_khz) = clock_divider(base_khz, target, v3);
        if clk_khz <= good.clk_khz { continue; }
        let rung = BusState { clk_sel, clk_khz, ..good };
        if !try_rung(dev, rca, &reference, floor, &mut good, rung) {
            dbg(b'g');
            break;
        }
        clock_raised = true;
    }

    // ── High Speed (CMD6 function group 1, SD spec 1.10 and later) ───────
    //
    // CMD6's answer is a 64-byte structure on DAT, not a field of the R1
    // response — the response says only that the command was accepted, never
    // whether the switch took. So it rides `read_data_command` exactly as the
    // SCR does, and the payload is what gets checked:
    //
    //   * bits [415:400] are group 1's support bitmap, so function 1 (High
    //     Speed) is bit 1 of byte 13;
    //   * bits [379:376] report which function group 1 actually ended up in,
    //     which is the low nibble of byte 16 — 1 if High Speed took, 0xF if the
    //     card declined.
    //
    // Mode 0 asks without committing, mode 1 commits. Both are issued at the
    // Default Speed rate settled above, which is where the card expects them.
    //
    // Gated on the Default Speed ladder having actually worked. If it did not,
    // the bus is already behaving in a way nothing here understands, and the
    // useful thing to do with that is stop — not ask a card that just failed a
    // 25 MHz read whether it would like to try 50. `sd_spec` of 0 also lands
    // here when the SCR was unreadable, which is the conservative answer:
    // CMD6 only exists from spec 1.10 onward.
    if sd_spec >= 1 && clock_raised {
        let mut st = [0u8; SWITCH_STATUS_LEN];
        let mut hs_ok = false;
        if dev.read_data_command(CMD_SWITCH_FUNC, SWITCH_CHECK_HS, &mut st) {
            if st[13] & 0x02 != 0 {
                if dev.read_data_command(CMD_SWITCH_FUNC, SWITCH_SET_HS, &mut st)
                    && st[16] & 0x0F == 1
                {
                    hs_ok = true;
                } else {
                    dev.reset_cmd_dat();
                    dbg(b'h');
                }
            }
        } else {
            dev.reset_cmd_dat();
            dbg(b'h');
        }

        if hs_ok {
            // High Speed is 50 MHz at 3.3V signalling. UHS rates are
            // deliberately out of reach: they need the 1.8V switch this driver
            // does not implement and explicitly does not request in ACMD41.
            let (clk_sel, clk_khz) = clock_divider(base_khz, 50_000, v3);
            let rung = BusState { host_ctl: good.host_ctl | HC1_HIGH_SPEED, clk_sel, clk_khz };
            if clk_khz > good.clk_khz && !try_rung(dev, rca, &reference, floor, &mut good, rung) {
                dbg(b'h');
                // The host is back at the last verified rung but the card is
                // still in High Speed mode. That pairing is legal — a High
                // Speed card must still work at any rate up to 50 MHz — but if
                // it does not actually read, walk the card back to Default
                // Speed too rather than leave the mismatch in place.
                if !dev.verify_bus(&reference) {
                    let mut back = [0u8; SWITCH_STATUS_LEN];
                    if !dev.read_data_command(CMD_SWITCH_FUNC, SWITCH_SET_DS, &mut back) {
                        dev.reset_cmd_dat();
                    }
                    if !dev.verify_bus(&reference) {
                        fall_to_floor(dev, rca, floor, &mut good);
                    }
                }
            }
        }
    }

    // One line, printed here rather than at the end of `init`, so it lands even
    // if the partition scan that follows fails. This is the whole point of the
    // exercise: one hardware boot either shows `width=4` with a clock in the
    // tens of megahertz, or shows exactly which stage refused and what the
    // controller claimed about itself while refusing.
    dbg(b'\n');
    dbg_str("[RK018AC] bus width=");
    dbg_dec(if good.host_ctl & HC1_DATA_WIDTH_4 != 0 { 4 } else { 1 });
    dbg_str(" clk=");
    dbg_dec(good.clk_khz);
    dbg_str("kHz hs=");
    dbg_dec(if good.host_ctl & HC1_HIGH_SPEED != 0 { 1 } else { 0 });
    dbg_str(" base=");
    dbg_dec(base_khz);
    dbg_str("kHz tran=");
    dbg_dec(tran_speed_khz);
    dbg_str("kHz specver=");
    dbg_dec(spec);
    dbg_str(" caps=");
    dbg_hex(caps as u64);
    dbg(b'\n');
}

// ── Card bring-up ─────────────────────────────────────────────────────────────

/// Real SD cards carry an MBR partition table (see
/// `scripts/prepare-rpi5-sdcard.sh`): partition 1 is the FAT32 boot
/// partition RPi5 firmware scans directly; partition 2, MBR type `0x83`
/// ("Linux" — F2FS has no dedicated MBR type byte, matching how a real
/// Linux system would label it), holds F2FS. Parsed at block 0 rather than
/// assuming a fixed offset, which would silently corrupt data if a future
/// partitioning-tool version changes the exact start sector.
#[cfg(feature = "rpi5")]
unsafe fn find_f2fs_partition(dev: &SdhciDevice) -> Option<u64> {
    let mut mbr = [0u8; BLOCK_SIZE];
    if !dev.do_io(false, 0, mbr.as_mut_ptr()) { return None; }
    if mbr[510] != 0x55 || mbr[511] != 0xAA { return None; } // no valid MBR signature
    for i in 0..4usize {
        let entry = &mbr[446 + i * 16..446 + i * 16 + 16];
        if entry[4] == 0x83 {
            let lba_start = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as u64;
            // MBR LBA is in 512-byte sectors; our blocks are 4096 bytes.
            return Some(lba_start / SECTORS_PER_BLOCK as u64);
        }
    }
    None
}

unsafe fn probe_card(base: usize) -> Option<SdhciDevice> {
    let mut dev = SdhciDevice { base, high_capacity: false, partition_offset: 0 };
    dev.reset_and_init_clock();

    dev.send_command(CMD_GO_IDLE_STATE, 0, RESP_NONE, false, false, false)?;

    // SEND_IF_COND: echo pattern 0xAA at 2.7-3.6V (0x100). A card that
    // doesn't answer (no card inserted, or a pre-v2 card) fails this driver
    // outright rather than falling back — acceptable for a v1 targeting
    // SDHC/SDXC cards on QEMU/real hardware.
    let r = dev.send_command(CMD_SEND_IF_COND, 0x1AA, RESP_48, false, true, true)?;
    if r[0] & 0xFF != 0xAA { dbg(b'x'); return None; }

    // ACMD41 loop: CMD55 (APP_CMD) + ACMD41 (SD_SEND_OP_COND) with HCS set,
    // until the card reports ready (response bit 31). R3 has no valid CRC
    // or command-index field, so both checks are disabled.
    let mut acmd41_tries: u32 = 0;
    dev.high_capacity = loop {
        dev.send_command(CMD_APP_CMD, 0, RESP_48, false, true, true)?;
        // 0x40FF_8000 = HCS (bit 30) | VDD window 2.7-3.6V (bits 23:8 = 0xFF80).
        //
        // The window is the operative part and was previously empty (0x5100_0000
        // claimed "HCS | 3.3V window" in a comment but set bits 30/28/24 and no
        // window at all). Per the SD Physical Layer spec an ACMD41 carrying a
        // zero voltage window is an *inquiry*: the card reports its OCR and
        // deliberately stays in idle without starting its power-up sequence, so
        // the busy bit polled below never clears and this loop cannot terminate.
        // On hardware that hung the boot right after CMD8 succeeded.
        //
        // This card's OCR is 0xC0FF8000 — the firmware prints it during its own
        // probe — so 0xFF80 is exactly the window it supports. Bit 24 (S18R, the
        // 1.8V signalling request) is deliberately not set: nothing in this
        // driver implements the voltage switch that would have to follow.
        let r = dev.send_command(ACMD_SD_SEND_OP_COND, 0x40FF_8000, RESP_48, false, false, false)?;
        if r[0] & (1 << 31) != 0 {
            break r[0] & (1 << 30) != 0;
        }
        // Power-up can legitimately take ~1s of polling, but not forever: a
        // card that never sets the ready bit is a dead card, not a slow one.
        acmd41_tries += 1;
        if acmd41_tries > 100_000 { dbg(b'a'); return None; }
        core::hint::spin_loop();
    };

    dev.send_command(CMD_ALL_SEND_CID, 0, RESP_136, false, true, false)?;

    let r = dev.send_command(CMD_SEND_RELATIVE_ADDR, 0, RESP_48, false, true, true)?;
    let rca = (r[0] >> 16) as u32;

    // SEND_CSD, deliberately here and not later: CMD9 is only legal while the
    // card is in stand-by, so it has to go out between CMD3 and CMD7. What we
    // want from it is TRAN_SPEED, the card's own ceiling on the bus clock.
    //
    // R2 is a 136-bit response, and SDHCI drops the CRC7 and end bit before
    // storing it: the card's bits [127:8] land in the response registers as
    // bits [119:0]. TRAN_SPEED is CSD[103:96], so it comes back shifted down by
    // eight to register bits [95:88] — the top byte of the third word. (Index
    // check is off for the same reason it is off for CMD2: an R2 response
    // carries no command-index field to check.)
    //
    // Failure is not fatal. `negotiate_bus` reads a zero as "unknown" and falls
    // back to the 25 MHz every SDHC card is required to support.
    let tran_speed_khz = match dev.send_command(CMD_SEND_CSD, rca << 16, RESP_136, false, true, false) {
        Some(csd) => tran_speed_to_khz(((csd[2] >> 24) & 0xFF) as u8),
        None => { dev.reset_cmd_dat(); dbg(b'd'); 0 }
    };

    dev.send_command(CMD_SELECT_CARD, rca << 16, RESP_48B, false, true, true)?;

    if !dev.high_capacity {
        dev.send_command(CMD_SET_BLOCKLEN, SECTOR_SIZE as u32, RESP_48, false, true, true)?;
    }

    // The card is in transfer state and reachable at the identification rate,
    // which is the known-good configuration everything below degrades to. Wider
    // and faster from here is pure upside — `negotiate_bus` cannot fail, only
    // decline to improve — so it is not part of the `?` chain above.
    negotiate_bus(&dev, rca, tran_speed_khz);

    #[cfg(feature = "rpi5")]
    {
        dev.partition_offset = find_f2fs_partition(&dev)?;
    }

    Some(dev)
}

// ── Global device table ───────────────────────────────────────────────────────

static DEVICES: Mutex<[Option<SdhciDevice>; MAX_BLK_DEVICES]> =
    Mutex::new([const { None }; MAX_BLK_DEVICES]);
static DEVICE_COUNT: Mutex<usize> = Mutex::new(0);

// ── Public API (matches virtio_blk.rs) ───────────────────────────────────────

pub fn init() {
    let virt_base = mm::phys_to_virt(SDHCI_BASE);
    let mut devs = DEVICES.lock();
    let mut cnt = DEVICE_COUNT.lock();

    // Device index 0 is intentionally left empty. `userland/init` hardcodes
    // its mount source as `/dev/vdb` (device index 1), matching the virtio
    // three-disk convention (`drive0`=boot disk index 0, `data0`=index 1)
    // used on the QEMU virt/x86_64 targets. This board has no equivalent
    // boot disk — the kernel+initrd load via `-device loader`, not a block
    // device — so mirroring the index instead of touching shared userland
    // code keeps `/dev/vdb` working unmodified everywhere.
    *cnt = match unsafe { probe_card(virt_base) } {
        Some(d) => { devs[1] = Some(d); dbg(b']'); 2 }
        None => { dbg(b'-'); 0 }
    };
}

pub fn device_count() -> usize {
    *DEVICE_COUNT.lock()
}

/// Read one 4096-byte block from device `dev_idx` at logical block `blk`.
pub fn read_block(dev_idx: usize, blk: u64, buf: &mut [u8; BLOCK_SIZE]) -> bool {
    let devs = DEVICES.lock();
    if let Some(ref dev) = devs[dev_idx] {
        dev.do_io(false, blk, buf.as_mut_ptr())
    } else {
        false
    }
}

/// Read `buf.len() / 4096` contiguous blocks starting at logical block `blk`
/// into `buf`. `buf.len()` must be a non-zero multiple of 4096.
///
/// Mirrors `virtio_blk::read_blocks`, which `servers/f2fs` reaches through the
/// `drivers::blkdev` alias, so the same source compiles against either backend.
/// There is no `MAX_IO_BLOCKS` splitting to mirror here: `do_io` already
/// decomposes every 4096-byte block into eight single-sector CMD17 transfers,
/// so no request this loop issues is larger than one 512-byte sector either
/// way. One lock acquisition covers the whole span, matching virtio_blk.
pub fn read_blocks(dev_idx: usize, blk: u64, buf: &mut [u8]) -> bool {
    if buf.is_empty() || buf.len() % BLOCK_SIZE != 0 { return false; }
    let devs = DEVICES.lock();
    let dev = match devs[dev_idx] { Some(ref d) => d, None => return false };
    let total = buf.len() / BLOCK_SIZE;
    for i in 0..total {
        let ok = dev.do_io(false, blk + i as u64, unsafe { buf.as_mut_ptr().add(i * BLOCK_SIZE) });
        if !ok { return false; }
    }
    true
}

/// Commit `dev_idx`'s writes to stable storage.
///
/// The virtio counterpart issues a real `VIRTIO_BLK_T_FLUSH` because the host
/// backend genuinely holds a writeback cache that `write_block` returning does
/// not empty. This driver has no such intermediary: writes are single-block
/// CMD24s driven by programmed I/O straight into the controller's buffer port,
/// and `do_io_sector` does not return until the controller raises Transfer
/// Complete — by which point the card has left programming state and the data
/// is on media. So there is nothing left to commit, and reporting success is
/// the accurate answer rather than a convenient one.
///
/// Still gated on the device existing, so a flush against an absent device
/// reports failure exactly as virtio_blk does.
pub fn flush(dev_idx: usize) -> bool {
    let devs = DEVICES.lock();
    devs[dev_idx].is_some()
}

/// Write one 4096-byte block to device `dev_idx` at logical block `blk`.
pub fn write_block(dev_idx: usize, blk: u64, buf: &[u8; BLOCK_SIZE]) -> bool {
    let devs = DEVICES.lock();
    if let Some(ref dev) = devs[dev_idx] {
        dev.do_io(true, blk, buf.as_ptr() as *mut u8)
    } else {
        false
    }
}

/// Returns true if device `dev_idx` contains an F2FS volume (magic at byte 1024 of block 0).
pub fn has_f2fs(dev_idx: usize) -> bool {
    let mut buf = alloc::vec![0u8; BLOCK_SIZE];
    let arr: &mut [u8; BLOCK_SIZE] = buf.as_mut_slice().try_into().unwrap();
    if !read_block(dev_idx, 0, arr) { return false; }
    let magic = u32::from_le_bytes(buf[F2FS_SB_OFFSET..F2FS_SB_OFFSET + 4].try_into().unwrap());
    magic == F2FS_MAGIC
}

/// Metadata for `lsblk`. Capacity is still not tracked for SDHCI cards, so
/// `total_blocks` is reported as 0 (unknown) rather than guessed. `probe_card`
/// does now read the CSD (CMD9), but only keeps TRAN_SPEED out of it; wiring
/// C_SIZE through would also have to decide whether `lsblk` should report the
/// whole card or just the F2FS partition this device is offset into, and that
/// is a separate question from bus speed.
pub fn info(dev_idx: usize) -> Option<crate::BlkDevInfo> {
    let devs = DEVICES.lock();
    devs.get(dev_idx)?.as_ref()?;
    drop(devs);
    Some(crate::BlkDevInfo {
        total_blocks: 0,
        block_size: BLOCK_SIZE as u32,
        fstype: if has_f2fs(dev_idx) { Some("f2fs") } else { None },
    })
}
