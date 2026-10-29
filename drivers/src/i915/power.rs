//! Display power-well refcounts (Linux intel_display_power gen9, driver
//! requests in HSW_PWR_WELL_CTL2): PW2 (AUX B..D, pipes B/C, DDI audio) and the
//! DDI B/C/D IO wells. One mutex serialises request/release so concurrent
//! users (AUX probes, the pipe B sequence) cannot drop a well under each
//! other. A well the BIOS already requested (CTL1/CTL2) is never released.

use super::{ilog, now_ns, Mmio};

const PWR_WELL_CTL2: u32 = 0x45404;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dom { Pw2 = 0, DdiBIo = 1, DdiCIo = 2, DdiDIo = 3 }

const NDOM: usize = 4;
const NAMES: [&str; NDOM] = ["PW2", "DDI_B_IO", "DDI_C_IO", "DDI_D_IO"];

impl Dom {
    /// Well index in HSW_PWR_WELL_CTL (request bit 2i+1, state bit 2i).
    const fn idx(self) -> u32 { match self { Dom::Pw2 => 15, Dom::DdiBIo => 2, Dom::DdiCIo => 3, Dom::DdiDIo => 4 } }
    pub const fn req(self) -> u32 { 1 << (2 * self.idx() + 1) }
    pub const fn state(self) -> u32 { 1 << (2 * self.idx()) }
    pub const fn name(self) -> &'static str { NAMES[self as usize] }
    /// The IO well of DDI port 1 (B), 2 (C), 3 (D).
    pub fn ddi_io(port: u32) -> Option<Dom> {
        match port { 1 => Some(Dom::DdiBIo), 2 => Some(Dom::DdiCIo), 3 => Some(Dom::DdiDIo), _ => None }
    }
}

/// Per-domain users and whether we set the request bit (so only we clear it).
struct State { refs: [u32; NDOM], ours: [bool; NDOM], max_us: [u64; NDOM] }
static STATE: spin::Mutex<State> = spin::Mutex::new(State { refs: [0; NDOM], ours: [false; NDOM], max_us: [0; NDOM] });

/// Take a reference; the first one requests the well and waits (up to 1 ms,
/// as Linux's 1 ms timeout) for the state bit. Returns false if the state bit
/// did not come up (the reference is still held; call `put`).
pub fn get(m: &Mmio, d: Dom) -> bool {
    let mut g = STATE.lock();
    let i = d as usize;
    let mut up = true;
    if g.refs[i] == 0 {
        let old = m.r(PWR_WELL_CTL2);
        g.ours[i] = old & d.req() == 0;
        if g.ours[i] {
            let t0 = now_ns();
            m.w(PWR_WELL_CTL2, old | d.req());
            up = m.wait(PWR_WELL_CTL2, d.state(), d.state(), 1000).is_ok();
            let us = now_ns().wrapping_sub(t0) / 1000;
            if us > g.max_us[i] { g.max_us[i] = us; }
        }
    }
    g.refs[i] += 1;
    up
}

/// Drop a reference; the last one releases a well we requested.
pub fn put(m: &Mmio, d: Dom) {
    let mut g = STATE.lock();
    let i = d as usize;
    g.refs[i] = g.refs[i].saturating_sub(1);
    if g.refs[i] == 0 && g.ours[i] {
        m.w(PWR_WELL_CTL2, m.r(PWR_WELL_CTL2) & !d.req());
        g.ours[i] = false;
    }
}

pub fn refs(d: Dom) -> u32 { STATE.lock().refs[d as usize] }

/// One line with every domain's refcount, request/state bits and slowest
/// bring-up, e.g. `power: PW2 refs 0 req 0 on 0 (max 31 us), ...`.
pub fn report(m: &Mmio, what: &str) {
    let g = STATE.lock();
    let ctl = m.r(PWR_WELL_CTL2);
    let mut s = alloc::string::String::new();
    for d in [Dom::Pw2, Dom::DdiBIo, Dom::DdiCIo, Dom::DdiDIo] {
        use core::fmt::Write;
        let _ = write!(s, "{} refs {} req {} on {} (max {} us); ", d.name(), g.refs[d as usize],
                       (ctl & d.req() != 0) as u8, (ctl & d.state() != 0) as u8, g.max_us[d as usize]);
    }
    ilog!("power: {} CTL2 0x{:08x}: {}", what, ctl, s.trim_end());
}

/// Run `f` with the domain held.
pub fn with<T>(m: &Mmio, d: Dom, f: impl FnOnce() -> T) -> T {
    get(m, d);
    let r = f();
    put(m, d);
    r
}
