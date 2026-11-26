//! BCM2712 "dma40" engine: register map, 40-bit control blocks (SCBs), the DREQ cell decode,
//! cyclic ring construction and the channel start/abort sequences. Pure logic, no MMIO and no
//! allocation, so the host tests in `drivers/dma40-tests` run exactly this code.
//!
//! Reference: Linux `drivers/dma/bcm2835-dma.c` (rpi-6.x, the 40-bit variant). The DT node is
//! `dma40: dma@10600 { reg = <0x10 0x10600 0x0 0x600>; brcm,dma-channel-mask = <0x7c0>;
//! interrupts = SPI 0x56..0x5b }`: channel n's registers sit at `0x10_0001_0000 + n * 0x100`
//! (Linux subtracts the start channel from the mapped base), so the node covers channels 6..11
//! and the ARM may use 6..10. The node's `dma-ranges` are the identity, so DMA addresses are
//! CPU physical addresses (DRAM and the 0x10_xxxx_xxxx peripheral window alike).

/// CPU physical address of channel 0 of the engine (channel n = `BLOCK_PHYS + n * CHAN_SIZE`).
pub const BLOCK_PHYS: u64 = 0x10_0001_0000;
pub const CHAN_SIZE: u64 = 0x100;
/// First and last channel the `dma40` node covers.
pub const FIRST_CHAN: u8 = 6;
pub const LAST_CHAN: u8 = 11;
/// `brcm,dma-channel-mask` of the `dma40` node: the channels the ARM may use (6..10).
pub const DT_CHANNEL_MASK: u32 = 0x7c0;
/// Channel the HDMI audio path uses (the pisound-design note).
pub const AUDIO_CHAN: u8 = 10;
/// HDMI0/HDMI1 MAI DREQ cells, C0 stepping (`bcm2712-rpi-5-b.dtb`: `dmas = <&dma40 0x41fa000a>` /
/// `0x41fa0011`): DREQ 10 and 17.
pub const DREQ_CELL_HDMI0: u32 = 0x41fa_000a;
pub const DREQ_CELL_HDMI1: u32 = 0x41fa_0011;
/// The same cells on the D0 stepping (`bcm2712d0-rpi-5-b.dtb`: `0x41fa000c` / `0x41fa000d`):
/// DREQ 12 and 13. Using the C0 numbers on a D0 board leaves the engine unpaced.
pub const DREQ_CELL_HDMI0_D0: u32 = 0x41fa_000c;
pub const DREQ_CELL_HDMI1_D0: u32 = 0x41fa_000d;

/// The MAI DREQ cell for `port` (0 = hdmi0, 1 = hdmi1) on a D0 or C0 SoC.
pub const fn hdmi_dreq_cell(d0: bool, port: usize) -> u32 {
    match (d0, port) {
        (true, 0) => DREQ_CELL_HDMI0_D0,
        (true, _) => DREQ_CELL_HDMI1_D0,
        (false, 0) => DREQ_CELL_HDMI0,
        (false, _) => DREQ_CELL_HDMI1,
    }
}

/// `cell` with its PER_MAP (low 5 bits) replaced by `per_map` (the `hdmi.dreq=N` knob).
pub const fn cell_with_per_map(cell: u32, per_map: u32) -> u32 { (cell & !31) | (per_map & 31) }

// Channel registers.
pub const CS: u32 = 0x00;
pub const CB: u32 = 0x04;
pub const DEBUG: u32 = 0x0c;
pub const TI: u32 = 0x10;
pub const SRC: u32 = 0x14;
pub const SRCI: u32 = 0x18;
pub const DEST: u32 = 0x1c;
pub const DESTI: u32 = 0x20;
pub const LEN: u32 = 0x24;
pub const NEXT_CB: u32 = 0x28;
pub const DEBUG2: u32 = 0x2c;

// CS bits.
pub const CS_ACTIVE: u32 = 1 << 0;
pub const CS_END: u32 = 1 << 1;
pub const CS_INT: u32 = 1 << 2;
pub const CS_DREQ: u32 = 1 << 3;
pub const CS_DREQ_PAUSED: u32 = 1 << 6;
/// "We always want to run in supervisor mode."
pub const CS_PROT: u32 = (1 << 8) | (1 << 9);
pub const CS_ERR: u32 = 1 << 10;
pub const fn cs_qos(x: u32) -> u32 { (x & 0x1f) << 16 }
pub const fn cs_panic_qos(x: u32) -> u32 { (x & 0x1f) << 20 }
pub const CS_TRANSACTIONS: u32 = 1 << 25;
pub const CS_WAIT_FOR_WRITES: u32 = 1 << 28;
pub const CS_DISDEBUG: u32 = 1 << 29;
pub const CS_ABORT: u32 = 1 << 30;
pub const CS_HALT: u32 = 1 << 31;
/// `BCM2711_DMA40_CS_FLAGS(x)`: the bits of a DREQ cell that go straight into CS.
pub const fn cs_flags(cell: u32) -> u32 {
    cell & (cs_qos(15) | cs_panic_qos(15) | CS_WAIT_FOR_WRITES | CS_DISDEBUG)
}
/// `BCM2711_DMA40_MEMCPY_FLAGS`.
pub const CS_MEMCPY_FLAGS: u32 = CS_WAIT_FOR_WRITES | CS_DISDEBUG;

// TI bits.
pub const TI_INTEN: u32 = 1 << 0;
pub const TI_WAIT_RESP: u32 = 1 << 2;
pub const TI_WAIT_RD_RESP: u32 = 1 << 3;
pub const fn ti_per_map(x: u32) -> u32 { (x & 31) << 9 }
pub const TI_S_DREQ: u32 = 1 << 14;
pub const TI_D_DREQ: u32 = 1 << 15;

// SRCI / DESTI bits (the low byte holds address bits 32..39).
pub const fn xi_burst_len(x: u32) -> u32 { (x & 15) << 8 }
pub const XI_INC: u32 = 1 << 12;
pub const XI_SIZE_128: u32 = 2 << 13;
pub const XI_IGNORE: u32 = 1 << 15;
/// `BCM2711_DMA40_MEMCPY_XFER_INFO` (`BURST_LEN(16)` is 0 with the `& 15` macro, kept verbatim).
pub const XI_MEMCPY: u32 = XI_SIZE_128 | XI_INC | xi_burst_len(16);

// DEBUG bits.
pub const DEBUG_WRITE_ERR: u32 = 1 << 0;
pub const DEBUG_FIFO_ERR: u32 = 1 << 1;
pub const DEBUG_READ_ERR: u32 = 1 << 2;
pub const DEBUG_READ_CB_ERR: u32 = 1 << 3;
pub const DEBUG_RESET: u32 = 1 << 23;
pub const DEBUG_ERR_MASK: u32 = DEBUG_WRITE_ERR | DEBUG_FIFO_ERR | DEBUG_READ_ERR | DEBUG_READ_CB_ERR;

/// Largest length one SCB carries (`MAX_DMA40_LEN`).
pub const MAX_CB_LEN: usize = 1 << 30;
/// DMA addresses are 40 bits wide.
pub const ADDR_LIMIT: u64 = 1 << 40;
/// The CB register holds `addr >> 5` in 32 bits, so SCBs must live below 128 GiB.
pub const CB_ADDR_LIMIT: u64 = 1 << 37;

// Legacy bcm2835 DREQ-cell flag bits (the upper bits of a `dmas` cell).
const CELL_WIDE_SOURCE: u32 = 1 << 24;
const CELL_WIDE_DEST: u32 = 1 << 25;
const CELL_NO_WAIT_RESP: u32 = 1 << 27;
const CELL_BURST: u32 = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dma40Error {
    /// Address beyond 40 bits, or an SCB beyond the CB register's reach.
    AddrRange,
    /// SCB address not 32-byte aligned.
    Misaligned,
    /// Zero length, a ring not a whole number of periods, or a period not a multiple of 4.
    BadLength,
    /// More SCBs needed than the caller's table holds.
    TooManyCbs,
    /// Channel outside the DT mask, or already claimed.
    NoChannel,
    Timeout,
    /// The engine flagged an error: `(cs, debug)`.
    Hw(u32, u32),
    /// Copied data did not match.
    Mismatch,
    /// No hardware mapping.
    NoMap,
}

/// `struct bcm2711_dma40_scb`: 32 bytes, 32-byte aligned.
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scb {
    pub ti: u32,
    pub src: u32,
    pub srci: u32,
    pub dst: u32,
    pub dsti: u32,
    pub len: u32,
    pub next_cb: u32,
    pub rsvd: u32,
}

impl Scb {
    pub fn src_addr(&self) -> u64 { ((self.srci as u64 & 0xff) << 32) | self.src as u64 }
    pub fn dst_addr(&self) -> u64 { ((self.dsti as u64 & 0xff) << 32) | self.dst as u64 }
}

/// Value for the CB / NEXT_CB fields: the SCB's address >> 5.
pub fn cb_addr(scb_phys: u64) -> Result<u32, Dma40Error> {
    if scb_phys & 0x1f != 0 { return Err(Dma40Error::Misaligned); }
    if scb_phys == 0 || scb_phys >= CB_ADDR_LIMIT { return Err(Dma40Error::AddrRange); }
    Ok((scb_phys >> 5) as u32)
}

fn split_addr(a: u64) -> Result<(u32, u32), Dma40Error> {
    if a >= ADDR_LIMIT { return Err(Dma40Error::AddrRange); }
    Ok((a as u32, ((a >> 32) & 0xff) as u32))
}

/// Encodes one SCB. `srci_flags`/`dsti_flags` must not use the low (address) byte.
pub fn encode(ti: u32, src: u64, srci_flags: u32, dst: u64, dsti_flags: u32, len: usize, next: u32)
    -> Result<Scb, Dma40Error>
{
    if len == 0 || len > MAX_CB_LEN { return Err(Dma40Error::BadLength); }
    let (s_lo, s_hi) = split_addr(src)?;
    let (d_lo, d_hi) = split_addr(dst)?;
    if src.checked_add(len as u64).map_or(true, |e| e > ADDR_LIMIT) { return Err(Dma40Error::AddrRange); }
    Ok(Scb {
        ti,
        src: s_lo,
        srci: s_hi | (srci_flags & !0xff),
        dst: d_lo,
        dsti: d_hi | (dsti_flags & !0xff),
        len: len as u32,
        next_cb: next,
        rsvd: 0,
    })
}

/// A peripheral DREQ cell (`dmas = <&dma40 cell>`) decoded the way `bcm2835-dma.c` does for a
/// 40-bit channel: low 5 bits = PER_MAP, the rest = CS QOS flags and TI/SRCI/DESTI options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreqCfg {
    pub per_map: u32,
    pub cs_flags: u32,
    /// TI bits shared by every SCB (WAIT_RESP, PER_MAP); the direction adds S_/D_DREQ.
    pub ti: u32,
    /// Width and burst flags (`to_bcm2711_srci`/`dsti` without INC).
    pub src_xi: u32,
    pub dst_xi: u32,
}

impl DreqCfg {
    pub fn from_cell(cell: u32) -> DreqCfg {
        let burst = if cell & CELL_BURST != 0 { xi_burst_len(3) } else { 0 };
        DreqCfg {
            per_map: cell & 31,
            cs_flags: cs_flags(cell),
            ti: (if cell & CELL_NO_WAIT_RESP != 0 { 0 } else { TI_WAIT_RESP }) | ti_per_map(cell & 31),
            src_xi: (if cell & CELL_WIDE_SOURCE != 0 { XI_SIZE_128 } else { 0 }) | burst,
            dst_xi: (if cell & CELL_WIDE_DEST != 0 { XI_SIZE_128 } else { 0 }) | burst,
        }
    }
}

/// Number of SCBs a mem-to-mem copy of `len` bytes needs.
pub fn memcpy_cbs(len: usize) -> usize { len.div_ceil(MAX_CB_LEN) }

/// Builds a linear mem-to-mem chain into `out` (SCB i at `scb_phys + 32 * i`); the last SCB
/// interrupts when `irq`. Returns the number of SCBs used.
pub fn build_memcpy(out: &mut [Scb], scb_phys: u64, dst: u64, src: u64, len: usize, irq: bool)
    -> Result<usize, Dma40Error>
{
    if len == 0 { return Err(Dma40Error::BadLength); }
    let n = memcpy_cbs(len);
    if n > out.len() { return Err(Dma40Error::TooManyCbs); }
    for i in 0..n {
        let off = i * MAX_CB_LEN;
        let this = core::cmp::min(MAX_CB_LEN, len - off);
        let last = i + 1 == n;
        let next = if last { 0 } else { cb_addr(scb_phys + 32 * (i as u64 + 1))? };
        let ti = TI_WAIT_RESP | if last && irq { TI_INTEN } else { 0 };
        out[i] = encode(ti, src + off as u64, XI_MEMCPY, dst + off as u64, XI_MEMCPY, this, next)?;
    }
    cb_addr(scb_phys)?;
    Ok(n)
}

/// A cyclic ring may only target a peripheral FIFO inside the BCM2712 peripheral window (CPU
/// physical, `0x10_7c00_0000..0x10_8000_0000`). Anything else - a 32-bit bus alias, which is
/// ordinary DRAM on a board with 2 GiB or more - would have the engine write over memory. The
/// check lives here so the built-in driver and the module exports (`kapi_dma40_start_cyclic`)
/// share it.
pub const CYCLIC_FIFO_MIN: u64 = 0x10_7c00_0000;
pub const CYCLIC_FIFO_MAX: u64 = 0x10_8000_0000;

/// Builds a cyclic mem-to-device ring: one SCB per period, the last pointing back at the first.
/// `fifo` is the peripheral FIFO's DMA (= CPU physical) address, written with a fixed address
/// and paced by the cell's DREQ. Every period's SCB interrupts when `irq_per_period`.
/// Returns the number of SCBs (= periods).
pub fn build_cyclic_tx(out: &mut [Scb], scb_phys: u64, ring: u64, ring_len: usize, period: usize,
    fifo: u64, dreq: &DreqCfg, irq_per_period: bool) -> Result<usize, Dma40Error>
{
    if period == 0 || period % 4 != 0 || period > MAX_CB_LEN || ring_len == 0 || ring_len % period != 0 {
        return Err(Dma40Error::BadLength);
    }
    let n = ring_len / period;
    if n > out.len() { return Err(Dma40Error::TooManyCbs); }
    if fifo & 3 != 0 { return Err(Dma40Error::Misaligned); }
    if !(CYCLIC_FIFO_MIN..CYCLIC_FIFO_MAX).contains(&fifo) { return Err(Dma40Error::AddrRange); }
    let first = cb_addr(scb_phys)?;
    let ti = dreq.ti | TI_D_DREQ | if irq_per_period { TI_INTEN } else { 0 };
    for i in 0..n {
        let next = if i + 1 == n { first } else { cb_addr(scb_phys + 32 * (i as u64 + 1))? };
        out[i] = encode(ti, ring + (i * period) as u64, dreq.src_xi | XI_INC, fifo, dreq.dst_xi, period, next)?;
    }
    Ok(n)
}

/// Play position: the channel's current source address as a byte offset into the ring, or
/// `None` while the engine is not inside it (idle, or still fetching the first SCB).
pub fn ring_offset(src_lo: u32, srci: u32, ring: u64, ring_len: usize) -> Option<usize> {
    let a = ((srci as u64 & 0xff) << 32) | src_lo as u64;
    if a >= ring && a < ring + ring_len as u64 { Some((a - ring) as usize) } else if a == ring + ring_len as u64 { Some(0) } else { None }
}

/// Bytes the engine consumed moving from offset `prev` to `now` in a ring of `ring_len`.
pub fn ring_advance(prev: usize, now: usize, ring_len: usize) -> usize {
    if now >= prev { now - prev } else { ring_len - prev + now }
}

/// CS value that starts a channel with the given CS flags.
pub const fn cs_start(flags: u32) -> u32 { CS_ACTIVE | CS_PROT | flags }
/// CS value the interrupt handler writes: clears INT, keeps a cyclic channel running.
pub const fn cs_ack(flags: u32) -> u32 { CS_INT | CS_ACTIVE | CS_PROT | flags }

/// Interrupt ID of a dma40 channel: SPI 0x50 + n (dma32 holds 0..5), INTID = SPI + 32.
pub fn intid(ch: u8) -> Option<u32> {
    if (FIRST_CHAN..=LAST_CHAN).contains(&ch) { Some(32 + 0x50 + ch as u32) } else { None }
}

/// Offset of channel `ch` from the start of the dma40 node's window (`BLOCK_PHYS + 6 * 0x100`).
pub fn chan_window_off(ch: u8) -> Option<usize> {
    if (FIRST_CHAN..=LAST_CHAN).contains(&ch) { Some((ch - FIRST_CHAN) as usize * CHAN_SIZE as usize) } else { None }
}

/// Picks a channel from `mask` not in `claimed`: `prefer` if it is free, else the highest free.
pub fn pick_channel(mask: u32, claimed: u32, prefer: Option<u8>) -> Option<u8> {
    let free = mask & !claimed & (((1u32 << (LAST_CHAN + 1)) - 1) & !((1u32 << FIRST_CHAN) - 1));
    if let Some(p) = prefer {
        if p < 32 && free & (1 << p) != 0 { return Some(p); }
    }
    if free == 0 { None } else { Some(31 - free.leading_zeros() as u8) }
}

/// Register access for one channel (offsets are the constants above).
pub trait ChanRegs {
    fn r(&self, off: u32) -> u32;
    fn w(&self, off: u32, v: u32);
    fn delay_us(&self, us: u32);
}

/// True when the channel has no control block loaded (Linux: "ACTIVE is not a reliable
/// indicator; a zero CB address means idle").
pub fn idle<R: ChanRegs + ?Sized>(r: &R) -> bool { r.r(CB) == 0 }

/// Linux `bcm2835_dma_abort` for a 40-bit channel: pause, wait for outstanding AXI
/// transactions (bounded), CS back to PROT, DEBUG.RESET. Returns false if the pause timed out
/// (a stuck peripheral); the reset is done either way.
pub fn abort<R: ChanRegs + ?Sized>(r: &R) -> bool {
    if idle(r) { return true; }
    r.w(CS, r.r(CS) & !CS_ACTIVE);
    let mut t = 100;
    while r.r(CS) & CS_TRANSACTIONS != 0 && t > 0 {
        r.delay_us(1);
        t -= 1;
    }
    r.w(CS, CS_PROT);
    r.w(DEBUG, r.r(DEBUG) | DEBUG_RESET);
    t > 0
}

/// Loads the first SCB and starts the channel (`bcm2835_dma_start_desc`).
pub fn start<R: ChanRegs + ?Sized>(r: &R, scb_phys: u64, cs_flags: u32) -> Result<(), Dma40Error> {
    let cb = cb_addr(scb_phys)?;
    r.w(CB, cb);
    r.w(CS, cs_start(cs_flags));
    Ok(())
}

/// Waits (bounded) for a one-shot chain to finish: CB back to 0 or END set. Errors carry CS and
/// DEBUG. `irq_done` lets an interrupt-driven caller finish early.
pub fn wait_done<R: ChanRegs + ?Sized>(r: &R, timeout_us: u32, irq_done: &dyn Fn() -> bool) -> Result<(), Dma40Error> {
    let mut waited = 0u32;
    loop {
        let cs = r.r(CS);
        let dbg = r.r(DEBUG);
        if cs & CS_ERR != 0 || dbg & DEBUG_ERR_MASK != 0 { return Err(Dma40Error::Hw(cs, dbg)); }
        if irq_done() || cs & CS_END != 0 || r.r(CB) == 0 { return Ok(()); }
        if waited >= timeout_us { return Err(Dma40Error::Timeout); }
        r.delay_us(10);
        waited += 10;
    }
}

/// Clears END/INT after a one-shot chain (write-1-to-clear), leaving the channel idle.
pub fn ack_done<R: ChanRegs + ?Sized>(r: &R) { r.w(CS, CS_END | CS_INT | CS_PROT); }

/// CRC-32 (IEEE 802.3, reflected), for the self-test pattern check.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 { c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 }; }
    }
    !c
}

/// Self-test pattern: a 32-bit LCG keyed by the word index, so shifted or dropped words show.
pub fn pattern_word(i: usize) -> u32 {
    (i as u32).wrapping_mul(0x9e37_79b9).wrapping_add(0x7f4a_7c15) ^ 0xa5a5_0000
}
