//! VideoCore mailbox property interface (channel 8) — the only way to get a
//! framebuffer on a Raspberry Pi 5.
//!
//! ## Why this exists
//!
//! The Pi 5 firmware publishes **no `simple-framebuffer` DTB node**. The node
//! it does publish is named `fb`, `compatible = "brcm,bcm2708-fb"`, and carries
//! nothing but a `firmware` phandle — no `reg`, no `width`, no `height`, no
//! `stride`. `boot/src/device_tree.rs` matches only `framebuffer@`/`framebuffer`
//! and therefore finds nothing, which is why `BOOT_INFO.framebuffer_base` is 0
//! on that board and the console is serial-only. Setting
//! `framebuffer_width`/`_height`/`_depth` and `hdmi_force_hotplug` in config.txt
//! does not change that — tested on hardware, still `Physical base: 0x0`.
//!
//! Linux gets the surface by asking the VideoCore firmware over the mailbox.
//! So do we.
//!
//! ## Base address provenance
//!
//! Decompiled with `dtc` from the vendored
//! `target/rpi5-uefi/bcm2712-rpi-5-b.dtb`, which was verified byte-for-byte
//! identical (via `dtc` + `diff` of the round-tripped DTS) to the copy the
//! firmware actually loads from the SD card's boot partition:
//!
//! ```text
//! soc {
//!     #address-cells = <1>;  #size-cells = <1>;
//!     ranges     = <0x7c000000  0x10 0x7c000000  0x4000000>;
//!     dma-ranges = <0xc0000000  0x00 0x00       0x40000000>,
//!                  <0x7c000000  0x10 0x7c000000  0x4000000>;
//!
//!     mailbox: mailbox@7c013880 {
//!         compatible = "brcm,bcm2835-mbox";
//!         reg = <0x7c013880 0x40>;
//!     };
//!     firmware { compatible = "raspberrypi,bcm2835-firmware"; mboxes = <&mailbox>; };
//!     fb       { compatible = "brcm,bcm2708-fb"; firmware = <&firmware>; status = "okay"; };
//! }
//! ```
//!
//! `soc/ranges` translates a soc-local address `0x7cXXXXXX` to CPU physical
//! `0x10_7cXXXXXX`, so `mailbox@7c013880` lives at **`0x10_7C01_3880`**. That
//! is the same translation that yields the two BCM2712 addresses this kernel
//! already boots on: `arch_aarch64::uart::BASE = 0x10_7D00_1000` from
//! `serial@7d001000`, and `sdhci::SDHCI_BASE = 0x10_00FF_F000` from
//! `mmc@fff000`. It is emphatically *not* the BCM2835 constant `0x7E00B880`
//! nor the BCM2711 one — those boards put the peripheral block somewhere else.
//!
//! No page-table work is needed to reach it: `kernel/src/entry_aarch64.s`
//! installs Device block descriptors for PUD entries 64 and 65, covering
//! `0x10_0000_0000..0x10_8000_0000`, in both the identity (L0[0]) and HHDM
//! (L0[256]) halves. `0x10_7C01_3880` is inside entry 65.
//!
//! ## Bus addresses
//!
//! `soc/dma-ranges`'s first entry is the answer to whether the legacy alias
//! survived the move to a 16 GB board: it declares VideoCore bus `0xC000_0000`
//! → ARM physical `0x0`, for `0x4000_0000` bytes. So:
//!
//! * ARM physical → bus: `bus = phys + 0xC000_0000`, **valid only below 1 GiB**
//! * bus → ARM physical: `phys = bus & 0x3FFF_FFFF`
//!
//! The 1 GiB ceiling binds the *property message buffer* we hand the firmware
//! (see [`MboxError::BufOutOfRange`]). It does **not** bind the framebuffer the
//! firmware hands back — that is just memory the CPU writes and the display
//! engine scans out, and no bus address is involved after the reply.
//!
//! ## QEMU `-M raspi4b`
//!
//! Compiled for `raspi4b` as well, where QEMU models a BCM2835-style property
//! mailbox (`hw/misc/bcm2835_property.c`) plus `bcm2835_fb`. That is not a
//! hardware target and its addresses are not BCM2712's, but it exercises every
//! line of protocol below — message encoding, handshake polarity, tag round-back,
//! clamping, and the no-display bail-out — for free, which is the entire reason
//! the `raspi4b` cfg arms exist. Only the two constants and the bus-alias
//! behaviour differ, and both are called out where they appear.

extern "C" {
    /// `kernel/src/main.rs`. Writes straight to the PL011, bypassing both the
    /// framebuffer console and the VT layer — neither of which exists yet when
    /// this driver runs. `arch_aarch64::uart`'s register helpers add the HHDM
    /// offset only once `mm` has one, so this is equally correct before and
    /// after `mm::init_with_map`.
    fn serial_write_byte_direct(b: u8);
    /// `arch/aarch64/src/lib.rs`. Clean **and invalidate** to the Point of
    /// Coherency. `arch_flush_cache_range` is `dc cvau` (Point of Unification)
    /// and is not a substitute — see that function's doc comment.
    fn arch_dcache_clean_inval_range(addr: usize, len: usize);
}

// ── Board constants ───────────────────────────────────────────────────────────

/// ARM-side mailbox MMIO base. See the module doc for the DTB derivation.
#[cfg(feature = "rpi5")]
pub const MBOX_BASE_PHYS: usize = 0x10_7C01_3880;
/// BCM2711 peripheral base 0xFE000000 + the BCM2835 mailbox offset 0xB880.
/// QEMU-only; see the module doc.
#[cfg(all(feature = "raspi4b", not(feature = "rpi5")))]
pub const MBOX_BASE_PHYS: usize = 0xFE00_B880;

// Register offsets within the 0x40-byte window. Two 0x20-byte mailboxes:
// MAIL0 is VideoCore→ARM, MAIL1 is ARM→VideoCore.
const MBOX_READ: usize = 0x00; // MAIL0 read
const MBOX_STATUS0: usize = 0x18; // MAIL0 status
const MBOX_WRITE: usize = 0x20; // MAIL1 write
const MBOX_STATUS1: usize = 0x38; // MAIL1 status

const MBOX_FULL: u32 = 1 << 31;
const MBOX_EMPTY: u32 = 1 << 30;
const CHAN_PROPERTY: u32 = 8;

/// `soc/dma-ranges`: bus 0xC000_0000 ↔ ARM physical 0, one gigabyte.
const BUS_ALIAS: u64 = 0xC000_0000;
const BUS_MASK: u64 = 0x3FFF_FFFF;
/// Hard ceiling on anything we hand the firmware as a bus address.
const BUS_LIMIT: u64 = 0x4000_0000;

// ── Property tags ─────────────────────────────────────────────────────────────

const TAG_GET_FIRMWARE_REV: u32 = 0x0000_0001;
const TAG_GET_BOARD_REV: u32 = 0x0001_0002;
const TAG_GET_ARM_MEMORY: u32 = 0x0001_0005;
const TAG_GET_VC_MEMORY: u32 = 0x0001_0006;
const TAG_GET_PHYSICAL_WH: u32 = 0x0004_0003;
const TAG_ALLOCATE_BUFFER: u32 = 0x0004_0001;
const TAG_GET_PITCH: u32 = 0x0004_0008;
const TAG_RELEASE_BUFFER: u32 = 0x0004_8001;
const TAG_SET_PHYSICAL_WH: u32 = 0x0004_8003;
const TAG_SET_VIRTUAL_WH: u32 = 0x0004_8004;
const TAG_SET_DEPTH: u32 = 0x0004_8005;
const TAG_SET_PIXEL_ORDER: u32 = 0x0004_8006;
const TAG_SET_VIRTUAL_OFFSET: u32 = 0x0004_8009;

const REQ_CODE: u32 = 0x0000_0000;
const RESP_SUCCESS: u32 = 0x8000_0000;

/// Pixel byte order handed to `SET_PIXEL_ORDER`.
///
/// 0 = BGR, 1 = RGB. `drivers::framebuffer` composes colours as `0x00RRGGBB`
/// in a `u32`; a little-endian store lays that out in memory as the bytes
/// `B, G, R, X`, which is what the firmware calls BGR. This is the one value in
/// the whole design that serial output cannot settle, which is why
/// `kernel/src/main.rs` paints red/green/blue reference bars and holds them —
/// if the leftmost bar photographs blue, flip this to 1.
const PIXEL_ORDER_BGR: u32 = 0;

/// Alignment requested from `ALLOCATE_BUFFER`. A page, so the framebuffer's
/// 4 KiB mapping in `arch_aarch64::init` starts exactly on the base and
/// `base & 4095` (which `kernel_main` adds to the virtual base) is zero.
const FB_ALIGN: u32 = 4096;

// ── Property buffer ───────────────────────────────────────────────────────────

/// Words of scratch. 64 × 4 = 256 B = four 64-byte cache lines; the longest
/// message below is 35 words.
const BUF_WORDS: usize = 64;

/// The property message buffer, on the `rpi5` path.
///
/// A `static mut` in `.bss` rather than an allocation, because this driver runs
/// **before `mm::init_with_map`** — there is no buddy allocator, no heap, and
/// no `ATTR_NORMAL_NC` MAIR attribute yet (`mmu::enable_identity` installs
/// index 2 later, inside `arch_aarch64::init`). What there *is* is the kernel
/// image, which the `rpi5` link places at physical 0x20_0000 and which
/// `kernel_main` has already handed to `mm::buddy::reserve_range`: physically
/// contiguous, comfortably under the 1 GiB bus-alias ceiling, and zeroed by the
/// entry stub.
///
/// It is mapped Normal Write-Back by the 1 GiB block descriptor covering
/// 0..1 GiB, so coherency with the VideoCore — which reads it through the
/// uncached bus alias, around our caches — is bought with explicit
/// `dc civac` maintenance instead of a page attribute. `align(64)` is what
/// makes that maintenance safe: a cache-line-aligned, cache-line-multiple
/// buffer means the clean/invalidate can never write back or discard a
/// neighbouring static.
#[cfg(feature = "rpi5")]
#[repr(C, align(64))]
struct MboxBuf([u32; BUF_WORDS]);
#[cfg(feature = "rpi5")]
static mut MBOX_BUF: MboxBuf = MboxBuf([0; BUF_WORDS]);

/// Physical address of the property buffer on the `raspi4b` path.
///
/// That build links the kernel at physical `0x4008_0000` (see
/// `linkers/aarch64-direct.ld`; only the `--rpi5` variant rewrites
/// `KERNEL_PHYS` to 0x20_0000), which is **above** the 1 GiB bus alias — a
/// `static mut` in the image would be unreachable by the VideoCore. So the
/// QEMU path uses a fixed low scratch page instead. `kernel_main` reserves it;
/// see [`scratch_reservation`].
#[cfg(all(feature = "raspi4b", not(feature = "rpi5")))]
pub const MBOX_BUF_PHYS: usize = 0x0010_0000;

/// The physical page `kernel_main` must keep out of the buddy allocator, if any.
///
/// `None` on `rpi5` — the buffer lives inside the kernel image, which is
/// already reserved.
pub const fn scratch_reservation() -> Option<(usize, usize)> {
    #[cfg(feature = "rpi5")]
    {
        None
    }
    #[cfg(all(feature = "raspi4b", not(feature = "rpi5")))]
    {
        Some((MBOX_BUF_PHYS, MBOX_BUF_PHYS + 4096))
    }
}

/// `(virtual, physical)` addresses of the property buffer.
///
/// `hhdm_offset` is passed in rather than read from `mm` because this runs
/// before `mm::set_hhdm_offset`. On the direct AArch64 path it is
/// 0xffff_8000_0000_0000, which is also `KERNEL_VIRT` in the linker script —
/// so subtracting it from an in-image address recovers the load address.
#[inline]
unsafe fn buf_addrs(hhdm_offset: usize) -> (usize, usize) {
    #[cfg(feature = "rpi5")]
    {
        let v = core::ptr::addr_of!(MBOX_BUF) as usize;
        (v, v - hhdm_offset)
    }
    #[cfg(all(feature = "raspi4b", not(feature = "rpi5")))]
    {
        (MBOX_BUF_PHYS + hhdm_offset, MBOX_BUF_PHYS)
    }
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MboxError {
    /// The mailbox never accepted the doorbell, or never replied.
    Timeout,
    /// The firmware wrote back something other than 0x8000_0000 in word 1.
    BadCode(u32),
    /// A tag came back with its success bit clear, or with an unusable value.
    TagFailed(u32),
    /// The property buffer is not reachable through the 1 GiB bus alias.
    BufOutOfRange(u64),
    /// No display attached, or the firmware declined to allocate.
    NoDisplay,
    /// The message did not fit in [`BUF_WORDS`].
    TooLong,
}

// ── Result ────────────────────────────────────────────────────────────────────

/// A framebuffer the firmware allocated for us.
#[derive(Clone, Copy, Debug)]
pub struct FbAlloc {
    /// ARM physical base address.
    pub phys: u64,
    /// Raw VideoCore bus address, retained so the boot log can show both.
    pub bus: u64,
    /// Bytes the firmware actually allocated — *not* what we asked for.
    pub size: u64,
    /// Visible width, as accepted by the firmware.
    pub width: u32,
    /// Visible height, as accepted by the firmware.
    pub height: u32,
    /// Buffer height. `>= height`. When it is a multiple of `height` the buffer
    /// is tall enough to pan with `SET_VIRTUAL_OFFSET`, which is how the next
    /// lane replaces the console's `scroll_px` full-surface copy. Query this,
    /// do not assume it: the firmware is free to clamp it back to `height`.
    pub virt_height: u32,
    /// Bytes per row, from `GET_PITCH`. Never `width * 4` by assumption.
    pub pitch: u32,
    /// Bits per pixel. Only 32 is accepted.
    pub depth: u32,
}

impl FbAlloc {
    /// Bytes that must be kept out of the buddy allocator and swept from the
    /// caches: the **whole** buffer, off-screen panning rows included. Reserving
    /// only `pitch * height` would hand the allocator the off-screen half.
    pub fn reserve_len(&self) -> u64 {
        let geom = self.pitch as u64 * self.virt_height as u64;
        if self.size > geom {
            self.size
        } else {
            geom
        }
    }
}

// ── Logging ───────────────────────────────────────────────────────────────────

fn log(s: &str) {
    for &b in s.as_bytes() {
        unsafe { serial_write_byte_direct(b) };
    }
}

fn log_hex(v: u64, digits: usize) {
    const D: &[u8; 16] = b"0123456789ABCDEF";
    log("0x");
    for i in (0..digits).rev() {
        unsafe { serial_write_byte_direct(D[((v >> (i * 4)) & 0xF) as usize]) };
    }
}

fn log_h32(v: u32) {
    log_hex(v as u64, 8);
}

fn log_h64(v: u64) {
    log_hex(v, 16);
}

fn log_dec(mut v: u32) {
    if v == 0 {
        log("0");
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = 0;
    while v > 0 {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        unsafe { serial_write_byte_direct(buf[i]) };
    }
}

/// Hex-dump the first 16 reply words, four per line.
///
/// Sixteen lines of hex is the cheapest insurance this driver carries: the
/// Raspberry Pi 5 boots off an SD card that has to be physically moved between
/// machines, so a second boot is expensive. Any tag-encoding mistake — a wrong
/// value-buffer length, a missing end tag, an off-by-one word index — is
/// diagnosable offline from this dump without spending one.
fn log_dump(words: &[u32]) {
    let n = if words.len() < 16 { words.len() } else { 16 };
    let mut i = 0;
    while i < n {
        log("[MBOX] dump ");
        log_dec(i as u32);
        log(":");
        let mut j = 0;
        while j < 4 && i + j < n {
            log(" ");
            log_h32(words[i + j]);
            j += 1;
        }
        log("\n");
        i += 4;
    }
}

// ── MMIO + timing ─────────────────────────────────────────────────────────────

#[inline(always)]
unsafe fn rd32(base: usize, off: usize) -> u32 {
    ((base + off) as *const u32).read_volatile()
}

#[inline(always)]
unsafe fn wr32(base: usize, off: usize, val: u32) {
    ((base + off) as *mut u32).write_volatile(val)
}

#[inline(always)]
unsafe fn cntvct() -> u64 {
    let v: u64;
    core::arch::asm!("mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack));
    v
}

/// Ticks in roughly two seconds, derived from CNTFRQ_EL0 so it is wall-clock
/// regardless of the counter's rate, with a fallback if firmware left CNTFRQ
/// zero.
///
/// Generous on purpose: `ALLOCATE_BUFFER` can drive an HDMI mode set. Bounded
/// on purpose too — an unbounded wait on a mailbox that is not there is exactly
/// how a single expensive hardware boot gets spent learning nothing.
#[inline]
unsafe fn timeout_ticks() -> u64 {
    let f: u64;
    core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack));
    if f == 0 {
        200_000_000
    } else {
        f * 2
    }
}

/// Spin until `f()` is false, or the deadline passes. `true` = condition cleared.
#[inline]
unsafe fn spin_until(mut still_blocked: impl FnMut() -> bool) -> bool {
    let deadline = cntvct().wrapping_add(timeout_ticks());
    while still_blocked() {
        // Wrapping compare, same idiom as `uart::putc`'s TX deadline.
        if cntvct().wrapping_sub(deadline) < (1u64 << 63) {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

// ── The protocol ──────────────────────────────────────────────────────────────

/// Send one complete property message and wait for the reply.
///
/// `words` must already be a whole message — total size in bytes at `[0]`,
/// request code at `[1]`, tags, then a zero end tag. On success the returned
/// slice is the firmware's reply, read back out of the same buffer.
///
/// # Safety
/// Must be called before `mm::init_with_map`, hence the explicit
/// `hhdm_offset`; and only from the boot CPU with nothing else touching the
/// mailbox.
pub unsafe fn property_call(
    hhdm_offset: usize,
    words: &[u32],
) -> Result<&'static [u32], MboxError> {
    if words.len() > BUF_WORDS {
        return Err(MboxError::TooLong);
    }

    let (buf_virt, buf_phys) = buf_addrs(hhdm_offset);
    let bus = buf_phys as u64 + BUS_ALIAS;

    // The firmware reads this buffer through the 0xC000_0000 alias, which only
    // covers the low gigabyte. Fail loudly rather than letting the VideoCore
    // DMA-read whatever a truncated 32-bit bus address happens to land on.
    if buf_phys as u64 + (BUF_WORDS * 4) as u64 > BUS_LIMIT {
        return Err(MboxError::BufOutOfRange(buf_phys as u64));
    }
    // 16-byte alignment is structural: the low four bits of the doorbell word
    // carry the channel number.
    if buf_phys & 0xF != 0 {
        return Err(MboxError::BufOutOfRange(buf_phys as u64));
    }

    let dst = buf_virt as *mut u32;
    for (i, w) in words.iter().enumerate() {
        dst.add(i).write_volatile(*w);
    }

    let mbox = MBOX_BASE_PHYS + hhdm_offset;

    // Push the request out of our caches so the VideoCore, reading around them,
    // sees it. `dsb sy` then orders that against the doorbell store below.
    arch_dcache_clean_inval_range(buf_virt, BUF_WORDS * 4);
    core::arch::asm!("dsb sy", options(nostack, preserves_flags));

    if !spin_until(|| unsafe { rd32(mbox, MBOX_STATUS1) } & MBOX_FULL != 0) {
        log("[MBOX] TIMEOUT (send) st0=");
        log_h32(rd32(mbox, MBOX_STATUS0));
        log(" st1=");
        log_h32(rd32(mbox, MBOX_STATUS1));
        log("\n");
        return Err(MboxError::Timeout);
    }
    wr32(mbox, MBOX_WRITE, (bus as u32 & !0xF) | CHAN_PROPERTY);

    // Drain until our channel comes back; anything else on MAIL0 belongs to a
    // channel we do not use and is discarded.
    //
    // ONE deadline for the whole drain, deliberately not a fresh `spin_until`
    // per iteration. An earlier version re-armed the timeout each time round
    // this loop, which is not a timeout at all: a mailbox window that reads
    // back as all-zeroes — an unimplemented MMIO stub, or a base address off by
    // a peripheral — reports "not empty" forever and yields channel 0 forever,
    // so every iteration saw a fresh two seconds and the boot hung with no
    // output. Found while trying to *test* the timeout path, which is the only
    // reason it was found at all.
    let deadline = cntvct().wrapping_add(timeout_ticks());
    loop {
        if cntvct().wrapping_sub(deadline) < (1u64 << 63) {
            log("[MBOX] TIMEOUT (recv) st0=");
            log_h32(rd32(mbox, MBOX_STATUS0));
            log(" st1=");
            log_h32(rd32(mbox, MBOX_STATUS1));
            log("\n");
            return Err(MboxError::Timeout);
        }
        if rd32(mbox, MBOX_STATUS0) & MBOX_EMPTY != 0 {
            core::hint::spin_loop();
            continue;
        }
        if rd32(mbox, MBOX_READ) & 0xF == CHAN_PROPERTY {
            break;
        }
    }

    // Discard any line the CPU speculatively pulled in while waiting, so the
    // reply we read is the firmware's and not a stale copy. Nothing wrote the
    // buffer between the doorbell and here, so the "clean" half of civac has
    // nothing to write back.
    core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    arch_dcache_clean_inval_range(buf_virt, BUF_WORDS * 4);

    let reply = core::slice::from_raw_parts(buf_virt as *const u32, BUF_WORDS);
    log_dump(reply);

    if reply[1] != RESP_SUCCESS {
        return Err(MboxError::BadCode(reply[1]));
    }
    Ok(reply)
}

/// Smoke test and display probe, in one round trip.
///
/// Returns the firmware's *current* physical display size; `(0, 0)` means no
/// sink. The three memory/revision tags are not needed by anything downstream —
/// they are here because a plausible firmware revision proves the mailbox base
/// address **and** the outbound `phys + 0xC000_0000` translation in one shot,
/// independently of any question about whether the display half still works.
/// On the one hardware boot this is the line that separates "the mailbox is not
/// where we think it is" from "the firmware declined to allocate".
///
/// The VideoCore memory window is returned through `vc_out` for
/// [`bus_to_phys`]'s cross-check.
pub unsafe fn probe(hhdm_offset: usize, vc_out: &mut (u64, u64)) -> Result<(u32, u32), MboxError> {
    let (buf_virt, buf_phys) = buf_addrs(hhdm_offset);
    log("[MBOX] base=");
    log_h64(MBOX_BASE_PHYS as u64);
    log(" bufvirt=");
    log_h64(buf_virt as u64);
    log(" bufphys=");
    log_h64(buf_phys as u64);
    log(" bus=");
    log_h32(buf_phys as u32 + BUS_ALIAS as u32);
    log("\n");

    #[rustfmt::skip]
    let msg: [u32; 26] = [
        26 * 4, REQ_CODE,
        TAG_GET_FIRMWARE_REV, 4, REQ_CODE, 0,
        TAG_GET_BOARD_REV,    4, REQ_CODE, 0,
        TAG_GET_ARM_MEMORY,   8, REQ_CODE, 0, 0,
        TAG_GET_VC_MEMORY,    8, REQ_CODE, 0, 0,
        TAG_GET_PHYSICAL_WH,  8, REQ_CODE, 0, 0,
        0,
    ];
    let r = property_call(hhdm_offset, &msg)?;

    let (vc_base, vc_size) = (r[18] as u64, r[19] as u64);
    *vc_out = (vc_base, vc_size);
    let (dw, dh) = (r[23], r[24]);

    log("[MBOX] smoke code=");
    log_h32(r[1]);
    log(" fwrev=");
    log_h32(r[5]);
    log(" board=");
    log_h32(r[9]);
    log(" arm=");
    log_h32(r[13]);
    log("+");
    log_h32(r[14]);
    log(" vc=");
    log_h32(vc_base as u32);
    log("+");
    log_h32(vc_size as u32);
    log(" disp=");
    log_dec(dw);
    log("x");
    log_dec(dh);
    log("\n");

    Ok((dw, dh))
}

/// Convert the bus address `ALLOCATE_BUFFER` returned into an ARM physical one.
///
/// The DTB says mask (see the module doc), and on real BCM2712 that is what the
/// firmware returns. QEMU's `bcm2835_property` model returns a plain physical
/// address instead, un-aliased — so the masked candidate would land 0xC000_0000
/// below the real buffer and quietly scribble on someone else's RAM. The
/// VideoCore memory window from `GET_VC_MEMORY` discriminates the two without
/// guessing: the framebuffer is by construction inside it.
fn bus_to_phys(bus: u64, vc: (u64, u64)) -> (u64, bool, bool) {
    let (vc_base, vc_size) = vc;
    let masked = bus & BUS_MASK;
    let in_vc = |a: u64| vc_size != 0 && a >= vc_base && a < vc_base + vc_size;
    if !in_vc(masked) && in_vc(bus) {
        (bus, false, true)
    } else {
        (masked, true, in_vc(masked))
    }
}

/// One `ALLOCATE_BUFFER` attempt at a given geometry.
///
/// `virt_h` is the buffer height. Everything the firmware writes back is read
/// out of the reply; nothing that was requested is carried forward.
unsafe fn attempt(
    hhdm_offset: usize,
    w: u32,
    h: u32,
    virt_h: u32,
    vc: (u64, u64),
) -> Result<FbAlloc, MboxError> {
    log("[MBOX] req ");
    log_dec(w);
    log("x");
    log_dec(h);
    log(" virt ");
    log_dec(w);
    log("x");
    log_dec(virt_h);
    log(" depth=32 order=");
    log_dec(PIXEL_ORDER_BGR);
    log("\n");

    #[rustfmt::skip]
    let msg: [u32; 35] = [
        35 * 4, REQ_CODE,
        TAG_SET_PHYSICAL_WH,    8, REQ_CODE, w, h,
        TAG_SET_VIRTUAL_WH,     8, REQ_CODE, w, virt_h,
        TAG_SET_VIRTUAL_OFFSET, 8, REQ_CODE, 0, 0,
        TAG_SET_DEPTH,          4, REQ_CODE, 32,
        TAG_SET_PIXEL_ORDER,    4, REQ_CODE, PIXEL_ORDER_BGR,
        // Value buffer is 8 bytes: alignment in, (base, size) out.
        TAG_ALLOCATE_BUFFER,    8, REQ_CODE, FB_ALIGN, 0,
        TAG_GET_PITCH,          4, REQ_CODE, 0,
        0,
    ];
    let r = property_call(hhdm_offset, &msg)?;

    let got_w = r[5];
    let got_h = r[6];
    let got_vw = r[10];
    let got_vh = r[11];
    let depth = r[20];
    let order = r[24];
    let bus = r[28] as u64;
    let size = r[29] as u64;
    let pitch = r[33];

    log("[MBOX] fb code=");
    log_h32(r[1]);
    log(" phys=");
    log_dec(got_w);
    log("x");
    log_dec(got_h);
    log(" virt=");
    log_dec(got_vw);
    log("x");
    log_dec(got_vh);
    log(" depth=");
    log_dec(depth);
    log(" order=");
    log_dec(order);
    log(" alloc_bus=");
    log_h32(bus as u32);
    log(" alloc_size=");
    log_h32(size as u32);
    log(" pitch=");
    log_h32(pitch);
    log("\n");

    if got_w == 0 || got_h == 0 || bus == 0 || size == 0 {
        return Err(MboxError::NoDisplay);
    }
    // The console renders 32 bpp only; anything else would be drawn as garbage
    // rather than failing, which is worse than not having a console.
    if depth != 32 {
        return Err(MboxError::TagFailed(TAG_SET_DEPTH));
    }
    if pitch < got_w.saturating_mul(4) {
        return Err(MboxError::TagFailed(TAG_GET_PITCH));
    }

    // Report the firmware's stride in decimal next to the tightly-packed value,
    // because a consumer downstream cares about exactly this comparison: Mesa's
    // v3d driver lays linear resources out with no row alignment at all
    // (`stride = width * cpp`, v3d_resource.c), so a firmware pitch that is not
    // `width * 4` means a direct v3d render target cannot alias this surface and
    // a blit is required. Reported, never "fixed" — GET_PITCH stays
    // authoritative and nothing here ever computes `width * 4` as a stride.
    log("[MBOX] pitch=");
    log_dec(pitch);
    log(" tight=");
    log_dec(got_w.saturating_mul(4));
    log(" v3d_linear_match=");
    log_dec(if pitch == got_w.saturating_mul(4) { 1 } else { 0 });
    log("\n");

    let (phys, was_masked, vc_ok) = bus_to_phys(bus, vc);
    log("[MBOX] fb phys=");
    log_h64(phys);
    log(if was_masked { " (masked)" } else { " (raw)" });
    log(" vc_ok=");
    log_dec(if vc_ok { 1 } else { 0 });
    log("\n");

    // Clamp against what was actually allocated rather than against what was
    // asked for. A console that writes past the end of the buffer takes a data
    // abort into a black screen; one that draws fewer rows just looks short.
    let mut height = got_h;
    let mut virt_height = if got_vh < got_h { got_h } else { got_vh };
    let rows_fit = (size / pitch as u64) as u32;
    if rows_fit < virt_height {
        virt_height = rows_fit;
    }
    if virt_height < height {
        height = virt_height;
    }
    if height == 0 {
        return Err(MboxError::TagFailed(TAG_ALLOCATE_BUFFER));
    }

    Ok(FbAlloc {
        phys,
        bus,
        size,
        width: got_w,
        height,
        virt_height,
        pitch,
        depth,
    })
}

/// Best-effort `RELEASE_BUFFER`, so a refused tall allocation is not leaked
/// before the plain retry.
unsafe fn release(hhdm_offset: usize) {
    let msg: [u32; 6] = [6 * 4, REQ_CODE, TAG_RELEASE_BUFFER, 0, REQ_CODE, 0];
    let _ = property_call(hhdm_offset, &msg);
}

/// Ask the VideoCore firmware for a linear framebuffer.
///
/// `want_w`/`want_h` are used only if the firmware reports no current display
/// size of its own. The returned geometry is entirely the firmware's — see
/// [`FbAlloc`].
///
/// Asks first for a buffer **twice as tall as the visible area**. Nothing here
/// pans it, but a tall buffer is the precondition for replacing the console's
/// full-surface `scroll_px` copy with a `SET_VIRTUAL_OFFSET` pan later, and
/// asking for it costs one word in a message we are already sending. If the
/// firmware refuses outright, this falls back to a plain buffer; if it silently
/// clamps the virtual height, [`FbAlloc::virt_height`] records that. Callers
/// must query it, never assume it.
///
/// Never panics and never blocks indefinitely. Every failure returns `Err`,
/// which leaves `BOOT_INFO.framebuffer_base` at 0 and the boot on serial only —
/// bit for bit the behaviour this board has today.
///
/// # Safety
/// Call once, from the boot CPU, before `mm::init_with_map`.
pub unsafe fn init_framebuffer(
    hhdm_offset: usize,
    want_w: u32,
    want_h: u32,
) -> Result<FbAlloc, MboxError> {
    let mut vc = (0u64, 0u64);
    let (dw, dh) = match probe(hhdm_offset, &mut vc) {
        Ok(v) => v,
        Err(e) => {
            log("[MBOX] probe FAILED\n");
            return Err(e);
        }
    };

    let (w, h) = if dw != 0 && dh != 0 {
        (dw, dh)
    } else {
        // A firmware that reports 0x0 usually means no sink. Try the requested
        // mode anyway — with `hdmi_force_hotplug` the firmware will drive a
        // display it cannot read an EDID from — and let `attempt` decide.
        log("[MBOX] firmware reports no display size; trying requested mode\n");
        (want_w, want_h)
    };

    match attempt(hhdm_offset, w, h, h.saturating_mul(2), vc) {
        Ok(fb) => {
            log("[MBOX] ok (tall request accepted or clamped)\n");
            Ok(fb)
        }
        Err(e) => {
            log("[MBOX] tall request rejected (");
            log_h32(err_code(e));
            log("); retrying with virtual == physical\n");
            release(hhdm_offset);
            match attempt(hhdm_offset, w, h, h, vc) {
                Ok(fb) => {
                    log("[MBOX] ok (plain)\n");
                    Ok(fb)
                }
                Err(e2) => {
                    log("[MBOX] FAILED code=");
                    log_h32(err_code(e2));
                    log(" -- serial-only boot\n");
                    Err(e2)
                }
            }
        }
    }
}

/// Compact numeric form of an error, for the log.
fn err_code(e: MboxError) -> u32 {
    match e {
        MboxError::Timeout => 0xE000_0001,
        MboxError::BadCode(c) => c,
        MboxError::TagFailed(t) => t,
        MboxError::BufOutOfRange(_) => 0xE000_0002,
        MboxError::NoDisplay => 0xE000_0003,
        MboxError::TooLong => 0xE000_0004,
    }
}
