//! External outputs (lane lapdisp): the DDI ports beside eDP — HDMI and
//! DP / USB-C (Thunderbolt DP-in) — detected, probed and reported to the DRM
//! layer as extra connectors (`gpu_kms::KmsOutputs`).
//!
//! WHAT IS HERE. Port discovery from the VBT child devices (block 2: dvo_port,
//! DDC pin, AUX channel, device type; DP++ "dual mode" ports become a DP and
//! an HDMI-A connector, as Linux does), else from SFUSE_STRAP. Live hotplug
//! state from the PCH (SDEISR, SPT bits 21..23 = ports B..D). DP: a native AUX
//! read of DPCD 0x000..0x00F (DP_AUX_CH_CTL/DATA, power well 2 requested for
//! the transaction) and EDID over I2C-over-AUX. HDMI: EDID over GMBUS
//! (`GMBUS0..5`). Status is re-probed on GETCONNECTOR whenever the live HPD
//! bit changed (no DRM lock held there); every other path reads the cache.
//!
//! WHAT IS NOT. No second pipe / transcoder / DPLL / DDI modeset and no DP
//! link training yet: the encoders report possible_crtcs 0, so a compositor
//! lists the outputs (with EDID modes) but cannot light them. No HPD
//! interrupt and no hotplug uevent: status updates when a client asks.
//!
//! Knobs: `i915.extout=0` registers nothing; `i915.extout_aux=0` skips the
//! boot-time AUX probe of unplugged ports (the register-path proof).
//! Inert on failure: every probe is bounded, a failure reads "disconnected".

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::gpu_kms::{KmsConnInfo, KmsOutputs, KmsTiming};

use super::{delay_us, ilog, now_ns, opt, Device, Mmio};

const SFUSE_STRAP: u32 = 0xC2014;
const SDEISR: u32 = 0xC4000;
const PWR_WELL_CTL2: u32 = 0x45404; // driver request
const PW2_REQ: u32 = 1 << 31;
const PW2_STATE: u32 = 1 << 30;

const GMBUS0: u32 = 0xC5100;
const GMBUS1: u32 = 0xC5104;
const GMBUS2: u32 = 0xC5108;
const GMBUS3: u32 = 0xC510C;
const GMBUS4: u32 = 0xC5110;
const GMBUS5: u32 = 0xC5120;

const AUX_CTL_BASE: u32 = 0x64010;
const AUX_BUSY: u32 = 1 << 31;
const AUX_DONE: u32 = 1 << 30;
const AUX_TIMEOUT: u32 = 1 << 28;
const AUX_RCV_ERR: u32 = 1 << 25;

#[derive(Clone, Copy, Default)]
struct Port {
    /// DDI port: 1 = B, 2 = C, 3 = D.
    ddi: u8,
    gmbus_pin: u8,
    /// AUX channel index (0 = A .. 3 = D).
    aux: u8,
    dp: bool,
    hdmi: bool,
}

#[derive(Clone, Copy, Default)]
struct Conn {
    port: usize,
    is_dp: bool,
    type_id: u32,
    info: KmsConnInfo,
    /// Live HPD at the last probe (u8::MAX = never probed).
    hpd: u8,
}

static MMIO: AtomicUsize = AtomicUsize::new(0);
static FAKE: AtomicBool = AtomicBool::new(false);
static PROBING: AtomicBool = AtomicBool::new(false);
static PORTS: spin::Mutex<Vec<Port>> = spin::Mutex::new(Vec::new());
static CONNS: spin::Mutex<Vec<Conn>> = spin::Mutex::new(Vec::new());
static VBT_PORTS: spin::Mutex<Vec<Port>> = spin::Mutex::new(Vec::new());

static OUTPUTS: KmsOutputs = KmsOutputs { count, info, cached };

fn mmio() -> Mmio { Mmio { va: MMIO.load(Ordering::Acquire) } }

/// From `probe::log_opregion`: remember the VBT's external DDI ports.
pub(super) fn set_vbt(vbt: &[u8]) {
    let mut out = Vec::new();
    let rd16 = |o: usize| vbt.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let rd32 = |o: usize| vbt.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let mut parse = || -> Option<()> {
        let bdb = rd32(0x1c)?;
        let mut o = bdb + rd16(bdb + 0x12)?;
        let end = (bdb + rd16(bdb + 0x14)?).min(vbt.len());
        while o + 3 <= end {
            let (id, sz) = (vbt[o], rd16(o + 1)?);
            if id == 2 {
                let d = vbt.get(o + 3..o + 3 + sz)?;
                let cs = *d.get(4)? as usize;
                if cs < 0x1b { return None; }
                for c in d[5..].chunks_exact(cs) {
                    let ty = u16::from_le_bytes([c[2], c[3]]);
                    let ddi = match c[0x10] { 7 => 1, 8 => 2, 9 => 3, _ => continue }; // DVO_PORT_DPB/C/D (also HDMIB..D)
                    if ty == 0 { continue; }
                    let dp = ty & (1 << 2) != 0;
                    let hdmi = ty & (1 << 4) != 0 && ty & (1 << 11) == 0;
                    let aux = match c[0x1a] { 0x10 => 1, 0x20 => 2, 0x30 => 3, 0x40 => 0, _ => ddi };
                    out.push(Port { ddi, gmbus_pin: c[0x13], aux, dp, hdmi });
                }
            }
            o += 3 + sz;
        }
        Some(())
    };
    if parse().is_none() { ilog!("outputs: VBT child devices unreadable; SFUSE_STRAP fallback"); }
    *VBT_PORTS.lock() = out;
}

/// From `kms::init`, before the backend registers.
pub(super) fn init(d: &Device) {
    if opt("i915.extout") == Some(0) { ilog!("outputs: i915.extout=0, external connectors not reported"); return; }
    MMIO.store(d.mmio.va, Ordering::Release);
    FAKE.store(d.fake, Ordering::Release);
    let m = d.mmio;
    let mut ports = VBT_PORTS.lock().clone();
    let src = if !ports.is_empty() { "VBT" } else {
        // SFUSE_STRAP bit 2/1/0 = DDI B/C/D present; SKL default GMBUS pins.
        let s = if d.fake { 0b110 } else { m.r(SFUSE_STRAP) };
        for (bit, ddi, pin) in [(2u32, 1u8, 5u8), (1, 2, 4), (0, 3, 6)] {
            if s & (1 << bit) != 0 { ports.push(Port { ddi, gmbus_pin: pin, aux: ddi, dp: true, hdmi: true }); }
        }
        if d.fake { "fake device" } else { "SFUSE_STRAP" }
    };
    let mut conns = Vec::new();
    let (mut ndp, mut nhdmi) = (0u32, 0u32);
    for (pi, p) in ports.iter().enumerate() {
        if p.dp { ndp += 1; conns.push(conn(pi, true, ndp)); }
        if p.hdmi { nhdmi += 1; conns.push(conn(pi, false, nhdmi)); }
    }
    conns.truncate(crate::gpu_kms::EXT_MAX);
    let n = conns.len();
    let desc: Vec<alloc::string::String> = ports.iter().map(|p| alloc::format!("DDI-{} (gmbus pin {}, AUX {}{}{})",
        (b'A' + p.ddi) as char, p.gmbus_pin, (b'A' + p.aux) as char, if p.dp { ", DP" } else { "" }, if p.hdmi { ", HDMI" } else { "" })).collect();
    *PORTS.lock() = ports;
    *CONNS.lock() = conns;
    ilog!("outputs: {} ports from {}: {}; {} extra connectors, SDEISR=0x{:08x}",
        desc.len(), src, desc.join(" "), n, if d.fake { 0 } else { m.r(SDEISR) });
    if n == 0 { return; }
    // The register-path proof: one AUX transaction on every DP port, plugged
    // or not (an empty port must time out cleanly, never stay busy).
    if !d.fake && opt("i915.extout_aux") != Some(0) {
        for p in PORTS.lock().clone().iter().filter(|p| p.dp) {
            let t0 = now_ns();
            let mut b = [0u8; 1];
            let r = with_pw2(|| aux_native_read(&m, p.aux, 0, &mut b));
            ilog!("outputs: AUX-{} probe: {} in {} us (hpd live={})", (b'A' + p.aux) as char,
                match r { Ok(()) => alloc::format!("DPCD_REV=0x{:02x}", b[0]), Err(e) => alloc::format!("no sink ({})", e) },
                (now_ns() - t0) / 1000, hpd_live(&m, p.ddi));
        }
    }
    for i in 0..n { let _ = info(i); }
    let l = CONNS.lock();
    let st: Vec<alloc::string::String> = l.iter().map(|c| alloc::format!("{}-{}={}", if c.is_dp { "DP" } else { "HDMI-A" }, c.type_id,
        if c.info.connected { "connected" } else { "disconnected" })).collect();
    drop(l);
    crate::gpu_kms::register_outputs(&OUTPUTS);
    ilog!("outputs: registered {} (no second-pipe modeset yet: possible_crtcs 0)", st.join(" "));
}

fn conn(port: usize, is_dp: bool, type_id: u32) -> Conn {
    let info = KmsConnInfo { conn_type: if is_dp { 10 } else { 11 }, type_id, enc_type: 2, ..Default::default() };
    Conn { port, is_dp, type_id, info, hpd: u8::MAX }
}

fn hpd_live(m: &Mmio, ddi: u8) -> bool {
    if FAKE.load(Ordering::Relaxed) { return false; }
    m.r(SDEISR) & (1 << (20 + ddi as u32)) != 0
}

fn count() -> usize { CONNS.lock().len() }

fn cached(i: usize) -> Option<KmsConnInfo> { CONNS.lock().get(i).map(|c| c.info) }

/// Re-probe connector `i` when its port's live HPD changed since the last probe.
fn info(i: usize) -> Option<KmsConnInfo> {
    let (c, p) = {
        let l = CONNS.lock();
        let c = *l.get(i)?;
        (c, *PORTS.lock().get(c.port)?)
    };
    let m = mmio();
    let hpd = hpd_live(&m, p.ddi);
    if c.hpd == hpd as u8 || PROBING.swap(true, Ordering::Acquire) { return Some(c.info); }
    let mut inf = c.info;
    inf.connected = false;
    inf.nmodes = 0;
    if hpd {
        let mut edid = [0u8; 128];
        let ok = if c.is_dp {
            let mut rev = [0u8; 1];
            with_pw2(|| aux_native_read(&m, p.aux, 0, &mut rev).and_then(|_| aux_i2c_read_edid(&m, p.aux, &mut edid))).is_ok()
        } else {
            // A DP sink on a dual-mode port belongs to the DP connector.
            let dp_sink = p.dp && with_pw2(|| aux_native_read(&m, p.aux, 0, &mut [0u8; 1])).is_ok();
            !dp_sink && gmbus_read_edid(&m, p.gmbus_pin, &mut edid).is_ok()
        };
        if ok && edid[..8] == [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0] {
            inf.connected = true;
            parse_edid(&edid, &mut inf);
        } else if ok {
            inf.connected = true; // a sink answered but the EDID is unusable
        }
    }
    {
        let mut l = CONNS.lock();
        if let Some(e) = l.get_mut(i) { e.info = inf; e.hpd = hpd as u8; }
    }
    PROBING.store(false, Ordering::Release);
    ilog!("outputs: {}-{} hpd={} -> {} ({} modes{})", if c.is_dp { "DP" } else { "HDMI-A" }, c.type_id, hpd,
        if inf.connected { "connected" } else { "disconnected" }, inf.nmodes,
        if inf.nmodes > 0 { alloc::format!(", preferred {}x{}", inf.modes[0].h[0], inf.modes[0].v[0]) } else { alloc::string::String::new() });
    Some(inf)
}

fn parse_edid(e: &[u8; 128], inf: &mut KmsConnInfo) {
    inf.mm_w = e[21] as u32 * 10;
    inf.mm_h = e[22] as u32 * 10;
    for k in 0..4 {
        let d = &e[54 + 18 * k..72 + 18 * k];
        let clk = u16::from_le_bytes([d[0], d[1]]) as u32 * 10;
        if clk == 0 || inf.nmodes >= inf.modes.len() { continue; }
        let ha = d[2] as u32 | (d[4] as u32 >> 4) << 8;
        let hb = d[3] as u32 | (d[4] as u32 & 0xf) << 8;
        let va = d[5] as u32 | (d[7] as u32 >> 4) << 8;
        let vb = d[6] as u32 | (d[7] as u32 & 0xf) << 8;
        let hso = d[8] as u32 | ((d[11] as u32 >> 6) & 3) << 8;
        let hsw = d[9] as u32 | ((d[11] as u32 >> 4) & 3) << 8;
        let vso = (d[10] as u32 >> 4) | ((d[11] as u32 >> 2) & 3) << 4;
        let vsw = (d[10] as u32 & 0xf) | (d[11] as u32 & 3) << 4;
        if ha == 0 || va == 0 { continue; }
        if k == 0 {
            let (w, h) = (d[12] as u32 | (d[14] as u32 >> 4) << 8, d[13] as u32 | (d[14] as u32 & 0xf) << 8);
            if w > 0 && h > 0 { inf.mm_w = w; inf.mm_h = h; }
        }
        let f = d[17];
        let mut flags = 0;
        if f & 0x18 == 0x18 {
            flags |= if f & 2 != 0 { 1 } else { 2 }; // PHSYNC / NHSYNC
            flags |= if f & 4 != 0 { 4 } else { 8 }; // PVSYNC / NVSYNC
        }
        let n = inf.nmodes;
        inf.modes[n] = KmsTiming {
            clock_khz: clk,
            h: [ha as u16, (ha + hso) as u16, (ha + hso + hsw) as u16, (ha + hb) as u16],
            v: [va as u16, (va + vso) as u16, (va + vso + vsw) as u16, (va + vb) as u16],
            flags,
        };
        inf.nmodes += 1;
    }
}

/// Run `f` with power well 2 (AUX B..D) on; restore the request after.
fn with_pw2<T>(f: impl FnOnce() -> T) -> T {
    let m = mmio();
    let old = m.r(PWR_WELL_CTL2);
    let req = old & PW2_REQ == 0;
    if req {
        m.w(PWR_WELL_CTL2, old | PW2_REQ);
        let _ = m.wait(PWR_WELL_CTL2, PW2_STATE, PW2_STATE, 2000);
    }
    let r = f();
    if req { m.w(PWR_WELL_CTL2, m.r(PWR_WELL_CTL2) & !PW2_REQ); }
    r
}

// ── DP AUX ──────────────────────────────────────────────────────────────────

/// One AUX transaction: `tx` bytes out, reply into `rx`; returns the reply length.
fn aux_xfer(m: &Mmio, ch: u8, tx: &[u8], rx: &mut [u8; 20]) -> Result<usize, &'static str> {
    let ctl = AUX_CTL_BASE + ch as u32 * 0x100;
    if m.r(ctl) & AUX_BUSY != 0 { return Err("channel busy"); }
    for (i, w) in tx.chunks(4).enumerate() {
        let mut b = [0u8; 4];
        b[..w.len()].copy_from_slice(w);
        m.w(ctl + 4 + 4 * i as u32, u32::from_be_bytes(b));
    }
    m.w(ctl, AUX_BUSY | AUX_DONE | AUX_TIMEOUT | (3 << 26) | AUX_RCV_ERR | (tx.len() as u32) << 20 | (31 << 5) | 31);
    let v = match m.wait(ctl, AUX_BUSY, 0, 10_000) { Ok(_) => m.r(ctl), Err(_) => return Err("stuck busy") };
    m.w(ctl, v | AUX_DONE | AUX_TIMEOUT | AUX_RCV_ERR);
    if v & AUX_TIMEOUT != 0 { return Err("timeout"); }
    if v & AUX_RCV_ERR != 0 { return Err("receive error"); }
    let n = ((v >> 20) & 0x1f) as usize;
    if n == 0 || n > 20 { return Err("bad reply size"); }
    for i in 0..(n + 3) / 4 {
        let w = m.r(ctl + 4 + 4 * i as u32).to_be_bytes();
        for j in 0..4 { if 4 * i + j < 20 { rx[4 * i + j] = w[j]; } }
    }
    Ok(n)
}

/// AUX request with DEFER retries; `Ok(reply data)` on ACK.
fn aux_req(m: &Mmio, ch: u8, cmd: u8, addr: u32, data: &[u8], rlen: usize, out: &mut [u8]) -> Result<usize, &'static str> {
    let mut tx = [0u8; 20];
    tx[0] = cmd << 4 | ((addr >> 16) & 0xf) as u8;
    tx[1] = (addr >> 8) as u8;
    tx[2] = addr as u8;
    let mut n = 3;
    let len = if cmd & 1 != 0 { rlen } else { data.len() };
    if len > 0 { tx[3] = (len - 1) as u8; n = 4; }
    tx[n..n + data.len()].copy_from_slice(data);
    n += data.len();
    let mut timeouts = 0;
    for _ in 0..16 {
        let mut rx = [0u8; 20];
        match aux_xfer(m, ch, &tx[..n], &mut rx) {
            Ok(k) => {
                let native = (rx[0] >> 4) & 3;
                let i2c = (rx[0] >> 6) & 3;
                if native == 2 || (native == 0 && i2c == 2) { delay_us(500); continue; } // DEFER
                if native == 1 { return Err("native NACK"); }
                if i2c == 1 { return Err("i2c NACK"); }
                let got = (k - 1).min(out.len());
                out[..got].copy_from_slice(&rx[1..1 + got]);
                return Ok(got);
            }
            Err("timeout") if timeouts < 2 => { timeouts += 1; delay_us(400); }
            Err(e) => return Err(e),
        }
    }
    Err("deferred too long")
}

fn aux_native_read(m: &Mmio, ch: u8, addr: u32, out: &mut [u8]) -> Result<(), &'static str> {
    let got = aux_req(m, ch, 0x9, addr, &[], out.len(), out)?;
    if got < out.len() { Err("short read") } else { Ok(()) }
}

/// EDID block 0 over I2C-over-AUX (slave 0x50).
fn aux_i2c_read_edid(m: &Mmio, ch: u8, edid: &mut [u8; 128]) -> Result<(), &'static str> {
    const WRITE_MOT: u8 = 0x4;
    const READ_MOT: u8 = 0x5;
    aux_req(m, ch, WRITE_MOT, 0x50, &[0], 0, &mut [])?;
    let mut off = 0;
    while off < 128 {
        let mut chunk = [0u8; 16];
        let got = aux_req(m, ch, READ_MOT, 0x50, &[], 16, &mut chunk)?;
        if got == 0 { return Err("empty i2c read"); }
        edid[off..off + got].copy_from_slice(&chunk[..got]);
        off += got;
    }
    // Stop: address-only READ without MOT.
    let _ = aux_req(m, ch, 0x1, 0x50, &[], 0, &mut []);
    Ok(())
}

// ── GMBUS ───────────────────────────────────────────────────────────────────

fn gmbus_read_edid(m: &Mmio, pin: u8, edid: &mut [u8; 128]) -> Result<(), &'static str> {
    if FAKE.load(Ordering::Relaxed) { return Err("fake device"); }
    m.w(GMBUS4, 0);
    m.w(GMBUS5, 0);
    m.w(GMBUS0, pin as u32); // 100 kHz
    let len = 128u32;
    // CYCLE_INDEX (offset 0) + CYCLE_WAIT, read from 0x50.
    m.w(GMBUS1, 1 << 30 | 1 << 26 | 1 << 25 | len << 16 | (0x50 << 1) | 1);
    let mut r = Ok(());
    let mut off = 0usize;
    while off < 128 {
        match m.wait(GMBUS2, (1 << 11) | (1 << 10), 1 << 11, 50_000) {
            Ok(_) => {}
            Err(v) => { r = Err(if v & (1 << 10) != 0 { "NAK (no sink)" } else { "timeout" }); break; }
        }
        let w = m.r(GMBUS3).to_le_bytes();
        for j in 0..4 { if off < 128 { edid[off] = w[j]; off += 1; } }
    }
    if r.is_ok() {
        let _ = m.wait(GMBUS2, 1 << 14, 1 << 14, 50_000);
        m.w(GMBUS1, 1 << 30 | 1 << 27); // SW_RDY | CYCLE_STOP
    } else {
        m.w(GMBUS1, 1 << 31); // SW_CLR_INT
    }
    let _ = m.wait(GMBUS2, 1 << 9, 0, 10_000);
    m.w(GMBUS1, 0);
    m.w(GMBUS0, 0);
    r
}
