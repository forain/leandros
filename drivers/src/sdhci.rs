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
//! **Known open issue on the `raspi4b` path**: card bring-up (CMD0/CMD8/
//! ACMD41/...) is fully implemented and was verified byte-correct at the
//! register level — command encoding, argument, and base-address reads
//! (`CAPABILITIES` reads back exactly `0x052134b4`, matching QEMU's own
//! `capareg`) all confirmed via QMP/GDB-stub introspection against a live
//! instance. CMD8 nonetheless gets a genuine Command Timeout Error from the
//! controller every time. Root-caused (not guessed) via QEMU's own
//! `hw/sd/sd.c` source: `sd_do_command()` returns immediately without ever
//! reaching the card's command table when `blk_is_inserted()` is false for
//! the specific `sd-card` QOM object wired to this bus — confirmed via QMP
//! that `PRESENT_STATE`'s Card Inserted latch (set once, at
//! `sdhci_reset()`, from that same `blk_is_inserted()` check) reads
//! `0` for this card regardless of real elapsed wait time, and is
//! unaffected by attaching the drive explicitly via `-device
//! sd-card,drive=...,bus=sd-bus` instead of the legacy `-drive if=sd`
//! shorthand. This is a QEMU raspi4b machine/CLI block-backend-attachment
//! issue, not a bug in this driver's protocol logic — see the project
//! memory / session notes for the full investigation. Unresolved as of
//! this session; a fresh investigation into `raspi4b`'s board-construction
//! code or an alternate QEMU version is the likely next step, not further
//! changes to the command sequence below.

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
#[cfg(not(feature = "rpi5"))]
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

#[cfg(feature = "raspi4b")]
const SDHCI_BASE: usize = 0xfe34_0000;

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
const REG_POWER_CONTROL:      usize = 0x29; // u8
const REG_CLOCK_CONTROL:      usize = 0x2C; // u16
const REG_TIMEOUT_CONTROL:    usize = 0x2E; // u8
const REG_SOFTWARE_RESET:     usize = 0x2F; // u8
const REG_NORMAL_INT_STATUS:  usize = 0x30; // u16, W1C
const REG_ERROR_INT_STATUS:   usize = 0x32; // u16, W1C
const REG_NORMAL_INT_ENABLE:  usize = 0x34; // u16 (status-enable; signal/IRQ-enable left off — polled driver)
const REG_ERROR_INT_ENABLE:   usize = 0x36; // u16

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

// Clock Control bits
const CLK_INTERNAL_EN: u16 = 1 << 0;
const CLK_STABLE:      u16 = 1 << 1;
const CLK_SD_EN:       u16 = 1 << 2;
const CLK_DIV_MAX:     u16 = 0xFF << 8; // slowest legacy divided-clock setting; correctness over speed

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
const CMD_SELECT_CARD:        u8 = 7;
const CMD_SET_BLOCKLEN:       u8 = 16;
const CMD_READ_SINGLE_BLOCK:  u8 = 17;
const CMD_WRITE_BLOCK:        u8 = 24;
const CMD_APP_CMD:            u8 = 55;
const ACMD_SD_SEND_OP_COND:   u8 = 41;

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
                    self.w8(REG_SOFTWARE_RESET, SWRST_CMD | SWRST_DAT);
                    let mut spins: u32 = 0;
                    while self.r8(REG_SOFTWARE_RESET) & (SWRST_CMD | SWRST_DAT) != 0 {
                        spins += 1;
                        if spins > SPIN_LIMIT { break; }
                        core::hint::spin_loop();
                    }
                    self.w16(REG_NORMAL_INT_STATUS, NI_ALL);
                    self.w16(REG_ERROR_INT_STATUS, EI_ALL);
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

        // Divided clock mode, slowest divisor (correctness over speed — see
        // module doc comment; this driver stays at this rate for its whole
        // lifetime rather than negotiating a faster clock post-identification).
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

        // Stay in 1-bit bus mode (Host Control1 default): we never issue
        // ACMD6 (SET_BUS_WIDTH), and a card defaults to 1-bit DAT0-only mode —
        // switching the width bit without ACMD6 would desync host and card.
        // Leaves throughput on the table, not correctness.

        self.w16(REG_NORMAL_INT_ENABLE, NI_ALL);
        self.w16(REG_ERROR_INT_ENABLE, EI_ALL);
        self.w16(REG_NORMAL_INT_STATUS, NI_ALL);
        self.w16(REG_ERROR_INT_STATUS, EI_ALL);
    }
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

    dev.send_command(CMD_SELECT_CARD, rca << 16, RESP_48B, false, true, true)?;

    if !dev.high_capacity {
        dev.send_command(CMD_SET_BLOCKLEN, SECTOR_SIZE as u32, RESP_48, false, true, true)?;
    }

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

/// Metadata for `lsblk`. Capacity is not currently tracked for SDHCI cards
/// (would require parsing the CSD register via CMD9/SEND_CSD, unimplemented)
/// so `total_blocks` is reported as 0 (unknown) rather than guessed.
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
