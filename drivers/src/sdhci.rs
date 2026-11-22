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

#[allow(unused_imports)]
use leandros_lib::mmio::{MmioRd, MmioWr};
use spin::Mutex;
use crate::kenv::{mm, sched};

/// Set once `init` returns: after that `dbg*` is a no-op. The rpi5 arm pokes the
/// UART by physical address, valid only on the boot identity map; a runtime call
/// (retune, error recovery, re-probe) from a user mm faults, and a fault while
/// DEVICES is held wedges all block I/O. Runtime messages use `serial_debug`.
static PROBE_DONE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Bring-up marker straight to the Pi's debug PL011, independent of the console
/// stack. This driver runs before userspace and had never touched real silicon;
/// when it stalls, the only question worth answering is *which* wait stalled.
#[cfg(feature = "rpi5")]
fn dbg(c: u8) {
    if PROBE_DONE.load(core::sync::atomic::Ordering::Relaxed) { return; }
    const BASE: usize = 0x107D_0010_00;
    unsafe {
        let mut spins = 0u32;
        while leandros_lib::mmio::read((BASE + 0x18) as *const u32) & (1 << 5) != 0 {
            spins += 1;
            if spins > 5_000_000 { return; }
        }
        leandros_lib::mmio::write(BASE as *mut u32, c as u32);
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
    if PROBE_DONE.load(core::sync::atomic::Ordering::Relaxed) { return; }
    let base = mm::phys_to_virt(0xFE20_1000);
    unsafe {
        let mut spins = 0u32;
        while leandros_lib::mmio::read((base + 0x18) as *const u32) & (1 << 5) != 0 {
            spins += 1;
            if spins > 5_000_000 { return; }
        }
        leandros_lib::mmio::write(base as *mut u32, c as u32);
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

/// Open an IRQ window from inside a polled transfer, so this CPU keeps taking
/// its timer tick and IPIs. Every request is driven by PIO with IRQs masked
/// (syscall / fault / kthread context), one 512-byte sector at a time; a
/// multi-megabyte span, or a wait that runs to `SPIN_LIMIT` (~10-20 s of MMIO
/// reads on BCM2712), otherwise leaves the CPU deaf for that long — the
/// `[WDOG] cpuN took no timer tick for ~12 s` seen on the Pi under heavy SD
/// traffic. Same contract as `virtio_blk::irq_window_if_masked`: never a task
/// switch (the caller may hold `DEVICES`), never in IRQ context, and boot-time
/// requests (pid 0, before the scheduler) stay fully masked.
#[inline]
fn tick_window() {
    if sched::irqs_masked() && sched::current_pid() != 0 && !sched::irqctx::in_irq() {
        sched::preempt_disable();
        sched::irq_window();
        sched::preempt_enable_no_resched();
    }
}

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
/// Auto CMD12: the controller sends STOP_TRANSMISSION itself after the last
/// block of a multi-block transfer (SDHCI 2.0+, QEMU sdhci, BCM2712 brcmstb).
const TM_AUTO_CMD12:     u16 = 1 << 2;
const TM_MULTI_BLOCK:    u16 = 1 << 5;
/// Command Type = Abort (Command register bits 7:6), for a manual CMD12.
const CMD_TYPE_ABORT:    u16 = 0b11 << 6;

/// Sectors per multi-block command, and so the most I/O done under one
/// `DEVICES` hold: 128 x 512 B = 64 KiB.
const MULTI_MAX_SECTORS: usize = 128;
/// Blocks (4 KiB) per multi-block command.
const MULTI_MAX_BLOCKS: usize = MULTI_MAX_SECTORS / SECTORS_PER_BLOCK;
/// Multi-block CMD18/CMD25 in use. ON by default: proven bit-exact on the real
/// card (`sdhci.sdtest=3072`: 3 GiB, 199570 mixed-size spans, wbad=rbad=gbad=ioerr=0)
/// and soaked under F2FS writes with concurrent inbound SSH. 16-21 MB/s vs
/// 0.5-1.6 MB/s single-sector. `sdhci.multi=0` disables (see `configure`). Also
/// cleared (for good) after `MULTI_FALLBACK_LIMIT` transfers failed multi-block
/// but then succeeded sector by sector.
static MULTI_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// `sdhci.multitest=1`: at probe, compare a single-sector read and a
/// multi-block read of the first `MULTITEST_BLOCKS` blocks. Read-only.
static MULTITEST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
const MULTITEST_BLOCKS: usize = 1024; // 4 MiB
/// `sdhci.sdtest=<MiB>`: speed table + write/readback proof inside the scratch window.
static SDTEST_MIB: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static SDTEST_PENDING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The raw scratch region (see pi-remote skill): the last 8 GiB of the card,
/// 512-byte sectors, [FIRST, END). The only place `sdtest` may write.
const SCRATCH_FIRST: u64 = 233_222_144;
const SCRATCH_END: u64 = 249_999_360;

/// Kernel command-line knobs, read before `init` probes the card:
/// `sdhci.multi=1` (multi-block I/O) and `sdhci.multitest=1` (boot self-test).
///
/// QEMU `-M raspi4b` boots with no DTB and so no command line: a build with
/// `LEANDROS_SDHCI_KNOBS="sdhci.multitest=1"` in the environment bakes them in.
pub fn configure(cmdline: &str) {
    let baked = option_env!("LEANDROS_SDHCI_KNOBS").unwrap_or("");
    for w in baked.split_ascii_whitespace().chain(cmdline.split_ascii_whitespace()) {
        match w {
            "sdhci.multi=1" => MULTI_ON.store(true, Ordering::Relaxed),
            "sdhci.multi=0" => MULTI_ON.store(false, Ordering::Relaxed),
            "sdhci.multitest=1" => MULTITEST.store(true, Ordering::Relaxed),
            "sdhci.dma=1" => DMA_ON.store(true, Ordering::Relaxed),
            "sdhci.dma=0" => DMA_ON.store(false, Ordering::Relaxed),
            "sdhci.uhs=1" => UHS_ON.store(true, Ordering::Relaxed),
            "sdhci.uhs=0" => UHS_ON.store(false, Ordering::Relaxed),
            _ => {
                if let Some(v) = w.strip_prefix("sdhci.sdtest=") {
                    let m: u32 = v.parse().unwrap_or(0);
                    SDTEST_MIB.store(m, Ordering::Relaxed);
                    SDTEST_PENDING.store(m > 0, Ordering::Relaxed);
                }
            }
        }
    }
}
static MULTI_FALLBACKS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
const MULTI_FALLBACK_LIMIT: u32 = 3;

// ── ADMA2 (32-bit descriptors, bounce buffer below 4 GiB) ────────────────────
//
// BCM2712 `mmc@fff000` sits under a `dma-ranges` that is the identity map
// (bus address == physical), so descriptors carry physical addresses as is.
// Data is staged through one 64 KiB bounce buffer allocated below 4 GiB at
// init (`sdhci.dma=0` opts out, falling back to the PIO multi-block path):
// callers hand us arbitrary kernel buffers that need not be physically
// contiguous or DMA-reachable. The copy costs far less than the PIO it
// replaces. The engine is treated as non-coherent: `mm::dma_cache` clean /
// invalidate calls are plain on a coherent platform. Completion is polled
// (Transfer Complete), like every other wait here. Any error resets the CMD+DAT
// circuits (which also halts ADMA) and the span is redone through PIO.
const REG_ADMA_ERROR:    usize = 0x54; // u8
const REG_ADMA_ADDR:     usize = 0x58; // u32 low, u32 high at +4
const TM_DMA_EN:         u16 = 1 << 0;
const HC1_DMA_MASK:      u8 = 0b11 << 3;
const HC1_ADMA2_32:      u8 = 0b10 << 3;
const CAP_ADMA2:         u32 = 1 << 19;
const ADMA_VALID:        u8 = 1 << 0;
const ADMA_END:          u8 = 1 << 1;
const ADMA_TRAN:         u8 = 0b10 << 4;
/// ADMA transfers go up to 512 KiB per command: at SDR104 the per-command
/// overhead (CMD18/25 + auto-CMD12 + card busy) is what caps 64 KiB spans.
const DMA_MAX_SECTORS:   usize = 1024;
const DMA_MAX_BLOCKS:    usize = DMA_MAX_SECTORS / SECTORS_PER_BLOCK;
const BOUNCE_BYTES:      usize = DMA_MAX_SECTORS * SECTOR_SIZE;
const ADMA_DESC_BYTES:   usize = 4096; // each descriptor maps one 4 KiB page of the bounce
const DMA_FAIL_LIMIT:    u32 = 3;
static DMA_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
static DMA_READY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static DMA_BUSY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static DMA_FAILS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static BOUNCE_PHYS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static TABLE_PHYS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Allocate the bounce buffer and descriptor page below 4 GiB. False = no DMA.
/// Largest span (4 KiB blocks) one `DEVICES` hold may issue.
fn span_max_blocks() -> usize {
    if DMA_ON.load(Ordering::Relaxed) && DMA_READY.load(Ordering::Relaxed) { DMA_MAX_BLOCKS } else { MULTI_MAX_BLOCKS }
}

fn dma_alloc() -> bool {
    const LIMIT: usize = 0x1_0000_0000;
    if DMA_READY.load(Ordering::Relaxed) { return true; }
    let order = (BOUNCE_BYTES / 4096).trailing_zeros() as usize;
    let b = match mm::buddy::alloc_below(order, LIMIT) { Some(p) => p, None => return false };
    let t = match mm::buddy::alloc_below(0, LIMIT) {
        Some(p) => p,
        None => { mm::buddy::free(b, order); return false; }
    };
    unsafe {
        core::ptr::write_bytes(mm::phys_to_virt(b) as *mut u8, 0, BOUNCE_BYTES);
        core::ptr::write_bytes(mm::phys_to_virt(t) as *mut u8, 0, 4096);
    }
    BOUNCE_PHYS.store(b, Ordering::Relaxed);
    TABLE_PHYS.store(t, Ordering::Relaxed);
    DMA_READY.store(true, Ordering::Release);
    true
}

impl SdhciDevice {
    /// One ADMA2 CMD18/CMD25 of `count` sectors through the bounce buffer.
    /// Caller holds `DMA_BUSY`. On false the controller may be mid-transfer:
    /// the caller must `abort_multi()` before anything else.
    unsafe fn do_io_multi_dma(&self, is_write: bool, sector: u64, count: usize, buf: *mut u8) -> bool {
        let len = count * SECTOR_SIZE;
        let bphys = BOUNCE_PHYS.load(Ordering::Relaxed);
        let tphys = TABLE_PHYS.load(Ordering::Relaxed);
        let bvirt = mm::phys_to_virt(bphys);
        let tvirt = mm::phys_to_virt(tphys);
        let arg = if self.high_capacity { sector as u32 } else { (sector * SECTOR_SIZE as u64) as u32 };
        let cmd = if is_write { CMD_WRITE_MULTIPLE_BLOCK } else { CMD_READ_MULTIPLE_BLOCK };

        if is_write {
            core::ptr::copy_nonoverlapping(buf as *const u8, bvirt as *mut u8, len);
            mm::dma_cache::clean_range(bvirt, len);
        } else {
            mm::dma_cache::invalidate_range(bvirt, len);
        }
        // Descriptor table: one 8-byte entry (attr u16, length u16, address u32)
        // per 4 KiB page, END on the last.
        let nd = (len + ADMA_DESC_BYTES - 1) / ADMA_DESC_BYTES;
        for i in 0..nd {
            let off = i * ADMA_DESC_BYTES;
            let l = (len - off).min(ADMA_DESC_BYTES);
            let attr = ADMA_VALID | ADMA_TRAN | if i + 1 == nd { ADMA_END } else { 0 };
            let e = (tvirt + i * 8) as *mut u32;
            core::ptr::write_volatile(e, (attr as u32) | ((l as u32) << 16));
            core::ptr::write_volatile(e.add(1), (bphys + off) as u32);
        }
        mm::dma_cache::clean_range(tvirt, nd * 8);

        self.w16(REG_BLOCK_SIZE, (7 << 12) | SECTOR_SIZE as u16);
        self.w16(REG_BLOCK_COUNT, count as u16);
        let hc = self.r8(REG_HOST_CONTROL1);
        self.w8(REG_HOST_CONTROL1, (hc & !HC1_DMA_MASK) | HC1_ADMA2_32);
        self.w32(REG_ADMA_ADDR, tphys as u32);
        self.w32(REG_ADMA_ADDR + 4, 0);
        let tm = TM_DMA_EN | TM_BLOCK_COUNT_EN | TM_MULTI_BLOCK | TM_AUTO_CMD12
            | if is_write { 0 } else { TM_DIR_READ };
        self.w16(REG_TRANSFER_MODE, tm);

        let mut ok = self.send_command(cmd, arg, RESP_48, true, true, true).is_some()
            && self.wait_normal_int(NI_XFER_COMPLETE);
        if ok && self.r8(REG_ADMA_ERROR) & 0b111 != 0 { ok = false; }
        if ok && !is_write {
            mm::dma_cache::invalidate_range(bvirt, len);
            core::ptr::copy_nonoverlapping(bvirt as *const u8, buf, len);
        }
        // Leave Host Control 1 with SDMA selected, as PIO transfers expect.
        let hc = self.r8(REG_HOST_CONTROL1);
        self.w8(REG_HOST_CONTROL1, hc & !HC1_DMA_MASK);
        ok
    }

    /// Multi-block span: ADMA2 when available, PIO otherwise or when the DMA
    /// attempt fails (controller reset first, so the PIO retry starts clean).
    unsafe fn do_io_multi(&self, is_write: bool, sector: u64, count: usize, buf: *mut u8) -> bool {
        if DMA_ON.load(Ordering::Relaxed) && DMA_READY.load(Ordering::Acquire)
            && count * SECTOR_SIZE <= BOUNCE_BYTES
            && !DMA_BUSY.swap(true, Ordering::Acquire)
        {
            let ok = self.do_io_multi_dma(is_write, sector, count, buf);
            DMA_BUSY.store(false, Ordering::Release);
            if ok { return true; }
            self.abort_multi();
            if DMA_FAILS.fetch_add(1, Ordering::Relaxed) + 1 >= DMA_FAIL_LIMIT {
                DMA_ON.store(false, Ordering::Relaxed);
            }
        }
        self.do_io_multi_pio(is_write, sector, count, buf)
    }

    /// Probe-time proof, read-only: the first 16 sectors through ADMA2 must be
    /// byte-identical to the same read through PIO. Any mismatch or error
    /// disables DMA for good. Returns true when DMA stays on.
    unsafe fn dma_selftest(&self) -> bool {
        let mut a = [0u8; 16 * SECTOR_SIZE];
        let mut b = [0u8; 16 * SECTOR_SIZE];
        let pio_ok = self.do_io_multi_pio(false, 0, 16, a.as_mut_ptr());
        let dma_ok = pio_ok && self.do_io_multi_dma(false, 0, 16, b.as_mut_ptr());
        if !(pio_ok && dma_ok && a == b) {
            self.abort_multi();
            return false;
        }
        true
    }
}

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
const CMD_STOP_TRANSMISSION:  u8 = 12;
const CMD_READ_MULTIPLE_BLOCK:  u8 = 18;
const CMD_WRITE_MULTIPLE_BLOCK: u8 = 25;
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
    /// Length in 4096-byte blocks; 0 = unbounded (the f2fs device). Non-zero
    /// only for the boot-FAT device, whose reads/writes are bound by it.
    limit_blocks: u64,
    /// Boot-FAT device (`/dev/vdc`): reported read-only; the only write path
    /// is a root BLKWRWIN window in the VFS block layer, and every write is
    /// sector-granular and skips sectors whose bytes are unchanged.
    boot_fat: bool,
}

impl SdhciDevice {
    /// Whether `n` blocks at `blk` may be transferred.
    fn allowed(&self, is_write: bool, blk: u64, n: u64) -> bool {
        self.limit_blocks == 0 || blk.checked_add(n).map_or(false, |e| e <= self.limit_blocks)
    }
}

unsafe impl Send for SdhciDevice {}

impl SdhciDevice {
    unsafe fn r8(&self, off: usize) -> u8 { ((self.base + off) as *const u8).mmio_read() }
    unsafe fn w8(&self, off: usize, v: u8) { ((self.base + off) as *mut u8).mmio_write(v) }
    unsafe fn r16(&self, off: usize) -> u16 { ((self.base + off) as *const u16).mmio_read() }
    unsafe fn w16(&self, off: usize, v: u16) { ((self.base + off) as *mut u16).mmio_write(v) }
    unsafe fn r32(&self, off: usize) -> u32 { ((self.base + off) as *const u32).mmio_read() }
    unsafe fn w32(&self, off: usize, v: u32) { ((self.base + off) as *mut u32).mmio_write(v) }

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
            if spins & 1023 == 0 { tick_window(); }
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
            if spins & 1023 == 0 { tick_window(); }
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
            if spins & 1023 == 0 { tick_window(); }
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

    /// Read or write `count` (1..=MULTI_MAX_SECTORS) contiguous sectors with
    /// one CMD18/CMD25 and Auto CMD12, PIO through the buffer port.
    ///
    /// One command per span instead of one per sector: on a real card every
    /// CMD24 is a separate program cycle (~1 ms), which capped writes at
    /// ~0.5 MB/s; CMD25 lets the card stream the whole span.
    unsafe fn do_io_multi_pio(&self, is_write: bool, sector: u64, count: usize, buf: *mut u8) -> bool {
        let arg = if self.high_capacity { sector as u32 } else { (sector * SECTOR_SIZE as u64) as u32 };
        let cmd = if is_write { CMD_WRITE_MULTIPLE_BLOCK } else { CMD_READ_MULTIPLE_BLOCK };

        self.w16(REG_BLOCK_SIZE, SECTOR_SIZE as u16);
        self.w16(REG_BLOCK_COUNT, count as u16);
        let tm = TM_BLOCK_COUNT_EN | TM_MULTI_BLOCK | TM_AUTO_CMD12
            | if is_write { 0 } else { TM_DIR_READ };
        self.w16(REG_TRANSFER_MODE, tm);

        if self.send_command(cmd, arg, RESP_48, true, true, true).is_none() {
            return false;
        }
        let ready_bit = if is_write { NI_BUF_WRITE_READY } else { NI_BUF_READ_READY };
        for b in 0..count {
            if !self.wait_normal_int(ready_bit) {
                return false;
            }
            let p = buf.add(b * SECTOR_SIZE);
            for i in 0..(SECTOR_SIZE / 4) {
                let word_ptr = p.add(i * 4) as *mut u32;
                if is_write {
                    self.w32(REG_BUFFER_DATA, word_ptr.read_unaligned());
                } else {
                    word_ptr.write_unaligned(self.r32(REG_BUFFER_DATA));
                }
            }
            if b & 7 == 7 { tick_window(); }
        }
        // Transfer Complete follows the auto CMD12 (and, for a write, the
        // card's busy on DAT0).
        self.wait_normal_int(NI_XFER_COMPLETE)
    }

    /// `sdhci.multitest=1`: read the first `MULTITEST_BLOCKS` blocks of the
    /// partition single-sector and multi-block (spans of 16, 1 and 7 blocks,
    /// the shapes `span_io`/`read_block` issue) and compare. Read-only. One line:
    /// `[sdhci] multitest <n> blocks crc single=.. multi=.. fails=.. match|MISMATCH at block B sector S (+K more sectors) s=.. m=..`.
    unsafe fn multitest(&self) {
        const SHAPES: [usize; 3] = [16, 1, 7];
        let mut a = alloc::vec![0u8; MULTI_MAX_BLOCKS * BLOCK_SIZE];
        let mut b = alloc::vec![0u8; MULTI_MAX_BLOCKS * BLOCK_SIZE];
        let (mut crc_s, mut crc_m) = (!0u32, !0u32);
        let mut first: Option<(u64, usize, u32, u32)> = None;
        let (mut bad_sectors, mut fails, mut single_fail) = (0u32, 0u32, false);
        let (mut blk, mut k) = (0usize, 0usize);
        while blk < MULTITEST_BLOCKS {
            let n = SHAPES[k % SHAPES.len()].min(MULTITEST_BLOCKS - blk);
            k += 1;
            let len = n * BLOCK_SIZE;
            for x in b[..len].iter_mut() { *x = 0xa5; }
            for j in 0..n {
                if !self.do_io(false, (blk + j) as u64, a.as_mut_ptr().add(j * BLOCK_SIZE)) { single_fail = true; }
            }
            let sector = (blk as u64 + self.partition_offset) * SECTORS_PER_BLOCK as u64;
            if !self.do_io_multi(false, sector, n * SECTORS_PER_BLOCK, b.as_mut_ptr()) {
                fails += 1;
                self.abort_multi();
            }
            crc_s = crc32_update(crc_s, &a[..len]);
            crc_m = crc32_update(crc_m, &b[..len]);
            for s in 0..n * SECTORS_PER_BLOCK {
                let r = s * SECTOR_SIZE..(s + 1) * SECTOR_SIZE;
                if a[r.clone()] != b[r.clone()] {
                    bad_sectors += 1;
                    if first.is_none() {
                        let w = (0..SECTOR_SIZE / 4).find(|&w| a[r.start + 4 * w..r.start + 4 * w + 4] != b[r.start + 4 * w..r.start + 4 * w + 4]).unwrap_or(0);
                        let rd = |v: &[u8]| u32::from_le_bytes([v[r.start + 4 * w], v[r.start + 4 * w + 1], v[r.start + 4 * w + 2], v[r.start + 4 * w + 3]]);
                        first = Some(((blk + s / SECTORS_PER_BLOCK) as u64, s + (w << 16), rd(&a), rd(&b)));
                    }
                }
            }
            blk += n;
        }
        let hex = |v: u64, digits: usize| {
            let mut o = [0u8; 16];
            for i in 0..digits { let nib = ((v >> (4 * (digits - 1 - i))) & 0xf) as u8; o[i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 }; }
            o
        };
        let p = |s: &str| crate::kenv::pci::serial_debug(s);
        let ph = |v: u64, d: usize| { let o = hex(v, d); crate::kenv::pci::serial_debug(core::str::from_utf8(&o[..d]).unwrap_or("?")); };
        p("[sdhci] multitest 1024 blocks (4 MiB, spans 16/1/7) crc single="); ph(!crc_s as u64, 8);
        p(" multi="); ph(!crc_m as u64, 8);
        p(" multifails="); ph(fails as u64, 4);
        if single_fail { p(" SINGLE-SECTOR-READ-FAILED"); }
        match first {
            None => p(" match\n"),
            Some((fb, sw, sv, mv)) => {
                p(" MISMATCH at block "); ph(fb, 8);
                p(" span-sector "); ph((sw & 0xffff) as u64, 4);
                p(" word "); ph((sw >> 16) as u64, 4);
                p(" single="); ph(sv as u64, 8);
                p(" multi="); ph(mv as u64, 8);
                p(" badsectors="); ph(bad_sectors as u64, 8);
                p("\n");
            }
        }
    }

    /// After a failed multi-block transfer: reset CMD/DAT, send a manual
    /// CMD12 so a card left in the data or receive state returns to
    /// transfer state (an error reply from a card already there is fine),
    /// and reset again.
    unsafe fn abort_multi(&self) {
        self.reset_cmd_dat();
        self.w32(REG_ARGUMENT1, 0);
        self.wait_cmd_inhibit_clear(false);
        self.w16(REG_NORMAL_INT_STATUS, NI_ALL);
        self.w16(REG_ERROR_INT_STATUS, EI_ALL);
        self.w16(REG_COMMAND, ((CMD_STOP_TRANSMISSION as u16) << 8) | RESP_48B
            | CMD_CRC_CHECK | CMD_IDX_CHECK | CMD_TYPE_ABORT);
        let _ = self.wait_normal_int(NI_CMD_COMPLETE);
        self.wait_cmd_inhibit_clear(true);
        self.reset_cmd_dat();
    }

    /// Raw sector I/O confined to the scratch window. Every access (reads
    /// too) is range-checked here, the single chokepoint of `sdtest`.
    unsafe fn scratch_io(&self, is_write: bool, multi: bool, sector: u64, count: usize, buf: *mut u8) -> bool {
        if count == 0 || sector < SCRATCH_FIRST || sector.checked_add(count as u64).map_or(true, |e| e > SCRATCH_END) {
            crate::kenv::pci::serial_debug("[sdhci] sdtest: RANGE VIOLATION refused\n");
            return false;
        }
        let ok = if multi {
            self.do_io_multi(is_write, sector, count, buf)
        } else {
            (0..count).all(|i| self.do_io_sector(is_write, sector + i as u64, buf.add(i * SECTOR_SIZE)))
        };
        if !ok { self.abort_multi(); }
        ok
    }

    /// `sdhci.sdtest=<MiB>`: speed table and write/readback proof inside the
    /// scratch window only (the last 8 GiB of the card, free of partitions).
    /// Bypasses `MULTI_ON`; always exercises the raw multi/single paths.
    unsafe fn sdtest(&self, mib: u32) {
        let p = |s: &str| crate::kenv::pci::serial_debug(s);
        let ph = |v: u64| {
            let mut o = [0u8; 16];
            for i in 0..16 { let nib = ((v >> (4 * (15 - i))) & 0xf) as u8; o[i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 }; }
            let s = o.iter().position(|&c| c != b'0').unwrap_or(15);
            crate::kenv::pci::serial_debug(core::str::from_utf8(&o[s..]).unwrap_or("?"));
        };
        let pd = |mut v: u64| {
            let mut o = [0u8; 20]; let mut i = 20;
            if v == 0 { i -= 1; o[i] = b'0'; }
            while v > 0 { i -= 1; o[i] = b'0' + (v % 10) as u8; v /= 10; }
            crate::kenv::pci::serial_debug(core::str::from_utf8(&o[i..]).unwrap_or("?"));
        };
        let fill = |buf: &mut [u8], seed: u64| {
            let mut x = seed | 1;
            for w in buf.chunks_exact_mut(8) {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                w.copy_from_slice(&x.to_le_bytes());
            }
        };
        let mut wb = alloc::vec![0u8; DMA_MAX_SECTORS * SECTOR_SIZE];
        let mut rb = alloc::vec![0u8; DMA_MAX_SECTORS * SECTOR_SIZE];
        p("[sdhci] sdtest start mib="); pd(mib as u64); p("\n");
        // Phase A: speed. 16 MiB sequential from the window start, 64 KiB spans.
        let dma_live = DMA_ON.load(Ordering::Relaxed) && DMA_READY.load(Ordering::Relaxed);
        for (name, multi, is_write, sp, t_mib) in [("single write", false, true, 128usize, 4u64), ("single read", false, false, 128, 4),
                                        ("multi write", true, true, 128, 4), ("multi read", true, false, 128, 4),
                                        ("multi512k write", true, true, if dma_live { 1024 } else { 128 }, 16),
                                        ("multi512k read", true, false, if dma_live { 1024 } else { 128 }, 16)] {
            let spans = (t_mib * 1048576 / (sp * SECTOR_SIZE) as u64) as usize;
            let t0 = sched::monotonic_ns();
            let mut ok = true;
            for s in 0..spans {
                if is_write { fill(&mut wb[..], 0x1234_5678 + s as u64); }
                let b = if is_write { wb.as_mut_ptr() } else { rb.as_mut_ptr() };
                if !self.scratch_io(is_write, multi, SCRATCH_FIRST + (s * sp) as u64, sp, b) { ok = false; break; }
                if s % 16 == 0 && !name.starts_with("multi") { p("[sdhci] sdtest A "); p(name); p(" span "); pd(s as u64); p(" t_ms="); pd((sched::monotonic_ns() - t0) / 1_000_000); p("\n"); }
            }
            let dt = (sched::monotonic_ns() - t0).max(1);
            p("[sdhci] sdtest speed "); p(name); p(": "); pd(t_mib * 1048576 * 1_000_000_000 / dt / 1024); p(" KiB/s ok="); pd(ok as u64); p("\n");
        }
        // Phase B: mixed-size multi writes, verified by single-sector readback
        // (writes) and multi readback (reads), plus guard sectors either side.
        const SIZES: [usize; 16] = [1, 2, 3, 4, 7, 8, 15, 16, 31, 32, 64, 100, 128, 300, 512, 1024];
        let mut rng: u64 = 0x9e37_79b9_7f4a_7c15 ^ (sched::monotonic_ns() | 1);
        let (mut done, mut wbad, mut rbad, mut gbad, mut ioerr, mut iters) = (0u64, 0u32, 0u32, 0u32, 0u32, 0u64);
        let target = mib as u64 * 1048576;
        let mut g = [0u8; 2 * SECTOR_SIZE];
        while done < target {
            rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
            let n = SIZES[(rng >> 40) as usize % SIZES.len()];
            let start = SCRATCH_FIRST + 1 + ((rng >> 8) % ((1 << 21) - 1100));
            let len = n * SECTOR_SIZE;
            fill(&mut wb[..len], rng);
            let guard = iters % 4 == 0;
            if guard {
                fill(&mut g, rng ^ 0xdead_beef);
                if !self.scratch_io(true, false, start - 1, 1, g.as_mut_ptr())
                    || !self.scratch_io(true, false, start + n as u64, 1, g.as_mut_ptr().add(SECTOR_SIZE)) { ioerr += 1; }
            }
            if iters < 40 { p("[sdhci] sdtest B mw "); ph(start); p(" n="); pd(n as u64); p(if guard { " g\n" } else { "\n" }); }
            if !self.scratch_io(true, true, start, n, wb.as_mut_ptr()) { ioerr += 1; iters += 1; p("[sdhci] sdtest B mw FAILED\n"); continue; }
            if iters < 40 { p("[sdhci] sdtest B mw ok\n"); }
            let mut bad = false;
            if !self.scratch_io(false, false, start, n, rb.as_mut_ptr()) { ioerr += 1; }
            else if rb[..len] != wb[..len] {
                wbad += 1; bad = true;
                let d = (0..len).find(|&i| rb[i] != wb[i]).unwrap_or(0);
                if wbad <= 8 { p("[sdhci] sdtest WRITE MISMATCH sector "); ph(start); p(" n="); pd(n as u64); p(" first diff byte "); pd(d as u64); p("\n"); }
            }
            if !self.scratch_io(false, true, start, n, rb.as_mut_ptr()) { ioerr += 1; }
            else if rb[..len] != wb[..len] {
                rbad += 1;
                if rbad <= 8 { p("[sdhci] sdtest MULTI-READ MISMATCH sector "); ph(start); p(" n="); pd(n as u64); p(if bad { " (write bad too)\n" } else { " (single read agrees with write)\n" }); }
            }
            if guard {
                let mut r2 = [0u8; 2 * SECTOR_SIZE];
                if !self.scratch_io(false, false, start - 1, 1, r2.as_mut_ptr())
                    || !self.scratch_io(false, false, start + n as u64, 1, r2.as_mut_ptr().add(SECTOR_SIZE)) { ioerr += 1; }
                else if r2 != g { gbad += 1; if gbad <= 8 { p("[sdhci] sdtest GUARD CLOBBERED at span sector "); ph(start); p(" n="); pd(n as u64); p("\n"); } }
            }
            done += len as u64; iters += 1;
            if iters % 256 == 0 { p("[sdhci] sdtest progress MiB="); pd(done >> 20); p(" wbad="); pd(wbad as u64); p(" rbad="); pd(rbad as u64); p(" gbad="); pd(gbad as u64); p(" ioerr="); pd(ioerr as u64); p("\n"); }
        }
        p("[sdhci] sdtest DONE MiB="); pd(done >> 20); p(" iters="); pd(iters);
        p(" wbad="); pd(wbad as u64); p(" rbad="); pd(rbad as u64); p(" gbad="); pd(gbad as u64); p(" ioerr="); pd(ioerr as u64);
        p(if wbad == 0 && rbad == 0 && gbad == 0 && ioerr == 0 { " PASS\n" } else { " FAIL\n" });
    }

    /// `nblocks` (1..=MULTI_MAX_BLOCKS) contiguous 4 KiB blocks. Multi-block
    /// first; on any failure the span is redone with the single-sector path,
    /// which stays the only path once multi-block has proven unreliable.
    fn do_io_span(&self, is_write: bool, blk: u64, nblocks: usize, buf: *mut u8) -> bool {
        self.maybe_retune();
        if self.boot_fat {
            // Single-sector commands only (no multi-block), see `do_io`.
            return (0..nblocks).all(|j| self.do_io(is_write, blk + j as u64,
                unsafe { buf.add(j * BLOCK_SIZE) }));
        }
        if MULTI_ON.load(Ordering::Relaxed) {
            let sector = (blk + self.partition_offset) * SECTORS_PER_BLOCK as u64;
            tick_window();
            if unsafe { self.do_io_multi(is_write, sector, nblocks * SECTORS_PER_BLOCK, buf) } {
                return true;
            }
            let latched = LAST_FAIL.load(Ordering::Relaxed);
            unsafe { self.abort_multi(); self.retune(); }
            let ok = (0..nblocks).all(|j| self.do_io(is_write, blk + j as u64,
                unsafe { buf.add(j * BLOCK_SIZE) }));
            crate::kenv::pci::serial_debug(if is_write { "[sdhci] multi-block write" } else { "[sdhci] multi-block read" });
            crate::kenv::pci::serial_debug(" failed (N<<16|E latched: ");
            let mut hx = [0u8; 8];
            for (i, c) in hx.iter_mut().enumerate() {
                let nib = ((latched >> (28 - 4 * i)) & 0xf) as u8;
                *c = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
            }
            crate::kenv::pci::serial_debug(core::str::from_utf8(&hx).unwrap_or("?"));
            crate::kenv::pci::serial_debug(if ok { "), single-sector redo ok\n" } else { "), single-sector redo failed too\n" });
            if ok && MULTI_FALLBACKS.fetch_add(1, Ordering::Relaxed) + 1 >= MULTI_FALLBACK_LIMIT
                && MULTI_ON.swap(false, Ordering::Relaxed)
            {
                crate::kenv::pci::serial_debug("[sdhci] multi-block disabled; I/O is single-sector PIO from now on\n");
            }
            return ok;
        }
        (0..nblocks).all(|j| self.do_io(is_write, blk + j as u64, unsafe { buf.add(j * BLOCK_SIZE) }))
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
            tick_window();
            if is_write && self.boot_fat {
                // A 4 KiB VFS write over a 512 B edit must not re-write (and
                // so risk tearing) the 7 unchanged sectors: read, compare, skip.
                let mut cur = [0u8; SECTOR_SIZE];
                // A failed compare read is an I/O error: never write blind.
                if !unsafe { self.do_io_sector(false, sector, cur.as_mut_ptr()) } { return false; }
                if cur == unsafe { core::slice::from_raw_parts(buf.add(i * SECTOR_SIZE), SECTOR_SIZE) } { continue; }
            }
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
                    self.retune();
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

// ── UHS-I (SDR50 / SDR104) ───────────────────────────────────────────────────
//
// `sdhci.uhs=1`. Mirrors what Linux's sdhci-brcmstb + mmc core do on this
// board (DT: `vqmmc-supply` = `sd-io-1v8-reg`, a regulator-gpio on gio_aon 3,
// high = 1.8 V, 5 ms settle; `vmmc-supply` = `sd-vcc-reg`, a regulator-fixed on
// gio_aon 4 = SD_PWR_ON, high = on):
//
//   power-cycle card (3.3 V I/O) -> ACMD41 with S18R -> S18A? -> CMD11 ->
//   stop clock, DAT0 low -> I/O regulator 1.8 V + Host Control 2 1.8 V enable
//   -> 5 ms -> clock on, DAT[3:0] high -> CMD2/3/9/7 -> ACMD6 -> CMD6 SDR104
//   (or SDR50) -> Host Control 2 mode, clock -> CMD19 tuning -> verify.
//
// Any failure after CMD11 (or any verify failure) returns `None` from the
// attempt; `probe_card` then power-cycles the card through SD_PWR_ON (the same
// software power cycle Linux does at every boot) and re-probes at 3.3 V HS.

const REG_HOST_CONTROL2:  usize = 0x3E; // u16
const REG_CAPABILITIES_1: usize = 0x44; // u32
const HC2_MODE_MASK:   u16 = 0x7;
const HC2_1V8:         u16 = 1 << 3;
const HC2_EXEC_TUNING: u16 = 1 << 6;
const HC2_SAMPLING:    u16 = 1 << 7;
const PSTATE_DAT0:     u32 = 1 << 20;
const PSTATE_DAT_ALL:  u32 = 0xF << 20;
const CMD_VOLTAGE_SWITCH: u8 = 11;
const CMD_SEND_TUNING:    u8 = 19;
const CFG_OFF: usize = 0x400;

static UHS_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
static UHS_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static UHS_LAST_TUNE_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static UHS_RETUNES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
const UHS_RETUNE_NS: u64 = 4_000_000_000;

fn delay_us(us: u64) { crate::kdiag::delay_us(us) }

#[cfg(feature = "rpi5")]
mod board {
    use crate::bcm2712_gpio::{gpio_get, gpio_set_output, Chip};
    pub fn available() -> bool { gpio_get(Chip::GioAon, 3).is_ok() && gpio_get(Chip::GioAon, 4).is_ok() }
    pub fn io_1v8(on: bool) -> bool { gpio_set_output(Chip::GioAon, 3, on).is_ok() }
    pub fn card_power(on: bool) -> bool { gpio_set_output(Chip::GioAon, 4, on).is_ok() }
    pub fn io_is_1v8() -> bool { gpio_get(Chip::GioAon, 3).unwrap_or(false) }
}
#[cfg(not(feature = "rpi5"))]
mod board {
    pub fn available() -> bool { false }
    pub fn io_1v8(_: bool) -> bool { false }
    pub fn card_power(_: bool) -> bool { false }
    pub fn io_is_1v8() -> bool { false }
}

impl SdhciDevice {
    /// Software power cycle of the card (SD_PWR_ON low, I/O back to 3.3 V,
    /// Host Control 2 cleared), exactly the reset a card left at 1.8 V needs.
    /// Returns false (nothing changed) if the GPIO is not reachable.
    unsafe fn card_power_cycle(&self) -> bool {
        if !board::available() { return false; }
        self.w16(REG_CLOCK_CONTROL, 0);
        if !board::card_power(false) { return false; }
        let _ = board::io_1v8(false);
        self.w16(REG_HOST_CONTROL2, 0);
        delay_us(200_000);
        let ok = board::card_power(true);
        delay_us(50_000);
        ok
    }

    /// CMD11 + the host-side 1.8 V switch. False on any failure (the caller
    /// power-cycles the card).
    unsafe fn voltage_switch_1v8(&self) -> bool {
        if self.send_command(CMD_VOLTAGE_SWITCH, 0, RESP_48, false, true, true).is_none() {
            self.reset_cmd_dat();
            dbg_str("[sdhci] uhs: CMD11 no response\n");
            return false;
        }
        let cc = self.r16(REG_CLOCK_CONTROL);
        self.w16(REG_CLOCK_CONTROL, cc & !CLK_SD_EN);
        delay_us(2_000);
        if self.r32(REG_PRESENT_STATE) & PSTATE_DAT0 != 0 {
            dbg_str("[sdhci] uhs: DAT0 not low after CMD11\n");
            return false;
        }
        if !board::io_1v8(true) { dbg_str("[sdhci] uhs: vqmmc gpio failed\n"); return false; }
        self.w16(REG_HOST_CONTROL2, self.r16(REG_HOST_CONTROL2) | HC2_1V8);
        delay_us(10_000);
        if self.r16(REG_HOST_CONTROL2) & HC2_1V8 == 0 {
            dbg_str("[sdhci] uhs: HC2 1.8V bit did not stick\n");
            return false;
        }
        self.w16(REG_CLOCK_CONTROL, self.r16(REG_CLOCK_CONTROL) | CLK_SD_EN);
        delay_us(10_000);
        let ps = self.r32(REG_PRESENT_STATE);
        self.w16(REG_NORMAL_INT_STATUS, NI_ALL);
        self.w16(REG_ERROR_INT_STATUS, EI_ALL);
        if ps & PSTATE_DAT_ALL != PSTATE_DAT_ALL {
            dbg_str("[sdhci] uhs: DAT lines not high after switch, pstate=");
            dbg_hex(ps as u64);
            dbg(b'\n');
            return false;
        }
        true
    }

    /// Program a UHS timing: width 4, HC2 mode select, clock.
    unsafe fn apply_uhs(&self, mode: u16, clk_sel: u16) {
        let cc = self.r16(REG_CLOCK_CONTROL);
        self.w16(REG_CLOCK_CONTROL, cc & !CLK_SD_EN);
        let hc = self.r8(REG_HOST_CONTROL1);
        self.w8(REG_HOST_CONTROL1, hc | HC1_DATA_WIDTH_4 | if mode != 0 { HC1_HIGH_SPEED } else { 0 });
        let hc2 = self.r16(REG_HOST_CONTROL2);
        self.w16(REG_HOST_CONTROL2, (hc2 & !(HC2_MODE_MASK | HC2_EXEC_TUNING | HC2_SAMPLING)) | mode);
        self.w16(REG_CLOCK_CONTROL, clk_sel | CLK_INTERNAL_EN);
        let mut spins: u32 = 0;
        while self.r16(REG_CLOCK_CONTROL) & CLK_STABLE == 0 {
            spins += 1;
            if spins > SPIN_LIMIT { dbg(b'k'); break; }
            core::hint::spin_loop();
        }
        self.w16(REG_CLOCK_CONTROL, self.r16(REG_CLOCK_CONTROL) | CLK_SD_EN);
        delay_us(200);
    }

    /// SDHCI Execute Tuning with CMD19 (up to 40 tries). On failure the
    /// previous Host Control 2 value (minus the tuning bit) is restored.
    unsafe fn tune_sdr(&self, verbose: bool) -> bool {
        let saved = self.r16(REG_HOST_CONTROL2);
        self.w16(REG_HOST_CONTROL2, saved | HC2_EXEC_TUNING);
        let mut done = false;
        let mut errs = 0u32;
        let mut iters = 0u32;
        for _ in 0..40 {
            iters += 1;
            self.wait_cmd_inhibit_clear(true);
            self.w16(REG_NORMAL_INT_STATUS, NI_ALL);
            self.w16(REG_ERROR_INT_STATUS, EI_ALL);
            self.w16(REG_BLOCK_SIZE, 64);
            self.w16(REG_BLOCK_COUNT, 1);
            self.w16(REG_TRANSFER_MODE, TM_DIR_READ);
            self.w32(REG_ARGUMENT1, 0);
            self.w16(REG_COMMAND, ((CMD_SEND_TUNING as u16) << 8) | RESP_48 | CMD_CRC_CHECK | CMD_IDX_CHECK | CMD_DATA_PRESENT);
            let mut got = false;
            let mut spins: u32 = 0;
            loop {
                let st = self.r16(REG_NORMAL_INT_STATUS);
                if st & NI_ERROR != 0 { break; }
                if st & NI_BUF_READ_READY != 0 { got = true; break; }
                spins += 1;
                if spins > 4_000_000 { break; }
                core::hint::spin_loop();
            }
            if got {
                self.w16(REG_NORMAL_INT_STATUS, NI_BUF_READ_READY);
                for _ in 0..16 { let _ = self.r32(REG_BUFFER_DATA); }
                let t = self.r16(REG_NORMAL_INT_STATUS);
                if t & NI_XFER_COMPLETE != 0 { self.w16(REG_NORMAL_INT_STATUS, NI_XFER_COMPLETE); }
            } else {
                errs += 1;
                self.reset_cmd_dat();
            }
            if self.r16(REG_HOST_CONTROL2) & HC2_EXEC_TUNING == 0 { done = true; break; }
        }
        let hc2 = self.r16(REG_HOST_CONTROL2);
        let ok = done && hc2 & HC2_SAMPLING != 0;
        self.reset_cmd_dat();
        if !ok {
            self.w16(REG_HOST_CONTROL2, saved & !HC2_EXEC_TUNING);
        }
        // `dbg*` pokes UART10 by physical address: only valid at probe time
        // (identity map); at runtime a retune must stay silent here.
        if verbose {
            dbg_str("[sdhci] uhs: tuning ");
            dbg_str(if ok { "ok" } else { "FAILED" });
            dbg_str(" iters=");
            dbg_dec(iters);
            dbg_str(" errs=");
            dbg_dec(errs);
            dbg_str(" hc2=");
            dbg_hex(self.r16(REG_HOST_CONTROL2) as u64);
            dbg(b'\n');
        }
        ok
    }

    unsafe fn log_cfg(&self) {
        let c = self.base + CFG_OFF;
        let r = |o: usize| ((c + o) as *const u32).mmio_read();
        dbg_str("[sdhci] uhs: cfg ctrl=");
        dbg_hex(r(0x00) as u64);
        dbg_str(" pinsel=");
        dbg_hex(r(0x44) as u64);
        dbg_str(" max50=");
        dbg_hex(r(0x1ac) as u64);
        dbg_str(" iovdd1v8=");
        dbg_dec(board::io_is_1v8() as u32);
        dbg(b'\n');
    }

    /// Re-tune (periodic, or after an I/O error). Idle bus only.
    unsafe fn retune(&self) {
        if !UHS_ACTIVE.load(Ordering::Relaxed) { return; }
        let ok = self.tune_sdr(false);
        UHS_LAST_TUNE_NS.store(crate::kdiag::now_ns(), Ordering::Relaxed);
        UHS_RETUNES.fetch_add(1, Ordering::Relaxed);
        if !ok { crate::kenv::pci::serial_debug("[sdhci] uhs: retune failed, keeping previous sampling point\n"); }
    }

    #[inline]
    fn maybe_retune(&self) {
        if UHS_ACTIVE.load(Ordering::Relaxed) {
            let now = crate::kdiag::now_ns();
            if now.wrapping_sub(UHS_LAST_TUNE_NS.load(Ordering::Relaxed)) > UHS_RETUNE_NS {
                unsafe { self.retune(); }
            }
        }
    }

    unsafe fn crc_chunks(&self, out: &mut [u32; 8], buf: &mut [u8; 16 * SECTOR_SIZE]) -> bool {
        for (i, o) in out.iter_mut().enumerate() {
            if !self.do_io_multi(false, (i * 16) as u64, 16, buf.as_mut_ptr()) {
                self.reset_cmd_dat();
                return false;
            }
            *o = crc32_update(0xFFFF_FFFF, buf);
        }
        true
    }
}

/// UHS negotiation after the 1.8 V switch and card identification.
/// False = give up (caller power-cycles and falls back to 3.3 V HS).
unsafe fn negotiate_uhs(dev: &SdhciDevice, rca: u32) -> bool {
    let spec = (dev.r16(REG_HOST_VERSION) & 0xFF) as u32;
    let v3 = spec >= 2;
    let caps = dev.r32(REG_CAPABILITIES);
    let caps1 = dev.r32(REG_CAPABILITIES_1);
    let caps_base_mhz = if v3 { (caps >> 8) & 0xFF } else { (caps >> 8) & 0x3F };
    let base_khz = if caps_base_mhz != 0 { caps_base_mhz * 1000 } else { BASE_CLOCK_FALLBACK_KHZ };
    dbg_str("[sdhci] uhs: caps=");
    dbg_hex(caps as u64);
    dbg_str(" caps1=");
    dbg_hex(caps1 as u64);
    dbg_str(" base=");
    dbg_dec(base_khz);
    dbg_str("kHz\n");
    let can104 = caps1 & 2 != 0;
    let can50 = caps1 & 1 != 0;

    let floor = BusState { host_ctl: 0, clk_sel: CLK_DIV_MAX, clk_khz: base_khz / 510 };
    let mut good = floor;
    let mut reference = [0u8; SECTOR_SIZE];
    if !dev.do_io_sector(false, 0, reference.as_mut_ptr()) { dev.reset_cmd_dat(); return false; }

    // 4-bit, then 25 MHz at SDR12 timing.
    if !(dev.app_cmd(rca)
        && dev.send_command(ACMD_SET_BUS_WIDTH, 0b10, RESP_48, false, true, true).is_some())
    {
        dev.reset_cmd_dat();
        dbg_str("[sdhci] uhs: ACMD6 failed\n");
        return false;
    }
    let wide = BusState { host_ctl: HC1_DATA_WIDTH_4, ..good };
    if !try_rung(dev, rca, &reference, floor, &mut good, wide) { return false; }
    let (clk_sel, clk_khz) = clock_divider(base_khz, 25_000, v3);
    let r25 = BusState { clk_sel, clk_khz, ..good };
    if !try_rung(dev, rca, &reference, floor, &mut good, r25) { return false; }

    // Supported functions.
    let mut st = [0u8; SWITCH_STATUS_LEN];
    if !dev.read_data_command(CMD_SWITCH_FUNC, 0x00FF_FFF1, &mut st) {
        dev.reset_cmd_dat();
        dbg_str("[sdhci] uhs: CMD6 query failed\n");
        return false;
    }
    let bitmap = st[13];
    dbg_str("[sdhci] uhs: card group1 bitmap=");
    dbg_hex(bitmap as u64);
    dbg(b'\n');

    // Reference CRCs at the verified 25 MHz.
    let mut buf = [0u8; 16 * SECTOR_SIZE];
    let mut refcrc = [0u32; 8];
    if !dev.crc_chunks(&mut refcrc, &mut buf) { return false; }

    let cands: [(u16, u32, bool, &str); 2] = [
        (3, 200_000, can104 && bitmap & 0x08 != 0, "SDR104"),
        (2, 100_000, can50 && bitmap & 0x04 != 0, "SDR50"),
    ];
    for (func, khz, avail, name) in cands {
        if !avail { continue; }
        let (clk_sel, clk_khz) = clock_divider(base_khz, khz, v3);
        if clk_khz * 4 < khz * 3 {
            dbg_str("[sdhci] uhs: base clock too low for ");
            dbg_str(name);
            dbg(b'\n');
            continue;
        }
        if !(dev.read_data_command(CMD_SWITCH_FUNC, 0x80FF_FFF0 | func as u32, &mut st)
            && (st[16] & 0x0F) as u16 == func)
        {
            dev.reset_cmd_dat();
            dbg_str("[sdhci] uhs: CMD6 switch refused for ");
            dbg_str(name);
            dbg(b'\n');
            dev.apply_uhs(0, good.clk_sel);
            continue;
        }
        {
            // SD_PIN_SEL = card (Linux reads 2 here; we left whatever firmware set).
            let pm = (dev.base + CFG_OFF + 0x44) as *mut u32;
            let pv = pm.mmio_read();
            if pv & 3 != 2 { pm.mmio_write((pv & !3) | 2); }
        }
        dev.apply_uhs(func, clk_sel);
        if func == 3 {
            // Linux reaches SDR104 on this controller; make sure the strap
            // does not cap the clock at 50 MHz (SDIO_CFG_MAX_50MHZ_MODE).
            let m = (dev.base + CFG_OFF + 0x1ac) as *mut u32;
            let v = m.mmio_read();
            dbg_str("[sdhci] uhs: max50 cfg before=");
            dbg_hex(v as u64);
            // Linux (sdhci-brcmstb) leaves 0x80000000: STRAP_OVERRIDE (bit 31) set,
            // MAX_50MHZ_MODE_ENABLE (bit 0) clear. The previous revision wrote the
            // inverse, which caps the clock path at 50 MHz and starves tuning.
            if v & 1 != 0 || v & (1 << 31) == 0 {
                m.mmio_write((v | (1u32 << 31)) & !1u32);
                dbg_str(" -> ");
                dbg_hex(m.mmio_read() as u64);
            }
            dbg(b'\n');
            delay_us(5_000);
        }
        let mut ok = dev.tune_sdr(true);
        if !ok && func == 3 {
            delay_us(10_000);
            ok = dev.tune_sdr(true);
        }
        ok = ok && dev.verify_bus(&reference);
        if ok {
            let mut c = [0u32; 8];
            for _ in 0..4 {
                if !dev.crc_chunks(&mut c, &mut buf) || c != refcrc { ok = false; break; }
            }
        }
        if ok {
            UHS_LAST_TUNE_NS.store(crate::kdiag::now_ns(), Ordering::Relaxed);
            UHS_ACTIVE.store(true, Ordering::Relaxed);
            dbg_str("[sdhci] UHS-I ");
            dbg_str(name);
            dbg_str(" active: bus width=4 clk=");
            dbg_dec(clk_khz);
            dbg_str("kHz (1.8V, tuned)\n");
            // dbg* only reaches the debug UART; mirror the verdict into the
            // kernel log so dmesg shows which mode the card runs in.
            crate::kenv::pci::serial_debug("[sdhci] UHS-I ");
            crate::kenv::pci::serial_debug(name);
            crate::kenv::pci::serial_debug(" active: bus width=4 clk=");
            crate::kenv::pci::serial_debug_dec(clk_khz as u64);
            crate::kenv::pci::serial_debug("kHz (1.8V, tuned)\n");
            return true;
        }
        dbg_str("[sdhci] uhs: ");
        dbg_str(name);
        dbg_str(" did not verify, stepping down\n");
        dev.reset_cmd_dat();
        dev.apply_uhs(0, good.clk_sel);
    }
    false
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
    use crate::rpi_misc_logic::sdmbr;
    // Every read here is ABSOLUTE on the card: a raw copy with offset 0, so
    // this stays right even if `dev` already carries the root offset (it did
    // once, and the superblock was then read at twice the offset: the size
    // was unknown and the guard covered p2..end of card, EBUSY for p3/p4).
    let raw = SdhciDevice { base: dev.base, high_capacity: dev.high_capacity, partition_offset: 0, limit_blocks: 0, boot_fat: false };
    let mut mbr = [0u8; BLOCK_SIZE];
    if !raw.do_io(false, 0, mbr.as_mut_ptr()) { return None; }
    // A dual-boot card has two 0x83 partitions (F2FS root, Raspberry Pi OS
    // ext4 root): take the one whose superblock carries the F2FS magic.
    let g = sdmbr::root_guard(&mbr[..512], |blk, buf| raw.do_io(false, blk, buf.as_mut_ptr()))?;
    ROOT_FS_BYTES.store(g.bytes, Ordering::Relaxed);
    crate::kenv::pci::serial_debug(&alloc::format!(
        "[sdhci] root guard: raw writes EBUSY in sectors {}..{} (root fs {} bytes, from {})\n",
        g.start_lba, g.end_lba(), g.bytes, if g.from_sb { "superblock" } else { "MBR entry: fs size unknown" }));
    // MBR LBA is in 512-byte sectors; our blocks are 4096 bytes.
    Some(g.start_lba / SECTORS_PER_BLOCK as u64)
}

/// Whole card in 4096-byte blocks, from the CSD (0 = unknown).
static CARD_BLOCKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Byte length of the F2FS root filesystem (superblock block_count), 0 = unknown.
static ROOT_FS_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Device index of the whole card (`/dev/mmcblk0`, rpi5 only).
pub const WHOLE_IDX: usize = 3;

/// CSD v2.0 (SDHC/SDXC): C_SIZE = CSD[69:48], capacity (C_SIZE+1) * 512 KiB.
/// The response registers hold CSD[127:8] as bits [119:0] (see probe_card),
/// so C_SIZE is word 1 bits [29:8] and CSD_STRUCTURE word 3 bits [23:22].
/// CSD v1 (SDSC, <= 2 GB) cards are left at unknown.
fn card_capacity_from_csd(csd: &[u32; 4]) {
    if (csd[3] >> 22) & 0x3 != 1 { return; }
    let c_size = ((csd[1] >> 8) & 0x3F_FFFF) as u64;
    CARD_BLOCKS.store((c_size + 1) * 512 * 1024 / BLOCK_SIZE as u64, Ordering::Relaxed);
}

/// Absolute byte range of the mounted F2FS root on device `idx` (writes
/// there must go through the filesystem, never a raw node): `[0, fs)` on
/// vdb and `[p2 start, + fs)` on the whole card.
pub fn busy_range(idx: usize) -> Option<(u64, u64)> {
    let fs = ROOT_FS_BYTES.load(Ordering::Relaxed);
    let devs = DEVICES.lock();
    let root = devs.get(1)?.as_ref()?;
    match idx {
        1 => Some((0, if fs == 0 { u64::MAX } else { fs })),
        WHOLE_IDX if devs.get(WHOLE_IDX)?.is_some() =>
            Some((root.partition_offset * BLOCK_SIZE as u64, if fs == 0 { u64::MAX / 2 } else { fs })),
        _ => None,
    }
}

/// The whole-card device refuses (driver-level, below the VFS EBUSY check)
/// any write overlapping the mounted root filesystem.
fn whole_write_blocked(dev_idx: usize, blk: u64, n: u64) -> bool {
    if dev_idx != WHOLE_IDX { return false; }
    match busy_range(WHOLE_IDX) {
        Some((s, l)) => {
            let (a, e) = (blk * BLOCK_SIZE as u64, (blk + n) * BLOCK_SIZE as u64);
            a < s.saturating_add(l) && s < e
        }
        None => false,
    }
}

pub fn is_whole_card(idx: usize) -> bool {
    idx == WHOLE_IDX && matches!(DEVICES.lock().get(idx), Some(Some(_)))
}

/// MBR partition 1 if it is FAT32 (0x0b/0x0c) and 4 KiB-aligned:
/// `(offset, length)` in 4096-byte blocks (length rounded down).
#[cfg(feature = "rpi5")]
unsafe fn find_boot_partition(dev: &SdhciDevice) -> Option<(u64, u64)> {
    use alloc::format;
    let serial = crate::kenv::pci::serial_debug;
    // `dev` already carries the f2fs partition offset: read the MBR through
    // a raw copy (offset 0, no limit), as find_f2fs_partition does.
    let raw = SdhciDevice { base: dev.base, high_capacity: dev.high_capacity, partition_offset: 0, limit_blocks: 0, boot_fat: false };
    let mut mbr = [0u8; BLOCK_SIZE];
    if !raw.do_io(false, 0, mbr.as_mut_ptr()) { serial("[sdhci] boot FAT (MBR p1) not exposed: MBR read failed\n"); return None; }
    if mbr[510] != 0x55 || mbr[511] != 0xAA { serial("[sdhci] boot FAT (MBR p1) not exposed: no 55AA signature\n"); return None; }
    let e = &mbr[446..462];
    if e[4] != 0x0b && e[4] != 0x0c {
        serial(&format!("[sdhci] boot FAT (MBR p1) not exposed: type 0x{:02x}\n", e[4]));
        return None;
    }
    let lba = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
    let len = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
    let spb = SECTORS_PER_BLOCK as u64;
    if lba % spb != 0 || len < spb {
        serial(&format!("[sdhci] boot FAT (MBR p1) not exposed: unaligned lba {} len {}\n", lba, len));
        return None;
    }
    Some((lba / spb, len / spb))
}

unsafe fn probe_card(base: usize) -> Option<SdhciDevice> {
    let uhs = UHS_ON.load(Ordering::Relaxed) && board::available();
    if !uhs {
        crate::kenv::pci::serial_debug(if !UHS_ON.load(Ordering::Relaxed) {
            "[sdhci] UHS not used: disabled (sdhci.uhs=0)\n"
        } else {
            "[sdhci] UHS not used: SD I/O voltage / power GPIOs unavailable on this board\n"
        });
    }
    if let Some(d) = probe_card_inner(base, uhs, uhs) { return Some(d); }
    if uhs {
        crate::kenv::pci::serial_debug("[sdhci] UHS bring-up failed: power-cycling the card, retrying at 3.3V HS\n");
        UHS_ACTIVE.store(false, Ordering::Relaxed);
        return probe_card_inner(base, false, true);
    }
    None
}

unsafe fn probe_card_inner(base: usize, uhs: bool, cycle: bool) -> Option<SdhciDevice> {
    let mut dev = SdhciDevice { base, high_capacity: false, partition_offset: 0, limit_blocks: 0, boot_fat: false };
    // A card left at 1.8 V (previous kernel, Linux) cannot be talked to at
    // 3.3 V: power-cycle it first. Also on every UHS attempt (known state).
    if cycle || board::io_is_1v8() {
        if dev.card_power_cycle() {
            crate::kenv::pci::serial_debug("[sdhci] card power-cycled (SD_PWR_ON), I/O 3.3V\n");
        }
    }
    if uhs { dev.log_cfg(); }
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
    let acmd41_arg: u32 = if uhs { 0x40FF_8000 | (1 << 24) } else { 0x40FF_8000 };
    let (hc_bit, s18a) = loop {
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
        let r = dev.send_command(ACMD_SD_SEND_OP_COND, acmd41_arg, RESP_48, false, false, false)?;
        if r[0] & (1 << 31) != 0 {
            break (r[0] & (1 << 30) != 0, r[0] & (1 << 24) != 0);
        }
        // Power-up can legitimately take ~1s of polling, but not forever: a
        // card that never sets the ready bit is a dead card, not a slow one.
        acmd41_tries += 1;
        if acmd41_tries > 100_000 { dbg(b'a'); return None; }
        core::hint::spin_loop();
    };
    dev.high_capacity = hc_bit;
    let mut uhs_live = false;
    if uhs {
        if s18a {
            if !dev.voltage_switch_1v8() { return None; }
            uhs_live = true;
        } else {
            crate::kenv::pci::serial_debug("[sdhci] UHS not used: card did not offer S18A in ACMD41, staying at 3.3V HS\n");
        }
    }

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
        Some(csd) => {
            card_capacity_from_csd(&csd);
            tran_speed_to_khz(((csd[2] >> 24) & 0xFF) as u8)
        }
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
    if uhs_live {
        if !negotiate_uhs(&dev, rca) { return None; }
    } else {
        negotiate_bus(&dev, rca, tran_speed_khz);
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
        Some(d) => {
            #[cfg(feature = "rpi5")]
            let boot = unsafe { find_boot_partition(&d) };
            devs[1] = Some(d);
            // MBR partition 1 as /dev/vdc (read-only; BLKWRWIN windows only).
            // Registered only; nothing reads, mounts or scans it.
            #[cfg(feature = "rpi5")]
            let n = if let (Some((off, len)), Some(d1)) = (boot, devs[1].as_ref()) {
                devs[2] = Some(SdhciDevice {
                    base: d1.base, high_capacity: d1.high_capacity,
                    partition_offset: off, limit_blocks: len, boot_fat: true,
                });
                crate::kenv::pci::serial_debug("[sdhci] boot FAT (MBR p1) exposed as /dev/vdc\n");
                3
            } else { 2 };
            // The whole card as /dev/mmcblk0 (index 3): MBR, every partition,
            // free space. Writes overlapping the mounted root are refused.
            #[cfg(feature = "rpi5")]
            let n = {
                let cb = CARD_BLOCKS.load(Ordering::Relaxed);
                if let (true, Some(d1)) = (cb > 0, devs[1].as_ref()) {
                    devs[WHOLE_IDX] = Some(SdhciDevice {
                        base: d1.base, high_capacity: d1.high_capacity,
                        partition_offset: 0, limit_blocks: cb, boot_fat: false,
                    });
                    crate::kenv::pci::serial_debug("[sdhci] whole card exposed as /dev/mmcblk0\n");
                    WHOLE_IDX + 1
                } else {
                    crate::kenv::pci::serial_debug("[sdhci] whole card NOT exposed: capacity unknown (CSD)\n");
                    n
                }
            };
            #[cfg(not(feature = "rpi5"))]
            let n = 2;
            // ADMA2: allocate the bounce buffer, then prove one read against PIO.
            let dma_state: &str = if !DMA_ON.load(Ordering::Relaxed) { "off (sdhci.dma=0)" }
            else if !MULTI_ON.load(Ordering::Relaxed) { DMA_ON.store(false, Ordering::Relaxed); "off (multi-block off)" }
            else if let Some(ref d) = devs[1] {
                if unsafe { d.r32(REG_CAPABILITIES) } & CAP_ADMA2 == 0 { DMA_ON.store(false, Ordering::Relaxed); "off (no ADMA2 capability)" }
                else if !dma_alloc() { DMA_ON.store(false, Ordering::Relaxed); "off (no DMA memory)" }
                else if unsafe { d.dma_selftest() } { "ADMA2 (32-bit descriptors, bounce below 4 GiB)" }
                else { DMA_ON.store(false, Ordering::Relaxed); "off (selftest failed)" }
            } else { "off" };
            crate::kenv::pci::serial_debug("[sdhci] DMA: ");
            crate::kenv::pci::serial_debug(dma_state);
            crate::kenv::pci::serial_debug(" (sdhci.dma=0 opts out)\n");
            dbg(b']');
            if MULTITEST.load(Ordering::Relaxed) {
                if let Some(ref d) = devs[1] { unsafe { d.multitest(); } }
            }
            crate::kenv::pci::serial_debug(if MULTI_ON.load(Ordering::Relaxed) {
                "[sdhci] I/O mode: multi-block CMD18/CMD25 + auto-CMD12, PIO up to 64 KiB per command (ADMA2 per [sdhci] DMA line; single-sector fallback) (sdhci.multi=1)\n"
            } else {
                "[sdhci] I/O mode: PIO single-sector CMD17/CMD24 (multi-block off; sdhci.multi=1 enables it)\n"
            });
            n
        }
        None => { dbg(b'-'); 0 }
    };
    PROBE_DONE.store(true, core::sync::atomic::Ordering::Relaxed);
}

pub fn device_count() -> usize {
    *DEVICE_COUNT.lock()
}

/// Read one 4096-byte block from device `dev_idx` at logical block `blk`.
pub fn read_block(dev_idx: usize, blk: u64, buf: &mut [u8; BLOCK_SIZE]) -> bool {
    maybe_sdtest();
    let devs = DEVICES.lock();
    if let Some(ref dev) = devs[dev_idx] {
        dev.allowed(false, blk, 1) && dev.do_io_span(false, blk, 1, buf.as_mut_ptr())
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
    span_io(dev_idx, false, blk, buf.as_mut_ptr(), buf.len())
}

/// Write `buf.len() / BLOCK_SIZE` contiguous blocks starting at `blk`.
pub fn write_blocks(dev_idx: usize, blk: u64, buf: &[u8]) -> bool {
    span_io(dev_idx, true, blk, buf.as_ptr() as *mut u8, buf.len())
}

/// Runs the `sdhci.sdtest` once, from the first block request (task context,
/// so the tick window and watchdog keep working), before that request.
fn maybe_sdtest() {
    if SDTEST_PENDING.load(Ordering::Relaxed) && sched::current_pid() != 0 && !sched::irqctx::in_irq()
        && SDTEST_PENDING.swap(false, Ordering::Relaxed) {
        let devs = DEVICES.lock();
        if let Some(Some(d1)) = devs.get(1) {
            let raw = SdhciDevice { base: d1.base, high_capacity: d1.high_capacity,
                partition_offset: 0, limit_blocks: 0, boot_fat: false };
            unsafe { raw.sdtest(SDTEST_MIB.load(Ordering::Relaxed)); }
        }
    }
}

/// Contiguous span I/O, at most `MULTI_MAX_BLOCKS` per command and per
/// `DEVICES` hold. Between chunks (lock dropped) the caller's CPU may be
/// given to another task: the whole span is polled PIO on this CPU, and a
/// multi-megabyte f2fs writeback otherwise keeps every task queued behind
/// it (sshd's keepalive reply, for one) off this CPU until it ends.
fn span_io(dev_idx: usize, is_write: bool, blk: u64, buf: *mut u8, len: usize) -> bool {
    maybe_sdtest();
    if len == 0 || len % BLOCK_SIZE != 0 { return false; }
    let total = len / BLOCK_SIZE;
    {
        let devs = DEVICES.lock();
        match devs.get(dev_idx) {
            Some(Some(d)) if d.allowed(is_write, blk, total as u64) => {}
            _ => return false,
        }
    }
    // After the DEVICES hold above: busy_range takes that lock itself.
    if is_write && whole_write_blocked(dev_idx, blk, total as u64) { return false; }
    if !MULTI_ON.load(Ordering::Relaxed) {
        // Exactly the pre-multi-block behaviour: one 4 KiB block (8 single
        // sectors) per `DEVICES` hold, no reschedule point.
        for i in 0..total {
            let devs = DEVICES.lock();
            let dev = match devs.get(dev_idx) { Some(Some(d)) => d, _ => return false };
            let ok = dev.do_io(is_write, blk + i as u64, unsafe { buf.add(i * BLOCK_SIZE) });
            drop(devs);
            if !ok { return false; }
        }
        return true;
    }
    let mut i = 0;
    while i < total {
        let n = (total - i).min(span_max_blocks());
        let devs = DEVICES.lock();
        let dev = match devs.get(dev_idx) { Some(Some(d)) => d, _ => return false };
        let ok = dev.do_io_span(is_write, blk + i as u64, n, unsafe { buf.add(i * BLOCK_SIZE) });
        drop(devs);
        if !ok { return false; }
        i += n;
        if i < total { sched::kthread::cond_resched(); }
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
/// The boot-FAT device is always read-only (writable only inside a BLKWRWIN window).
pub fn read_only(dev_idx: usize) -> bool {
    let devs = DEVICES.lock();
    matches!(devs.get(dev_idx), Some(Some(d)) if d.boot_fat)
}

pub fn is_boot_fat(dev_idx: usize) -> bool {
    let devs = DEVICES.lock();
    matches!(devs.get(dev_idx), Some(Some(d)) if d.boot_fat)
}

pub fn flush(dev_idx: usize) -> bool {
    let devs = DEVICES.lock();
    devs[dev_idx].is_some()
}

/// Write one 4096-byte block to device `dev_idx` at logical block `blk`.
pub fn write_block(dev_idx: usize, blk: u64, buf: &[u8; BLOCK_SIZE]) -> bool {
    if whole_write_blocked(dev_idx, blk, 1) { return false; }
    let devs = DEVICES.lock();
    if let Some(ref dev) = devs[dev_idx] {
        dev.allowed(true, blk, 1) && dev.do_io_span(true, blk, 1, buf.as_ptr() as *mut u8)
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
    let (boot, lim) = { let d = devs.get(dev_idx)?.as_ref()?; (d.boot_fat, d.limit_blocks) };
    drop(devs);
    Some(crate::BlkDevInfo {
        total_blocks: lim,
        block_size: BLOCK_SIZE as u32,
        // Never fs-scan the boot FAT.
        fstype: if !boot && has_f2fs(dev_idx) { Some("f2fs") } else { None },
    })
}

/// Bitwise CRC-32 (IEEE, reflected); only the boot self-test uses it.
fn crc32_update(mut c: u32, data: &[u8]) -> u32 {
    for &x in data {
        c ^= x as u32;
        for _ in 0..8 { c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 }; }
    }
    c
}
