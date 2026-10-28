//! V4L2 video capture for UVC cameras (a child module of `syscall`, kept in
//! its own file like `syncfile_ioctl`).
//!
//! /dev/video0 (char 81:0, root:video 0660) is a dynamic device whose port has
//! a kernel handler (`handle`): VFS_OPEN (ENODEV without a camera, EBUSY when
//! already open), VFS_POLL (POLLIN when a frame is ready, POLLERR when not
//! streaming or unplugged), VFS_CLOSE (stops the stream). The `'V'` ioctls are
//! answered in the syscall layer (`intercept`), which reads and writes the
//! caller's structs directly; mmap is answered by `mmap_phys` from `sys_mmap`.
//!
//! Structs follow linux/videodev2.h on LP64 (both arches), so stock
//! libv4l/PipeWire can drive the node. Subset: QUERYCAP, ENUM_FMT,
//! ENUM_FRAMESIZES, ENUM_FRAMEINTERVALS, G/S/TRY_FMT, G/S_PARM, ENUMINPUT,
//! G/S_INPUT, REQBUFS, QUERYBUF, QBUF, DQBUF, STREAMON, STREAMOFF; MMAP memory
//! only, one discrete frame interval per frame size (the device default).
//!
//! Buffers: physically contiguous buddy blocks sized for the largest frame of
//! the camera, allocated on the first REQBUFS and kept for the life of the
//! kernel (a process may still have them mapped after close; they are never
//! handed back to the allocator, so a stale mapping can only see old frames).
//! mmap offset of buffer i = i << 24.
//!
//! The stream (`usb::uvc::Stream`) is pumped from the USB timer tick
//! (`usb::uvc::tick` -> `pump`, try-lock only, IRQ-safe wake) and also on
//! every DQBUF/poll in task context.

use super::*;
use alloc::sync::Arc;
use spin::Mutex;
use usb::uvc::{self, Buf, Camera, FormatDesc, FrameDesc, Stream, FMT_MJPEG, FMT_NV12, FMT_YUYV};

pub const VIDEO_MAJOR: u32 = 81;

const VFS_OPEN: u64 = 0x10;
const VFS_CLOSE: u64 = 0x13;
const VFS_POLL: u64 = 0x2F;

const EINTR: isize = -4;
const EIO: isize = -5;
const EAGAIN: isize = -11;
const ENOMEM: isize = -12;
const EFAULT: isize = -14;
const EBUSY: isize = -16;
const ENODEV: isize = -19;
const EINVAL: isize = -22;
const ENOTTY: isize = -25;

const VIDIOC_QUERYCAP: usize = 0x8068_5600;
const VIDIOC_ENUM_FMT: usize = 0xC040_5602;
const VIDIOC_G_FMT: usize = 0xC0D0_5604;
const VIDIOC_S_FMT: usize = 0xC0D0_5605;
const VIDIOC_REQBUFS: usize = 0xC014_5608;
const VIDIOC_QUERYBUF: usize = 0xC058_5609;
const VIDIOC_QBUF: usize = 0xC058_560F;
const VIDIOC_DQBUF: usize = 0xC058_5611;
const VIDIOC_STREAMON: usize = 0x4004_5612;
const VIDIOC_STREAMOFF: usize = 0x4004_5613;
const VIDIOC_G_PARM: usize = 0xC0CC_5615;
const VIDIOC_S_PARM: usize = 0xC0CC_5616;
const VIDIOC_ENUMINPUT: usize = 0xC050_561A;
const VIDIOC_G_INPUT: usize = 0x8004_5626;
const VIDIOC_S_INPUT: usize = 0xC004_5627;
const VIDIOC_TRY_FMT: usize = 0xC0D0_5640;
const VIDIOC_ENUM_FRAMESIZES: usize = 0xC02C_564A;
const VIDIOC_ENUM_FRAMEINTERVALS: usize = 0xC034_564B;

const BUF_TYPE_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FIELD_NONE: u32 = 1;
const CAP_VIDEO_CAPTURE: u32 = 0x1;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const BUF_FLAG_QUEUED: u32 = 0x2;
const BUF_FLAG_DONE: u32 = 0x4;
const BUF_FLAG_ERROR: u32 = 0x40;
const BUF_FLAG_TIMESTAMP_MONOTONIC: u32 = 0x2000;
const BUF_CAP_SUPPORTS_MMAP: u32 = 0x1;
const FMT_FLAG_COMPRESSED: u32 = 0x1;
const CAP_TIMEPERFRAME: u32 = 0x1000;
const COLORSPACE_JPEG: u32 = 7;
const COLORSPACE_SRGB: u32 = 8;

const POLLIN: u32 = 0x1;
const POLLERR: u32 = 0x8;
const POLLRDNORM: u32 = 0x40;

const MAX_BUFS: usize = 8;
const OFF_SHIFT: usize = 24;

struct Dev {
    owner: u32,
    cam: Option<Arc<Camera>>,
    fmt: u8,
    frame: u8,
    nreq: usize,
    pending: [bool; MAX_BUFS],
    queued: [bool; MAX_BUFS],
    streaming: bool,
}

struct Pool { cap: usize, order: usize, phys: [usize; MAX_BUFS], n: usize }

static DEV: Mutex<Dev> = Mutex::new(Dev {
    owner: 0, cam: None, fmt: 0, frame: 0, nreq: 0,
    pending: [false; MAX_BUFS], queued: [false; MAX_BUFS], streaming: false,
});
static POOL: Mutex<Pool> = Mutex::new(Pool { cap: 0, order: 0, phys: [0; MAX_BUFS], n: 0 });
static STREAM: Mutex<Option<Stream>> = Mutex::new(None);
static SEQ: AtomicU64 = AtomicU64::new(0);
static WAKE_PENDING: AtomicBool = AtomicBool::new(false);

fn poll_key() -> u64 { sched::poll_key(sched::poll_class::VIDEO, 0) }

// ── registration, tick, VFS handler ─────────────────────────────────────────

/// Create /dev/video0 and hook the USB tick. Called once at boot.
pub(crate) fn init() {
    let Some(port) = ipc::port::create(0) else { return };
    if !ipc::port::register_kernel_handler(port, handle) { return; }
    vfs::register_device_notify_open("/dev/video0", port, 0, VIDEO_MAJOR, 0);
    uvc::set_pump(pump);
}

/// USB timer tick (IRQ context): reassemble frames, wake pollers.
fn pump() {
    if let Some(mut g) = STREAM.try_lock() {
        if let Some(s) = g.as_mut() {
            let before = s.ready();
            s.poll();
            if s.ready() > before || s.is_gone() { WAKE_PENDING.store(true, Ordering::Release); }
        }
    }
    if WAKE_PENDING.load(Ordering::Acquire) {
        SEQ.fetch_add(1, Ordering::AcqRel);
        if sched::try_wake_poll_key(poll_key()) { WAKE_PENDING.store(false, Ordering::Release); }
    }
}

fn msg_arg(msg: &Message, n: usize) -> u64 {
    u64::from_le_bytes(msg.data[n * 8..n * 8 + 8].try_into().unwrap_or([0u8; 8]))
}

fn reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

fn default_format(cam: &Camera) -> (u8, u8) {
    let f = cam.info.formats.iter().find(|f| f.kind == FMT_MJPEG)
        .or(cam.info.formats.iter().find(|f| fourcc(f.kind) != 0));
    match f { Some(f) => (f.index, f.default_frame), None => (0, 0) }
}

fn handle(msg: &Message, _caller: u32, _target: u32) -> Message {
    match msg.tag {
        VFS_OPEN => {
            let id = match msg_arg(msg, 4) as u32 { 0 => u32::MAX, v => v };
            let Some(cam) = uvc::camera(0) else { return reply(ENODEV as i64) };
            if default_format(&cam).0 == 0 { return reply(ENODEV as i64); }
            let mut d = DEV.lock();
            if d.owner != 0 { return reply(EBUSY as i64); }
            let (f, fr) = default_format(&cam);
            *d = Dev { owner: id, cam: Some(cam), fmt: f, frame: fr, nreq: 0,
                       pending: [false; MAX_BUFS], queued: [false; MAX_BUFS], streaming: false };
            reply(0)
        }
        VFS_POLL => {
            let revents = {
                let mut g = STREAM.lock();
                match g.as_mut() {
                    Some(s) => {
                        s.poll();
                        if s.is_gone() { POLLERR } else if s.ready() > 0 { POLLIN | POLLRDNORM } else { 0 }
                    }
                    None => POLLERR,
                }
            };
            let key = poll_key();
            let mut m = Message::empty();
            m.data[0..8].copy_from_slice(&(revents as u64).to_le_bytes());
            m.data[8..16].copy_from_slice(&SEQ.load(Ordering::Acquire).to_le_bytes());
            m.data[24..32].copy_from_slice(&sched::poll_key_bit(key).to_le_bytes());
            m.data[32] = 1;
            sched::poll_reply_put_keys(&mut m.data, &[key]);
            m
        }
        VFS_CLOSE => {
            let id = match msg_arg(msg, 1) as u32 { 0 => u32::MAX, v => v };
            let mut d = DEV.lock();
            if d.owner == id {
                stream_off(&mut d);
                d.owner = 0;
                d.cam = None;
                d.nreq = 0;
            }
            reply(0)
        }
        _ => reply(-38),
    }
}

/// mmap of `/dev/video0` at `off` for `len` bytes: the buffer's physical address.
pub(super) fn mmap_phys(open_id: u32, off: usize, len: usize) -> Result<usize, isize> {
    let id = if open_id == 0 { u32::MAX } else { open_id };
    let d = DEV.lock();
    if d.owner != id { return Err(EBUSY); }
    let i = off >> OFF_SHIFT;
    if off & ((1 << OFF_SHIFT) - 1) != 0 || i >= d.nreq { return Err(EINVAL); }
    let p = POOL.lock();
    if i >= p.n || len == 0 || len > (4096usize << p.order) { return Err(EINVAL); }
    Ok(p.phys[i])
}

// ── ioctls ──────────────────────────────────────────────────────────────────

/// Answer a `'V'` ioctl on /dev/video0; `None` = not ours.
pub(super) fn intercept(pid: u32, fd: usize, cmd: usize, arg: usize) -> Option<isize> {
    if (cmd >> 8) & 0xFF != 0x56 { return None; }
    let Some(vfs::VnodeKind::DynamicDevice { port, dev_id, open_id }) = vfs::vfs_get_node_kind(pid, fd) else { return None };
    if vfs::dynamic_device_major(port, dev_id) != Some(VIDEO_MAJOR) { return None; }
    let id = if open_id == 0 { u32::MAX } else { open_id };
    Some(ioctl(pid, fd, id, cmd, arg))
}

fn rd<const N: usize>(arg: usize) -> Result<[u8; N], isize> {
    let mut b = [0u8; N];
    if unsafe { ua::copy_from_user(b.as_mut_ptr(), arg, N) } != 0 { Err(EFAULT) } else { Ok(b) }
}
fn wr(arg: usize, b: &[u8]) -> isize {
    if unsafe { ua::copy_to_user(arg, b.as_ptr(), b.len()) } != 0 { EFAULT } else { 0 }
}
fn g32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn p32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
fn p64(b: &mut [u8], o: usize, v: u64) { b[o..o + 8].copy_from_slice(&v.to_le_bytes()); }
fn pstr(b: &mut [u8], o: usize, n: usize, s: &str) {
    let k = s.len().min(n - 1);
    b[o..o + k].copy_from_slice(&s.as_bytes()[..k]);
}

fn fourcc(kind: u8) -> u32 {
    match kind { FMT_MJPEG => 0x4750_4A4D, FMT_YUYV => 0x5659_5559, FMT_NV12 => 0x3231_564E, _ => 0 }
}
fn fmt_desc(kind: u8) -> &'static str {
    match kind { FMT_MJPEG => "Motion-JPEG", FMT_YUYV => "YUYV 4:2:2", _ => "Y/UV 4:2:0" }
}
fn formats(cam: &Camera) -> impl Iterator<Item = &FormatDesc> {
    cam.info.formats.iter().filter(|f| fourcc(f.kind) != 0)
}
fn find_fmt(cam: &Camera, pixfmt: u32) -> Option<&FormatDesc> {
    formats(cam).find(|f| fourcc(f.kind) == pixfmt)
}
fn sizeimage(f: &FormatDesc, fr: &FrameDesc) -> u32 {
    let (w, h) = (fr.width as u32, fr.height as u32);
    match f.kind { FMT_YUYV => w * h * 2, FMT_NV12 => w * h * 3 / 2, _ => fr.max_size.max(4096) }
}
fn gcd(a: u32, b: u32) -> u32 { if b == 0 { a.max(1) } else { gcd(b, a % b) } }
fn fract(interval: u32) -> (u32, u32) {
    let (n, d) = (interval.max(1), 10_000_000);
    let g = gcd(n, d);
    (n / g, d / g)
}
/// Fill a `struct v4l2_pix_format` at `o`.
fn pix_fill(b: &mut [u8], o: usize, f: &FormatDesc, fr: &FrameDesc) {
    for x in &mut b[o..o + 48] { *x = 0; }
    p32(b, o, fr.width as u32);
    p32(b, o + 4, fr.height as u32);
    p32(b, o + 8, fourcc(f.kind));
    p32(b, o + 12, FIELD_NONE);
    let bpl = match f.kind { FMT_YUYV => fr.width as u32 * 2, FMT_NV12 => fr.width as u32, _ => 0 };
    p32(b, o + 16, bpl);
    p32(b, o + 20, sizeimage(f, fr));
    p32(b, o + 24, if f.kind == FMT_MJPEG { COLORSPACE_JPEG } else { COLORSPACE_SRGB });
}
fn cur(d: &Dev) -> Option<(&FormatDesc, &FrameDesc)> {
    let cam = d.cam.as_ref()?;
    let f = cam.info.formats.iter().find(|f| f.index == d.fmt)?;
    let fr = f.frames.iter().find(|y| y.index == d.frame).or(f.frames.first())?;
    Some((f, fr))
}
/// Fill a `struct v4l2_buffer` (88 bytes).
fn buf_fill(b: &mut [u8; 88], i: usize, bytes: u32, flags: u32, seq: u32, ts_ns: u64, len: u32) {
    *b = [0u8; 88];
    p32(b, 0, i as u32);
    p32(b, 4, BUF_TYPE_CAPTURE);
    p32(b, 8, bytes);
    p32(b, 12, flags | BUF_FLAG_TIMESTAMP_MONOTONIC);
    p32(b, 16, FIELD_NONE);
    p64(b, 24, ts_ns / 1_000_000_000);
    p64(b, 32, (ts_ns % 1_000_000_000) / 1000);
    p32(b, 56, seq);
    p32(b, 60, MEMORY_MMAP);
    p64(b, 64, (i << OFF_SHIFT) as u64);
    p32(b, 72, len);
}

/// Make sure the pool holds `n` buffers of at least `cap` bytes.
fn pool_ensure(n: usize, cap: usize) -> Result<(), isize> {
    let mut p = POOL.lock();
    if p.cap < cap {
        // Leaked on purpose: an old mapping may still name these frames.
        let pages = (cap + 4095) / 4096;
        let order = (usize::BITS - (pages - 1).leading_zeros()) as usize;
        *p = Pool { cap: 4096 << order, order, phys: [0; MAX_BUFS], n: 0 };
    }
    while p.n < n {
        let phys = mm::buddy::alloc(p.order).ok_or(ENOMEM)?;
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(phys) as *mut u8, 0, p.cap); }
        let k = p.n;
        p.phys[k] = phys;
        p.n += 1;
    }
    Ok(())
}

fn stream_off(d: &mut Dev) {
    let s = STREAM.lock().take();
    if let Some(s) = s { let _ = s.stop(); }
    d.streaming = false;
    d.pending = [false; MAX_BUFS];
    d.queued = [false; MAX_BUFS];
}

fn ioctl(pid: u32, fd: usize, id: u32, cmd: usize, arg: usize) -> isize {
    if cmd == VIDIOC_DQBUF { return dqbuf(pid, fd, id, arg); }
    let mut d = DEV.lock();
    if d.owner != id { return EBUSY; }
    let Some(cam) = d.cam.clone() else { return ENODEV };
    match cmd {
        VIDIOC_QUERYCAP => {
            let mut b = [0u8; 104];
            pstr(&mut b, 0, 16, "uvcvideo");
            let card = alloc::format!("UVC Camera (UVC {:x}.{:02x})", cam.info.bcd_uvc >> 8, cam.info.bcd_uvc & 0xFF);
            pstr(&mut b, 16, 32, &card);
            pstr(&mut b, 48, 32, &alloc::format!("usb-xhci-{}", cam.func.slot));
            p32(&mut b, 80, (6 << 16) | (1 << 8));
            p32(&mut b, 84, CAP_VIDEO_CAPTURE | CAP_STREAMING | CAP_DEVICE_CAPS);
            p32(&mut b, 88, CAP_VIDEO_CAPTURE | CAP_STREAMING);
            wr(arg, &b)
        }
        VIDIOC_ENUM_FMT => {
            let mut b = match rd::<64>(arg) { Ok(b) => b, Err(e) => return e };
            if g32(&b, 4) != BUF_TYPE_CAPTURE { return EINVAL; }
            let Some(f) = formats(&cam).nth(g32(&b, 0) as usize) else { return EINVAL };
            for x in &mut b[8..] { *x = 0; }
            p32(&mut b, 8, if f.kind == FMT_MJPEG { FMT_FLAG_COMPRESSED } else { 0 });
            pstr(&mut b, 12, 32, fmt_desc(f.kind));
            p32(&mut b, 44, fourcc(f.kind));
            wr(arg, &b)
        }
        VIDIOC_ENUM_FRAMESIZES => {
            let mut b = match rd::<44>(arg) { Ok(b) => b, Err(e) => return e };
            let Some(f) = find_fmt(&cam, g32(&b, 4)) else { return EINVAL };
            let Some(fr) = f.frames.get(g32(&b, 0) as usize) else { return EINVAL };
            for x in &mut b[8..] { *x = 0; }
            p32(&mut b, 8, 1); // V4L2_FRMSIZE_TYPE_DISCRETE
            p32(&mut b, 12, fr.width as u32);
            p32(&mut b, 16, fr.height as u32);
            wr(arg, &b)
        }
        VIDIOC_ENUM_FRAMEINTERVALS => {
            let mut b = match rd::<52>(arg) { Ok(b) => b, Err(e) => return e };
            let Some(f) = find_fmt(&cam, g32(&b, 4)) else { return EINVAL };
            let (w, h) = (g32(&b, 8), g32(&b, 12));
            let Some(fr) = f.frames.iter().find(|y| y.width as u32 == w && y.height as u32 == h) else { return EINVAL };
            if g32(&b, 0) != 0 { return EINVAL; }
            for x in &mut b[16..] { *x = 0; }
            let (n, dd) = fract(fr.interval);
            p32(&mut b, 16, 1); // V4L2_FRMIVAL_TYPE_DISCRETE
            p32(&mut b, 20, n);
            p32(&mut b, 24, dd);
            wr(arg, &b)
        }
        VIDIOC_G_FMT => {
            let mut b = match rd::<208>(arg) { Ok(b) => b, Err(e) => return e };
            if g32(&b, 0) != BUF_TYPE_CAPTURE { return EINVAL; }
            let Some((f, fr)) = cur(&d) else { return EINVAL };
            pix_fill(&mut b, 8, f, fr);
            wr(arg, &b)
        }
        VIDIOC_S_FMT | VIDIOC_TRY_FMT => {
            let mut b = match rd::<208>(arg) { Ok(b) => b, Err(e) => return e };
            if g32(&b, 0) != BUF_TYPE_CAPTURE { return EINVAL; }
            let f = find_fmt(&cam, g32(&b, 16)).or(find_fmt(&cam, fourcc(FMT_MJPEG))).or(formats(&cam).next());
            let Some(f) = f else { return EINVAL };
            let (w, h) = (g32(&b, 8) as i64, g32(&b, 12) as i64);
            let fr = f.frames.iter().min_by_key(|y| (y.width as i64 - w).abs() + (y.height as i64 - h).abs());
            let Some(fr) = fr else { return EINVAL };
            if cmd == VIDIOC_S_FMT {
                // The pool is sized for the largest frame, so only streaming blocks a change.
                if d.streaming { return EBUSY; }
                d.fmt = f.index;
                d.frame = fr.index;
            }
            pix_fill(&mut b, 8, f, fr);
            wr(arg, &b)
        }
        VIDIOC_G_PARM | VIDIOC_S_PARM => {
            let mut b = match rd::<204>(arg) { Ok(b) => b, Err(e) => return e };
            if g32(&b, 0) != BUF_TYPE_CAPTURE { return EINVAL; }
            let Some((_, fr)) = cur(&d) else { return EINVAL };
            for x in &mut b[4..] { *x = 0; }
            let (n, dd) = fract(fr.interval);
            p32(&mut b, 4, CAP_TIMEPERFRAME);
            p32(&mut b, 12, n);
            p32(&mut b, 16, dd);
            wr(arg, &b)
        }
        VIDIOC_ENUMINPUT => {
            let mut b = match rd::<80>(arg) { Ok(b) => b, Err(e) => return e };
            if g32(&b, 0) != 0 { return EINVAL; }
            for x in &mut b[4..] { *x = 0; }
            pstr(&mut b, 4, 32, "Camera 1");
            p32(&mut b, 36, 2); // V4L2_INPUT_TYPE_CAMERA
            wr(arg, &b)
        }
        VIDIOC_G_INPUT => wr(arg, &0u32.to_le_bytes()),
        VIDIOC_S_INPUT => {
            let b = match rd::<4>(arg) { Ok(b) => b, Err(e) => return e };
            if g32(&b, 0) != 0 { EINVAL } else { 0 }
        }
        VIDIOC_REQBUFS => {
            let mut b = match rd::<20>(arg) { Ok(b) => b, Err(e) => return e };
            if g32(&b, 4) != BUF_TYPE_CAPTURE || g32(&b, 8) != MEMORY_MMAP { return EINVAL; }
            if d.streaming { return EBUSY; }
            let n = (g32(&b, 0) as usize).min(MAX_BUFS);
            if n > 0 {
                let n = n.max(2);
                let cap = formats(&cam).flat_map(|f| f.frames.iter().map(move |y| sizeimage(f, y) as usize))
                    .max().unwrap_or(0).clamp(64 * 1024, 8 << 20);
                if let Err(e) = pool_ensure(n, cap) { return e; }
                d.nreq = n;
            } else {
                d.nreq = 0;
            }
            d.pending = [false; MAX_BUFS];
            d.queued = [false; MAX_BUFS];
            p32(&mut b, 0, d.nreq as u32);
            p32(&mut b, 12, BUF_CAP_SUPPORTS_MMAP);
            wr(arg, &b)
        }
        VIDIOC_QUERYBUF | VIDIOC_QBUF => {
            let mut b = match rd::<88>(arg) { Ok(b) => b, Err(e) => return e };
            let i = g32(&b, 0) as usize;
            if g32(&b, 4) != BUF_TYPE_CAPTURE || i >= d.nreq { return EINVAL; }
            let len = POOL.lock().cap as u32;
            if cmd == VIDIOC_QBUF {
                if g32(&b, 60) != MEMORY_MMAP || d.queued[i] || d.pending[i] { return EINVAL; }
                if d.streaming {
                    let ok = STREAM.lock().as_mut().map(|s| s.qbuf(i)).unwrap_or(false);
                    if !ok { return EINVAL; }
                    d.queued[i] = true;
                } else {
                    d.pending[i] = true;
                }
            }
            let flags = if d.queued[i] || d.pending[i] { BUF_FLAG_QUEUED } else { 0 };
            buf_fill(&mut b, i, 0, flags, 0, 0, len);
            wr(arg, &b)
        }
        VIDIOC_STREAMON => {
            let b = match rd::<4>(arg) { Ok(b) => b, Err(e) => return e };
            if g32(&b, 0) != BUF_TYPE_CAPTURE || d.nreq == 0 { return EINVAL; }
            if d.streaming { return 0; }
            let bufs = {
                let p = POOL.lock();
                (0..d.nreq).map(|i| Buf { ptr: mm::phys_to_virt(p.phys[i]) as *mut u8, cap: p.cap }).collect()
            };
            let mut s = match Stream::start(&cam.func, &cam.info, d.fmt, d.frame, bufs) {
                Ok(s) => s,
                Err(_) => return EIO,
            };
            for i in 0..d.nreq {
                if d.pending[i] && s.qbuf(i) { d.queued[i] = true; }
            }
            d.pending = [false; MAX_BUFS];
            *STREAM.lock() = Some(s);
            d.streaming = true;
            0
        }
        VIDIOC_STREAMOFF => {
            let b = match rd::<4>(arg) { Ok(b) => b, Err(e) => return e };
            if g32(&b, 0) != BUF_TYPE_CAPTURE { return EINVAL; }
            stream_off(&mut d);
            0
        }
        _ => ENOTTY,
    }
}

fn dqbuf(pid: u32, fd: usize, id: u32, arg: usize) -> isize {
    let mut b = match rd::<88>(arg) { Ok(b) => b, Err(e) => return e };
    if g32(&b, 4) != BUF_TYPE_CAPTURE { return EINVAL; }
    {
        let d = DEV.lock();
        if d.owner != id { return EBUSY; }
        if !d.streaming { return EINVAL; }
    }
    let nonblock = vfs::fd_nonblock(pid, fd);
    let deadline = monotonic_ns() + 3_000_000_000;
    loop {
        let got = {
            let mut g = STREAM.lock();
            let Some(s) = g.as_mut() else { return EINVAL };
            s.poll();
            if s.is_gone() { return ENODEV; }
            s.dqbuf()
        };
        if let Some(done) = got {
            DEV.lock().queued[done.index] = false;
            let len = POOL.lock().cap as u32;
            let flags = BUF_FLAG_DONE | if done.error { BUF_FLAG_ERROR } else { 0 };
            buf_fill(&mut b, done.index, done.bytes as u32, flags, done.seq, done.ts_ns, len);
            return wr(arg, &b);
        }
        if nonblock { return EAGAIN; }
        if monotonic_ns() > deadline { return EIO; }
        if sleep_until_ns(deadline_after_ns(1_000_000), 0, true) < 0 { return EINTR; }
    }
}
