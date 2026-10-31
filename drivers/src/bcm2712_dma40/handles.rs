//! Handle table over claimed dma40 channels, for module drivers (`kapi_dma40_*`). A handle is
//! `slot + 1`; 0 is never valid. Off the rpi5 build every call fails with -19. Modules are
//! permanent, so there is no owner cleanup: a module that dies mid-stream keeps its channel.

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod real {
    use super::super::{cb::Dma40Error, claim, polled, Channel};
    use spin::Mutex;

    struct Slot(Option<Channel>);
    // The channel owns raw MMIO addresses only; every access goes through the table lock.
    unsafe impl Send for Slot {}
    const NSLOT: usize = 6;
    static TABLE: Mutex<[Slot; NSLOT]> = Mutex::new([const { Slot(None) }; NSLOT]);

    fn errno(e: Dma40Error) -> i64 {
        match e { Dma40Error::NoChannel => -16, Dma40Error::NoMap => -19, _ => -22 }
    }

    fn with<R>(h: u32, f: impl FnOnce(&mut Channel) -> R) -> Option<R> {
        let i = (h as usize).checked_sub(1)?;
        let mut t = TABLE.lock();
        t.get_mut(i)?.0.as_mut().map(f)
    }

    /// `prefer` < 0 = any channel. Returns the handle or -errno.
    pub fn claim_handle(prefer: i32) -> i64 {
        let p = if prefer < 0 { None } else { Some(prefer as u8) };
        let ch = match claim(p) { Ok(c) => c, Err(e) => return errno(e) };
        let mut t = TABLE.lock();
        for (i, s) in t.iter_mut().enumerate() {
            if s.0.is_none() { s.0 = Some(ch); return i as i64 + 1; }
        }
        -16
    }

    pub fn release(h: u32) -> i32 {
        let Some(i) = (h as usize).checked_sub(1) else { return -22 };
        let ch = { let mut t = TABLE.lock(); t.get_mut(i).and_then(|s| s.0.take()) };
        match ch { Some(c) => { drop(c); 0 } None => -22 }
    }

    pub fn start_cyclic(h: u32, ring: u64, len: usize, period: usize, fifo: u64, dreq: u32, irq: bool) -> i64 {
        with(h, |c| match c.start_cyclic(ring, len, period, fifo, dreq, irq) { Ok(n) => n as i64, Err(e) => errno(e) }).unwrap_or(-22)
    }
    pub fn stop(h: u32) -> i32 { with(h, |c| if c.stop() { 0 } else { -110 }).unwrap_or(-22) }
    pub fn position(h: u32) -> i64 { with(h, |c| c.position().map_or(-1, |p| p as i64)).unwrap_or(-22) }
    pub fn periods(h: u32) -> u64 { with(h, |c| c.periods()).unwrap_or(0) }
    pub fn poll(h: u32) -> i32 { with(h, |c| c.poll() as i32).unwrap_or(-22) }
    pub fn is_polled() -> i32 { polled() as i32 }
}

#[cfg(not(all(target_arch = "aarch64", feature = "rpi5")))]
mod real {
    pub fn claim_handle(_prefer: i32) -> i64 { -19 }
    pub fn release(_h: u32) -> i32 { -19 }
    pub fn start_cyclic(_h: u32, _ring: u64, _len: usize, _period: usize, _fifo: u64, _dreq: u32, _irq: bool) -> i64 { -19 }
    pub fn stop(_h: u32) -> i32 { -19 }
    pub fn position(_h: u32) -> i64 { -19 }
    pub fn periods(_h: u32) -> u64 { 0 }
    pub fn poll(_h: u32) -> i32 { -19 }
    pub fn is_polled() -> i32 { -19 }
}

pub use real::*;
