//! USB Video Class (UVC 1.0–1.5) streaming over isochronous IN endpoints.
//!
//! Enumeration hands every configuration with a VideoStreaming interface
//! (class 0E, subclass 02) to [`register`]. This module parses the
//! class-specific descriptors (MJPEG and uncompressed formats and their
//! frame sizes), negotiates a format with the VS_PROBE / VS_COMMIT controls,
//! selects the smallest alternate setting whose isochronous bandwidth holds
//! the negotiated `dwMaxPayloadTransferSize`, and opens a *framed*
//! [`IsochPipe`] on it (one record per service interval: one UVC payload).
//!
//! Payload reassembly (UVC 1.5 §2.4.3.3): every payload starts with a header
//! (`bHeaderLength`, `bmHeaderInfo`: FID bit 0, EOF bit 1, ERR bit 6). Data
//! after the header is appended to the frame being filled; a FID toggle or
//! EOF completes it. Frames land in caller-provided buffers that follow the
//! V4L2 queue model: [`Stream::qbuf`] gives a buffer to the driver,
//! completed frames are taken with [`Stream::dqbuf`]. With no queued buffer
//! a frame is dropped (counted). Buffers are memory the caller owns (the
//! kernel's V4L2 layer allocates mmap-able pages); this module never
//! allocates after [`Stream::start`], so [`Stream::poll`] is tick-safe.
//!
//! Self-test: `uvc.selftest=<frames>` on the kernel command line streams the
//! first camera's default format at enumeration and logs every frame's size
//! and FNV-1a checksum (`[UVC] frame …`), then stops the stream.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;

use crate::platform::{self, Deadline};
use crate::transport::UsbErr;
use crate::usb_log;
use crate::xhci::isoch::{IsochEpDesc, IsochPipe, Poll, StreamFormat, UsbFunction};

pub const FMT_MJPEG: u8 = 1;
pub const FMT_YUYV: u8 = 2;
pub const FMT_NV12: u8 = 3;
pub const FMT_OTHER: u8 = 0;

/// One frame size of one format.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameDesc {
    pub index: u8,
    pub width: u16,
    pub height: u16,
    pub max_size: u32,
    /// Default frame interval in 100 ns units.
    pub interval: u32,
}

/// One format (bFormatIndex) of the streaming interface.
#[derive(Clone, Debug, Default)]
pub struct FormatDesc {
    pub index: u8,
    pub kind: u8,
    pub bpp: u8,
    pub default_frame: u8,
    pub frames: Vec<FrameDesc>,
}

/// What the class-specific descriptors say.
#[derive(Clone, Debug, Default)]
pub struct VideoInfo {
    pub bcd_uvc: u16,
    pub vc_iface: u8,
    pub vs_iface: u8,
    pub formats: Vec<FormatDesc>,
    pub alts: Vec<IsochEpDesc>,
}

/// Whether `cfg` has a video streaming interface.
pub fn has_video_streaming(cfg: &[u8]) -> bool {
    walk(cfg).any(|d| d.len() >= 9 && d[1] == 4 && d[5] == 0x0E && d[6] == 0x02)
}

fn walk(cfg: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut i = 0usize;
    core::iter::from_fn(move || {
        if i + 2 > cfg.len() { return None; }
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() { return None; }
        let d = &cfg[i..i + len];
        i += len;
        Some(d)
    })
}

fn le16(d: &[u8], o: usize) -> u16 { u16::from_le_bytes([d[o], d[o + 1]]) }
fn le32(d: &[u8], o: usize) -> u32 { u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) }

/// Parse the VideoControl / VideoStreaming descriptors of the first video
/// function in `cfg`.
pub fn parse(cfg: &[u8]) -> Option<VideoInfo> {
    let mut v = VideoInfo::default();
    let (mut cur_class, mut cur_sub, mut cur_if) = (0u8, 0u8, 0xFFu8);
    let mut have_vs = false;
    for d in walk(cfg) {
        match d[1] {
            4 if d.len() >= 9 => {
                cur_if = d[2]; cur_class = d[5]; cur_sub = d[6];
                if cur_class == 0x0E && cur_sub == 0x01 && v.bcd_uvc == 0 { v.vc_iface = cur_if; }
                if cur_class == 0x0E && cur_sub == 0x02 && !have_vs { v.vs_iface = cur_if; have_vs = true; }
            }
            0x24 if cur_class == 0x0E && d.len() >= 3 => {
                if cur_sub == 0x01 && d[2] == 0x01 && d.len() >= 5 && cur_if == v.vc_iface {
                    v.bcd_uvc = le16(d, 3);
                } else if cur_sub == 0x02 && cur_if == v.vs_iface {
                    match d[2] {
                        0x04 | 0x06 if d.len() >= 11 => {
                            let (kind, bpp, def) = if d[2] == 0x06 {
                                (FMT_MJPEG, 0, d[6])
                            } else if d.len() >= 27 {
                                let kind = match &d[5..9] {
                                    b"YUY2" => FMT_YUYV,
                                    b"NV12" => FMT_NV12,
                                    _ => FMT_OTHER,
                                };
                                (kind, d[21], d[22])
                            } else { (FMT_OTHER, 0, 1) };
                            v.formats.push(FormatDesc { index: d[3], kind, bpp, default_frame: def, frames: Vec::new() });
                        }
                        0x05 | 0x07 if d.len() >= 26 => {
                            if let Some(f) = v.formats.last_mut() {
                                f.frames.push(FrameDesc {
                                    index: d[3], width: le16(d, 5), height: le16(d, 7),
                                    max_size: le32(d, 17), interval: le32(d, 21),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if !have_vs { return None; }
    v.alts = crate::xhci::isoch::scan_isoch(cfg).into_iter()
        .filter(|e| e.iface == v.vs_iface && e.is_in()).collect();
    Some(v)
}

/// Bytes one service interval of `e` carries at high speed.
fn alt_bytes(e: &IsochEpDesc) -> u32 {
    let mps = (e.mps & 0x7FF) as u32;
    if e.ss_bytes != 0 { e.ss_bytes as u32 } else { mps * (((e.mps >> 11) & 3) as u32 + 1) }
}

fn probe_len(bcd: u16) -> usize {
    if bcd >= 0x0150 { 48 } else if bcd >= 0x0110 { 34 } else { 26 }
}

/// The result of probe/commit.
#[derive(Clone, Copy, Debug, Default)]
pub struct Negotiated {
    pub format: u8,
    pub frame: u8,
    pub interval: u32,
    pub max_frame: u32,
    pub max_payload: u32,
}

/// VS_PROBE_CONTROL SET_CUR / GET_CUR, then VS_COMMIT_CONTROL SET_CUR.
pub fn negotiate(f: &UsbFunction, v: &VideoInfo, format: u8, frame: u8, interval: u32)
                 -> Result<Negotiated, UsbErr> {
    let n = probe_len(v.bcd_uvc);
    let mut p = [0u8; 48];
    p[0..2].copy_from_slice(&1u16.to_le_bytes()); // bmHint: keep dwFrameInterval
    p[2] = format;
    p[3] = frame;
    p[4..8].copy_from_slice(&interval.to_le_bytes());
    let idx = v.vs_iface as u16;
    f.control_out(0x21, 0x01, 1 << 8, idx, &p[..n])?;
    let mut r = [0u8; 48];
    let got = f.control_in(0xA1, 0x81, 1 << 8, idx, &mut r[..n])?;
    if got < 26 { return Err(UsbErr::Other(0xFF)); }
    f.control_out(0x21, 0x01, 2 << 8, idx, &r[..n])?;
    let fd = v.formats.iter().find(|x| x.index == r[2]).and_then(|x| x.frames.iter().find(|y| y.index == r[3]));
    let mut max_frame = le32(&r, 18);
    if max_frame == 0 { max_frame = fd.map_or(0, |y| y.max_size); }
    Ok(Negotiated { format: r[2], frame: r[3], interval: le32(&r, 4), max_frame, max_payload: le32(&r, 22) })
}

// ── the stream ──────────────────────────────────────────────────────────────

/// A frame buffer owned by the caller; the driver writes `cap` bytes at most.
#[derive(Clone, Copy, Debug)]
pub struct Buf {
    pub ptr: *mut u8,
    pub cap: usize,
}
unsafe impl Send for Buf {}

/// A completed frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Done {
    pub index: usize,
    pub bytes: usize,
    pub seq: u32,
    pub ts_ns: u64,
    /// The device flagged an error, or the frame overflowed its buffer.
    pub error: bool,
}

/// Stream counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct StreamStats {
    pub payloads: u64,
    pub frames: u64,
    pub dropped: u64,
    pub errors: u64,
}

pub struct Stream {
    pipe: Option<IsochPipe>,
    /// `uvc.fake=1`: (format kind, polls since the last frame); no hardware.
    fake: Option<(u8, u32)>,
    bufs: Vec<Buf>,
    /// Buffer indices given to the driver, oldest first.
    queued: Vec<usize>,
    done: Vec<Done>,
    /// The frame being filled: (buffer index or None when dropping, bytes, error).
    cur: Option<(Option<usize>, usize, bool)>,
    last_fid: u8,
    seq: u32,
    pkt: Box<[u8]>,
    pub neg: Negotiated,
    pub stats: StreamStats,
    gone: bool,
}

impl Stream {
    /// Negotiate `format`/`frame` (0 = the device's default) and open the
    /// streaming pipe. `bufs` are the frame buffers (none queued yet). Task
    /// context.
    pub fn start(f: &UsbFunction, v: &VideoInfo, format: u8, frame: u8, bufs: Vec<Buf>)
                 -> Result<Stream, &'static str> {
        let fmt = v.formats.iter().find(|x| x.index == format).or(v.formats.first()).ok_or("no video formats")?;
        let fr = fmt.frames.iter().find(|y| y.index == frame)
            .or(fmt.frames.iter().find(|y| y.index == fmt.default_frame)).or(fmt.frames.first())
            .ok_or("format without frames")?;
        let _ = f.set_interface(v.vs_iface, 0);
        let neg = negotiate(f, v, fmt.index, fr.index, fr.interval).map_err(|_| "probe/commit failed")?;
        let mut alts: Vec<&IsochEpDesc> = v.alts.iter().collect();
        alts.sort_by_key(|e| alt_bytes(e));
        let alt = alts.iter().find(|e| alt_bytes(e) >= neg.max_payload).or(alts.last())
            .ok_or("no isochronous alternate setting")?;
        usb_log!("[UVC] slot {}: format {} frame {} ({}x{}) interval {} max_frame {} max_payload {} -> alt {} ({} B/interval)\n",
                 f.slot, neg.format, neg.frame, fr.width, fr.height, neg.interval, neg.max_frame,
                 neg.max_payload, alt.alt, alt_bytes(alt));
        let pkt_max = alt_bytes(alt) as usize;
        // 240 TDs: 30 ms at one per 125 µs microframe, more than a 10 ms tick.
        let pipe = IsochPipe::open_ex(f, alt, StreamFormat { rate: 0, frame_bytes: 0 }, 1 << 20, 240, true)?;
        Ok(Stream {
            pipe: Some(pipe), fake: None, bufs, queued: Vec::with_capacity(32), done: Vec::with_capacity(32), cur: None,
            last_fid: 0xFF, seq: 0, pkt: vec![0u8; pkt_max.max(64)].into_boxed_slice(), neg,
            stats: StreamStats::default(), gone: false,
        })
    }

    /// The fake camera's stream (`uvc.fake=1`): no hardware, `poll` synthesizes frames.
    pub fn start_fake(v: &VideoInfo, format: u8, frame: u8, bufs: Vec<Buf>) -> Result<Stream, &'static str> {
        let fmt = v.formats.iter().find(|x| x.index == format).or(v.formats.first()).ok_or("no video formats")?;
        let fr = fmt.frames.iter().find(|y| y.index == frame).or(fmt.frames.first()).ok_or("format without frames")?;
        let neg = Negotiated { format: fmt.index, frame: fr.index, interval: fr.interval, max_frame: fr.max_size, max_payload: 0 };
        Ok(Stream {
            pipe: None, fake: Some((fmt.kind, 0)), bufs, queued: Vec::with_capacity(32), done: Vec::with_capacity(32),
            cur: None, last_fid: 0xFF, seq: 0, pkt: Vec::new().into_boxed_slice(), neg,
            stats: StreamStats::default(), gone: false,
        })
    }

    fn fake_poll(&mut self, kind: u8) {
        // The USB tick is 10 ms: one frame every 3 ticks is ~33 fps.
        let Some((_, n)) = self.fake.as_mut() else { return };
        *n += 1;
        if *n < 3 || self.queued.is_empty() || self.done.len() >= self.done.capacity() { return; }
        *n = 0;
        let i = self.queued.remove(0);
        let buf = self.bufs[i];
        let len = if kind == FMT_MJPEG {
            let j = FAKE_JPEG;
            let k = j.len().min(buf.cap);
            unsafe { core::ptr::copy_nonoverlapping(j.as_ptr(), buf.ptr, k); }
            // Trailing bytes after EOI are ignored by decoders; they make frames differ.
            let pad = ((self.seq % 8) as usize).min(buf.cap - k);
            for o in 0..pad { unsafe { *buf.ptr.add(k + o) = 0; } }
            k + pad
        } else {
            let k = (self.neg.max_frame as usize).min(buf.cap);
            for o in 0..k { unsafe { *buf.ptr.add(o) = (o as u32).wrapping_add(self.seq) as u8; } }
            k
        };
        self.stats.payloads += 1;
        self.stats.frames += 1;
        let d = Done { index: i, bytes: len, seq: self.seq, ts_ns: platform::now_ns(), error: false };
        self.seq = self.seq.wrapping_add(1);
        self.done.push(d);
    }

    pub fn nbufs(&self) -> usize { self.bufs.len() }
    pub fn is_gone(&self) -> bool { self.gone }

    /// Give buffer `i` to the driver.
    pub fn qbuf(&mut self, i: usize) -> bool {
        if i >= self.bufs.len() || self.queued.contains(&i) || self.queued.len() >= self.queued.capacity() { return false; }
        if matches!(self.cur, Some((Some(b), _, _)) if b == i) { return false; }
        self.queued.push(i);
        true
    }

    /// Take the oldest completed frame.
    pub fn dqbuf(&mut self) -> Option<Done> {
        if self.done.is_empty() { None } else { Some(self.done.remove(0)) }
    }
    pub fn ready(&self) -> usize { self.done.len() }

    fn finish(&mut self) {
        let Some((b, n, err)) = self.cur.take() else { return };
        let Some(i) = b else { return };
        if n == 0 {
            // Nothing landed: the buffer goes back to the front of the queue.
            self.queued.insert(0, i);
            return;
        }
        self.stats.frames += 1;
        if err { self.stats.errors += 1; }
        let d = Done { index: i, bytes: n, seq: self.seq, ts_ns: platform::now_ns(), error: err };
        self.seq = self.seq.wrapping_add(1);
        if self.done.len() < self.done.capacity() { self.done.push(d); } else { self.queued.push(i); }
    }

    fn payload(&mut self, n: usize) {
        if n < 2 { return; }
        let hl = self.pkt[0] as usize;
        let info = self.pkt[1];
        if hl < 2 || hl > n { return; }
        self.stats.payloads += 1;
        let fid = info & 1;
        if self.cur.is_some() && self.last_fid != 0xFF && fid != self.last_fid { self.finish(); }
        self.last_fid = fid;
        let data_len = n - hl;
        if self.cur.is_none() && data_len > 0 {
            let b = if self.queued.is_empty() { self.stats.dropped += 1; None } else { Some(self.queued.remove(0)) };
            self.cur = Some((b, 0, false));
        }
        if let Some((b, used, err)) = self.cur.as_mut() {
            if info & 0x40 != 0 { *err = true; }
            if let Some(i) = *b {
                let buf = self.bufs[i];
                let k = data_len.min(buf.cap.saturating_sub(*used));
                if k < data_len { *err = true; }
                unsafe { core::ptr::copy_nonoverlapping(self.pkt.as_ptr().add(hl), buf.ptr.add(*used), k); }
                *used += k;
            }
        }
        if info & 0x02 != 0 { self.finish(); }
    }

    /// Harvest the pipe and reassemble frames. Tick-safe (try-lock only).
    pub fn poll(&mut self) {
        if self.gone { return; }
        if let Some((kind, _)) = self.fake { self.fake_poll(kind); return; }
        let Some(pipe) = self.pipe.as_mut() else { return };
        if pipe.poll() == Poll::Gone { self.gone = true; return; }
        loop {
            let mut pkt = core::mem::take(&mut self.pkt);
            let r = self.pipe.as_mut().unwrap().read_packet(&mut pkt);
            self.pkt = pkt;
            match r {
                Some(n) => self.payload(n.min(self.pkt.len())),
                None => break,
            }
        }
    }

    /// Stop streaming (alternate setting 0); returns the buffers. Task context.
    pub fn stop(self) -> Vec<Buf> {
        if let Some(p) = self.pipe { p.close(); }
        self.bufs
    }
}

// ── the registry ────────────────────────────────────────────────────────────

/// A camera found at enumeration.
pub struct Camera {
    /// None for the `uvc.fake=1` camera.
    pub func: Option<UsbFunction>,
    pub info: VideoInfo,
}

pub static CAMERAS: Mutex<Vec<Arc<Camera>>> = Mutex::new(Vec::new());
static FAKE_CAMERA: AtomicU32 = AtomicU32::new(0);
const FAKE_JPEG: &[u8] = include_bytes!("fake_cam.jpg");
static SELFTEST_FRAMES: AtomicU32 = AtomicU32::new(0);

/// Parse `uvc.selftest=<frames>`.
pub fn set_cmdline(cmdline: &str) {
    for tok in cmdline.split_ascii_whitespace() {
        if tok == "uvc.fake=1" { FAKE_CAMERA.store(1, Ordering::Relaxed); }
        if let Some(v) = tok.strip_prefix("uvc.selftest=") {
            if let Ok(n) = v.parse::<u32>() { SELFTEST_FRAMES.store(n, Ordering::Relaxed); }
        }
    }
}

fn fmt_name(k: u8) -> &'static str {
    match k { FMT_MJPEG => "MJPEG", FMT_YUYV => "YUYV", FMT_NV12 => "NV12", _ => "other" }
}

/// Log the descriptors of a video function (the lsusb -v equivalent).
pub fn dump(slot: u8, v: &VideoInfo) {
    usb_log!("[UVC] slot {}: UVC {:x}.{:02x}, VC iface {}, VS iface {}, {} format(s), {} isoch alt(s)\n",
             slot, v.bcd_uvc >> 8, v.bcd_uvc & 0xFF, v.vc_iface, v.vs_iface, v.formats.len(), v.alts.len());
    for f in &v.formats {
        usb_log!("[UVC]   format {} {} bpp {} default frame {}\n", f.index, fmt_name(f.kind), f.bpp, f.default_frame);
        for fr in &f.frames {
            usb_log!("[UVC]     frame {} {}x{} max {} B interval {} (x100ns)\n",
                     fr.index, fr.width, fr.height, fr.max_size, fr.interval);
        }
    }
    for a in &v.alts {
        usb_log!("[UVC]   alt {} ep {:02x} mps {:#06x} ({} B/interval) bInterval {}\n",
                 a.alt, a.ep_addr, a.mps, alt_bytes(a), a.interval);
    }
}

/// Hand an enumerated video function to the registry. Task context.
pub fn register(f: UsbFunction) {
    let Some(info) = parse(&f.cfg) else { return };
    dump(f.slot, &info);
    let cam = Arc::new(Camera { func: Some(f), info });
    let n = SELFTEST_FRAMES.load(Ordering::Relaxed);
    if n != 0 { selftest(&cam, n); }
    CAMERAS.lock().push(cam);
}

/// Register the fake camera when `uvc.fake=1` was given (MJPG and YUYV 640x480).
pub fn register_fake() {
    if FAKE_CAMERA.load(Ordering::Relaxed) == 0 { return; }
    let fr = FrameDesc { index: 1, width: 640, height: 480, max_size: 614400, interval: 333333 };
    let info = VideoInfo {
        bcd_uvc: 0x0100, vc_iface: 0, vs_iface: 1,
        formats: alloc::vec![
            FormatDesc { index: 1, kind: FMT_MJPEG, bpp: 0, default_frame: 1, frames: alloc::vec![fr] },
            FormatDesc { index: 2, kind: FMT_YUYV, bpp: 16, default_frame: 1, frames: alloc::vec![fr] },
        ],
        alts: Vec::new(),
    };
    usb_log!("[UVC] fake camera registered (uvc.fake=1)\n");
    CAMERAS.lock().push(Arc::new(Camera { func: None, info }));
}

/// The first camera, if any.
pub fn camera(i: usize) -> Option<Arc<Camera>> { CAMERAS.lock().get(i).cloned() }

/// The device on (`hc`, `slot`) left.
pub fn detach(hc: &crate::xhci::Hc, slot: u8) {
    CAMERAS.lock().retain(|c| !c.func.as_ref().map_or(false, |f| core::ptr::eq(&*f.hc, hc) && f.slot == slot));
}

static PUMP: AtomicUsize = AtomicUsize::new(0);

/// Install the stream pump the USB timer tick runs (the kernel's V4L2 layer).
pub fn set_pump(f: fn()) { PUMP.store(f as usize, Ordering::Release); }

/// USB timer tick (IRQ context): run the installed pump, if any.
pub fn tick() {
    let p = PUMP.load(Ordering::Acquire);
    if p != 0 {
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
}

/// FNV-1a over `d`.
pub fn fnv1a(d: &[u8]) -> u32 {
    d.iter().fold(0x811C_9DC5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

fn selftest(cam: &Camera, frames: u32) {
    let fmt = cam.info.formats.iter().find(|x| x.kind == FMT_MJPEG).or(cam.info.formats.first());
    let Some(fmt) = fmt else { return };
    let cap = fmt.frames.iter().map(|y| y.max_size as usize).max().unwrap_or(0).clamp(64 * 1024, 8 << 20);
    let mut store: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; cap]).collect();
    let bufs = store.iter_mut().map(|b| Buf { ptr: b.as_mut_ptr(), cap }).collect();
    let mut s = match Stream::start(cam.func.as_ref().unwrap(), &cam.info, fmt.index, 0, bufs) {
        Ok(s) => s,
        Err(e) => { usb_log!("[UVC] selftest: start failed: {}\n", e); return; }
    };
    for i in 0..4 { s.qbuf(i); }
    let dl = Deadline::after_ms(3000 + frames * 100);
    let mut got = 0u32;
    while got < frames && !dl.expired() && !s.is_gone() {
        s.poll();
        while let Some(d) = s.dqbuf() {
            let b = &store[d.index][..d.bytes];
            usb_log!("[UVC] frame {} seq {} bytes {} fnv {:08x} head {:02x}{:02x} err {}\n",
                     got, d.seq, d.bytes, fnv1a(b), b[0], b.get(1).copied().unwrap_or(0), d.error as u8);
            got += 1;
            s.qbuf(d.index);
        }
        platform::relax();
    }
    let st = s.stats;
    let ps = s.pipe.as_ref().unwrap().local;
    let _ = s.stop();
    usb_log!("[UVC] selftest: {} frame(s), payloads {} dropped {} errors {}, isoch tds {} missed {} underrun {} errors {}\n",
             got, st.payloads, st.dropped, st.errors, ps.tds, ps.missed, ps.underrun, ps.errors);
    drop(store);
}
