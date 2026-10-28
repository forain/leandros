//! BCM2712 "dma40" DMA engine (Linux `bcm2835-dma` 40-bit channels 6..11), Raspberry Pi 5.
//!
//! [`cb`] is the pure part (register map, 40-bit SCB encoding, DREQ cells, cyclic rings,
//! start/abort sequences), host-tested in `drivers/dma40-tests`. The hardware part is rpi5-only:
//! channel claim from the DT mask (0x7c0 = 6..10), mem-to-mem copies, cyclic mem-to-FIFO rings
//! paced by a peripheral DREQ (HDMI audio: channel 10, DREQ 10/17 from the DTB), per-channel interrupts with
//! a polled fallback, and a boot self-test.
//!
//! Kernel API for a peripheral driver (pihdmi-audio):
//! ```ignore
//! let mut ch = bcm2712_dma40::claim(Some(cb::AUDIO_CHAN))?;
//! ch.start_cyclic(ring_phys, ring_len, period, MAI_DATA_PHYS, cb::DREQ_CELL_HDMI0, true)?;
//! // on the 100 Hz tick: if bcm2712_dma40::polled() { ch.poll(); }
//! let pos = ch.position();          // byte offset the engine reads next (LPIB)
//! let n = ch.periods();             // periods completed (IRQ or poll)
//! ch.stop();                        // or drop(ch): abort, free the SCB page, release
//! ```
//! All addresses are CPU physical: the node's `dma-ranges` are the identity.

pub mod cb;

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::*;

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use super::cb::{self, ChanRegs, Dma40Error, Scb};
    use crate::irq::{IrqReturn, IrqSource};
    use crate::pci::serial_debug;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use leandros_lib::mmio::{MmioRd, MmioWr};
    use mm::kva::{ioremap, CacheMode};
    use mm::{buddy, phys_to_virt};

    pub type SerrorCheck = fn() -> Option<u64>;

    const NCH: usize = (cb::LAST_CHAN - cb::FIRST_CHAN + 1) as usize;
    /// SCBs per channel: one 4 KiB page.
    const SCBS_PER_PAGE: usize = 4096 / core::mem::size_of::<Scb>();

    /// Virtual base of the node window (channel 6 at +0).
    static WIN: AtomicUsize = AtomicUsize::new(0);
    static CLAIMED: AtomicU32 = AtomicU32::new(0);
    /// Channels found busy (CB != 0, i.e. owned by the firmware) at claim time.
    static FW_BUSY: AtomicU32 = AtomicU32::new(0);
    static IRQ_REGISTERED: AtomicU32 = AtomicU32::new(0);
    static CYCLIC: AtomicU32 = AtomicU32::new(0);
    static POLLED: AtomicBool = AtomicBool::new(true);
    static IRQS: [AtomicU64; NCH] = [const { AtomicU64::new(0) }; NCH];
    static CSFLAGS: [AtomicU32; NCH] = [const { AtomicU32::new(0) }; NCH];
    /// Boot self-test result: 0 not run, 1 ok, 2 skipped, 3+ failed.
    pub static SELFTEST_STATE: AtomicU32 = AtomicU32::new(0);

    fn idx(ch: u8) -> usize { (ch - cb::FIRST_CHAN) as usize }

    #[derive(Clone, Copy)]
    struct Mmio { base: usize }
    impl ChanRegs for Mmio {
        fn r(&self, off: u32) -> u32 { unsafe { ((self.base + off as usize) as *const u32).mmio_read() } }
        fn w(&self, off: u32, v: u32) { unsafe { ((self.base + off as usize) as *mut u32).mmio_write(v) } }
        fn delay_us(&self, us: u32) {
            let end = sched::monotonic_ns() + us as u64 * 1000;
            while sched::monotonic_ns() < end { core::hint::spin_loop(); }
        }
    }

    fn regs(ch: u8) -> Option<Mmio> {
        let w = WIN.load(Ordering::Acquire);
        if w == 0 { return None; }
        cb::chan_window_off(ch).map(|o| Mmio { base: w + o })
    }

    /// `dc civac` over a kernel VA range, then `dsb sy`: the engine is not cache coherent.
    pub fn dcache_clean_inval(va: usize, len: usize) {
        let mut p = va & !63;
        while p < va + len {
            unsafe { core::arch::asm!("dc civac, {}", in(reg) p, options(nostack, preserves_flags)); }
            p += 64;
        }
        unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)); }
    }

    /// Maps the engine's register window once. False if the mapping fails.
    pub fn init() -> bool {
        if WIN.load(Ordering::Acquire) != 0 { return true; }
        let phys = cb::BLOCK_PHYS + cb::FIRST_CHAN as u64 * cb::CHAN_SIZE;
        match ioremap(phys, NCH * cb::CHAN_SIZE as usize, CacheMode::Uncached) {
            Some(v) => {
                WIN.store(v, Ordering::Release);
                crate::dma_shutdown::register("dma40", crate::dma_shutdown::Stage::Device, kexec_stop_all);
                true
            }
            None => false,
        }
    }

    /// kexec (dma_shutdown hook, interrupts off): every channel this kernel
    /// claimed is aborted and reset, so no control block chain keeps copying
    /// into memory the next kernel owns. Firmware-owned channels are left alone.
    fn kexec_stop_all() -> bool {
        let claimed = CLAIMED.load(Ordering::Acquire);
        CYCLIC.store(0, Ordering::Release);
        let mut ok = true;
        for ch in cb::FIRST_CHAN..=cb::LAST_CHAN {
            if claimed & (1 << ch) == 0 { continue; }
            if let Some(r) = regs(ch) { ok &= cb::abort(&r); }
        }
        ok
    }

    /// True when channel interrupts were not proven by the self-test: callers drive
    /// [`Channel::poll`] from a tick instead.
    pub fn polled() -> bool { POLLED.load(Ordering::Relaxed) }

    /// Interrupt/poll body: acknowledge INT and count it. A cyclic channel is kept running.
    fn service(ch: u8) -> bool {
        let Some(r) = regs(ch) else { return false };
        let cs = r.r(cb::CS);
        if cs & cb::CS_INT == 0 { return false; }
        let i = idx(ch);
        if CYCLIC.load(Ordering::Relaxed) & (1 << ch) != 0 {
            r.w(cb::CS, cb::cs_ack(CSFLAGS[i].load(Ordering::Relaxed)));
        } else {
            r.w(cb::CS, cb::CS_INT | cb::CS_PROT);
        }
        IRQS[i].fetch_add(1, Ordering::AcqRel);
        true
    }

    extern "C" fn isr(ctx: usize) -> IrqReturn {
        if service(ctx as u8) { IrqReturn::Handled } else { IrqReturn::None }
    }

    /// A claimed channel. Dropping it aborts any transfer and releases the channel.
    pub struct Channel {
        ch: u8,
        r: Mmio,
        scb_phys: usize,
        ring: Option<(u64, usize)>,
    }

    /// Claims a free channel from the DT mask: `prefer` if free, else the highest free one.
    /// A channel the firmware left running (CB != 0) is skipped and never used.
    pub fn claim(prefer: Option<u8>) -> Result<Channel, Dma40Error> {
        if !init() { return Err(Dma40Error::NoMap); }
        loop {
            let claimed = CLAIMED.load(Ordering::Acquire);
            let busy = claimed | FW_BUSY.load(Ordering::Relaxed);
            let ch = cb::pick_channel(cb::DT_CHANNEL_MASK, busy, prefer).ok_or(Dma40Error::NoChannel)?;
            if CLAIMED.compare_exchange(claimed, claimed | (1 << ch), Ordering::AcqRel, Ordering::Acquire).is_err() {
                continue;
            }
            let r = regs(ch).ok_or(Dma40Error::NoMap)?;
            if !cb::idle(&r) {
                FW_BUSY.fetch_or(1 << ch, Ordering::Relaxed);
                CLAIMED.fetch_and(!(1 << ch), Ordering::AcqRel);
                continue;
            }
            let Some(scb_phys) = buddy::alloc_below(0, cb::CB_ADDR_LIMIT as usize) else {
                CLAIMED.fetch_and(!(1 << ch), Ordering::AcqRel);
                return Err(Dma40Error::NoMap);
            };
            unsafe { core::ptr::write_bytes(phys_to_virt(scb_phys) as *mut u8, 0, 4096); }
            if IRQ_REGISTERED.fetch_or(1 << ch, Ordering::AcqRel) & (1 << ch) == 0 {
                if let Some(id) = cb::intid(ch) {
                    match crate::irq::request_irq(IrqSource::Line(id), isr, ch as usize) {
                        Ok(h) => core::mem::forget(h),
                        Err(_) => { IRQ_REGISTERED.fetch_and(!(1 << ch), Ordering::AcqRel); }
                    }
                }
            }
            return Ok(Channel { ch, r, scb_phys, ring: None });
        }
    }

    impl Channel {
        pub fn id(&self) -> u8 { self.ch }
        pub fn irqs(&self) -> u64 { IRQS[idx(self.ch)].load(Ordering::Acquire) }

        fn scbs(&mut self) -> &mut [Scb] {
            unsafe { core::slice::from_raw_parts_mut(phys_to_virt(self.scb_phys) as *mut Scb, SCBS_PER_PAGE) }
        }

        fn publish_scbs(&self, n: usize) {
            dcache_clean_inval(phys_to_virt(self.scb_phys), n * core::mem::size_of::<Scb>());
        }

        /// Synchronous mem-to-mem copy (physical addresses), bounded by `timeout_us`. Handles
        /// the cache maintenance for both buffers. Finishes on the interrupt or by polling.
        pub fn memcpy(&mut self, dst: u64, src: u64, len: usize, timeout_us: u32) -> Result<(), Dma40Error> {
            if self.ring.is_some() { self.stop(); }
            let phys = self.scb_phys as u64;
            let n = cb::build_memcpy(self.scbs(), phys, dst, src, len, true)?;
            self.publish_scbs(n);
            dcache_clean_inval(phys_to_virt(src as usize), len);
            dcache_clean_inval(phys_to_virt(dst as usize), len);
            CYCLIC.fetch_and(!(1 << self.ch), Ordering::AcqRel);
            CSFLAGS[idx(self.ch)].store(cb::CS_MEMCPY_FLAGS, Ordering::Relaxed);
            cb::ack_done(&self.r);
            let before = self.irqs();
            cb::start(&self.r, phys, cb::CS_MEMCPY_FLAGS)?;
            let i = idx(self.ch);
            let res = cb::wait_done(&self.r, timeout_us, &|| IRQS[i].load(Ordering::Acquire) != before);
            if res.is_err() { cb::abort(&self.r); }
            // Late INT (polled mode): count it like the ISR would.
            service(self.ch);
            cb::ack_done(&self.r);
            dcache_clean_inval(phys_to_virt(dst as usize), len);
            res
        }

        /// Starts a cyclic mem-to-device ring (`ring`..`ring+ring_len`, `period` bytes per SCB)
        /// into the peripheral FIFO at `fifo`, paced by the DREQ in `dreq_cell` (the DT `dmas`
        /// cell). The caller keeps the ring cache-clean (write, then [`dcache_clean_inval`]).
        /// Returns the number of periods.
        pub fn start_cyclic(&mut self, ring: u64, ring_len: usize, period: usize, fifo: u64,
            dreq_cell: u32, irq_per_period: bool) -> Result<usize, Dma40Error>
        {
            self.stop();
            let d = cb::DreqCfg::from_cell(dreq_cell);
            let phys = self.scb_phys as u64;
            let n = cb::build_cyclic_tx(self.scbs(), phys, ring, ring_len, period, fifo, &d, irq_per_period)?;
            self.publish_scbs(n);
            dcache_clean_inval(phys_to_virt(ring as usize), ring_len);
            CSFLAGS[idx(self.ch)].store(d.cs_flags, Ordering::Relaxed);
            CYCLIC.fetch_or(1 << self.ch, Ordering::AcqRel);
            self.ring = Some((ring, ring_len));
            cb::start(&self.r, phys, d.cs_flags)?;
            Ok(n)
        }

        /// Byte offset in the ring the engine reads next, or None if not running a ring.
        pub fn position(&self) -> Option<usize> {
            let (ring, len) = self.ring?;
            cb::ring_offset(self.r.r(cb::SRC), self.r.r(cb::SRCI), ring, len)
        }

        /// Periods (or one-shot chains) completed so far.
        pub fn periods(&self) -> u64 { self.irqs() }

        /// Polled fallback: service a pending INT. Harmless in IRQ mode.
        pub fn poll(&self) -> bool { service(self.ch) }

        /// `(cs, debug)` for diagnostics; e.g. MAI underruns show as DREQ_PAUSED staying clear.
        pub fn status(&self) -> (u32, u32) { (self.r.r(cb::CS), self.r.r(cb::DEBUG)) }

        /// Aborts whatever runs (bounded) and resets the channel. False if the pause timed out.
        pub fn stop(&mut self) -> bool {
            CYCLIC.fetch_and(!(1 << self.ch), Ordering::AcqRel);
            self.ring = None;
            cb::abort(&self.r)
        }
    }

    impl Drop for Channel {
        fn drop(&mut self) {
            self.stop();
            buddy::free(self.scb_phys, 0);
            CLAIMED.fetch_and(!(1 << self.ch), Ordering::AcqRel);
        }
    }

    const ST_OK: u32 = 1;
    const ST_SKIPPED: u32 = 2;
    const ST_FAIL: u32 = 3;
    const SELFTEST_BYTES: usize = 65536;
    const SELFTEST_ORDER: usize = 4;

    fn selftest_enabled(cmdline: &str) -> bool {
        !cmdline.split_ascii_whitespace().any(|t| t == "dma40.selftest=0")
    }

    /// Boot self-test (rpi5, task context): a 64 KiB mem-to-mem copy on a free channel with a
    /// CRC check. Inert: every outcome is one `[dma40] ...` line; it never panics or hangs.
    pub fn boot(cmdline: &str, serror_check: SerrorCheck) {
        if !selftest_enabled(cmdline) {
            serial_debug("[dma40] selftest skipped (dma40.selftest=0)\n");
            SELFTEST_STATE.store(ST_SKIPPED, Ordering::Relaxed);
            return;
        }
        if !init() {
            serial_debug("[dma40] fail map ioremap\n");
            SELFTEST_STATE.store(ST_FAIL, Ordering::Relaxed);
            return;
        }
        // Channel census before touching anything: firmware-owned channels show CB != 0.
        let mut census = alloc::string::String::new();
        for ch in cb::FIRST_CHAN..=cb::LAST_CHAN {
            if let Some(r) = regs(ch) {
                use core::fmt::Write;
                let _ = write!(census, " ch{}={:08x}/{:08x}", ch, r.r(cb::CS), r.r(cb::CB));
            }
        }
        if let Some(syn) = serror_check() {
            serial_debug(&alloc::format!("[dma40] fail serror stage=census syn=0x{:016x}\n", syn));
            SELFTEST_STATE.store(ST_FAIL, Ordering::Relaxed);
            return;
        }
        serial_debug(&alloc::format!("[dma40] cs/cb{}\n", census));
        let line = run_selftest(serror_check);
        serial_debug(&line);
    }

    fn run_selftest(serror_check: SerrorCheck) -> alloc::string::String {
        let fail = |what: &str| {
            SELFTEST_STATE.store(ST_FAIL, Ordering::Relaxed);
            alloc::format!("[dma40] selftest fail {}\n", what)
        };
        let mut ch = match claim(None) {
            Ok(c) => c,
            Err(e) => return fail(&alloc::format!("claim {:?}", e)),
        };
        let (Some(src), Some(dst)) = (
            buddy::alloc_below(SELFTEST_ORDER, cb::ADDR_LIMIT as usize),
            buddy::alloc_below(SELFTEST_ORDER, cb::ADDR_LIMIT as usize),
        ) else { return fail("alloc"); };
        let words = SELFTEST_BYTES / 4;
        let (sv, dv) = (phys_to_virt(src) as *mut u32, phys_to_virt(dst) as *mut u32);
        unsafe {
            for i in 0..words {
                sv.add(i).write(cb::pattern_word(i));
                dv.add(i).write(0xdead_beef);
            }
        }
        let want = cb::crc32(unsafe { core::slice::from_raw_parts(sv as *const u8, SELFTEST_BYTES) });
        let t0 = sched::monotonic_ns();
        let res = ch.memcpy(dst as u64, src as u64, SELFTEST_BYTES, 100_000);
        let us = (sched::monotonic_ns() - t0) / 1000;
        let irqs = ch.irqs();
        let (cs, dbg) = ch.status();
        let id = ch.id();
        let got = cb::crc32(unsafe { core::slice::from_raw_parts(dv as *const u8, SELFTEST_BYTES) });
        let syn = serror_check();
        drop(ch);
        buddy::free(src, SELFTEST_ORDER);
        buddy::free(dst, SELFTEST_ORDER);
        if let Some(syn) = syn {
            return fail(&alloc::format!("serror ch={} syn=0x{:016x}", id, syn));
        }
        if let Err(e) = res {
            return fail(&alloc::format!("{:?} ch={} cs=0x{:08x} debug=0x{:08x} t={}us", e, id, cs, dbg, us));
        }
        if got != want {
            return fail(&alloc::format!("crc ch={} want=0x{:08x} got=0x{:08x}", id, want, got));
        }
        let irq_ok = irqs > 0;
        POLLED.store(!irq_ok, Ordering::Relaxed);
        SELFTEST_STATE.store(ST_OK, Ordering::Relaxed);
        alloc::format!(
            "[dma40] selftest ok ch={} bytes={} crc=0x{:08x} mode={} intid={} src=0x{:x} dst=0x{:x} t={}us\n",
            id, SELFTEST_BYTES, got, if irq_ok { "irq" } else { "poll" },
            cb::intid(id).unwrap_or(0), src, dst, us)
    }
}
