//! PipeWire server for LeandrOS.
//!
//! Provides a minimal PipeWire-compatible IPC interface over AF_UNIX sockets.
//!
//! The playback device is single-writer, like an ALSA hw PCM without dmix:
//! one spool feeds one virtio-sound stream, so two producers would interleave
//! their samples into garbage. Whoever holds it — an open of /dev/pipewire for
//! writing (the PipeWire session's leandros-snd-sink), or a process speaking
//! the legacy IPC protocol below (aplay, MAME, doom on the serial console) —
//! keeps it until that open's last fd closes or that process exits; everyone
//! else gets EBUSY, at open(2) for the device node and as the reply to
//! SET_PARAMS/PCM for an IPC client. Mixing several programs is PipeWire's
//! job; a console player run while a session plays sound is refused, exactly
//! as `aplay -D hw:0` is refused on Linux while PipeWire holds the card.
//!
//! Access follows the node: /dev/pipewire is 116:16 root:audio 0660 (the VFS
//! enforces it at open), and an IPC client must pass the same check — it has
//! to be root or in the `audio` group.

#![no_std]

extern crate alloc;

use ipc::{Message, port};
use drivers::snd::VirtioSnd;
use drivers::hda::Hda;
mod hdmi_ops;
use drivers::Driver;
use drivers::pci;
use kmod_abi::{KSndOps, KSND_CAPTURE};

/// The playback device behind the spool: virtio-sound (QEMU's default) or
/// an Intel HD Audio controller (real PCs; QEMU `--hda`). Same calls, so
/// everything above this enum is backend-agnostic.
enum Backend {
    None,
    Virtio(VirtioSnd),
    Hda(Hda),
    /// A loadable sound module (`kapi_snd_register`): the same calls through `KSndOps`.
    Module { ops: &'static KSndOps, ctx: usize },
}

/// Text from a module callback that fills `(buf, cap) -> len`.
fn module_text(f: impl FnOnce(*mut u8, usize) -> usize) -> alloc::string::String {
    let mut buf = [0u8; 1024];
    let n = f(buf.as_mut_ptr(), buf.len()).min(buf.len());
    alloc::string::String::from_utf8_lossy(&buf[..n]).into_owned()
}

impl Backend {
    fn reconfigure_stream(&mut self, id: u32, freq: u32, ch: u8) {
        match self { Backend::Virtio(d) => d.reconfigure_stream(id, freq, ch),
                     Backend::Hda(d) => d.reconfigure_stream(id, freq, ch),
                     Backend::Module { ops, ctx } => (ops.configure)(*ctx, id, freq, ch),
                     Backend::None => {} }
    }
    fn send_pcm_data(&mut self, data: &[u8]) -> usize {
        match self { Backend::Virtio(d) => d.send_pcm_data(data),
                     Backend::Hda(d) => d.send_pcm_data(data),
                     Backend::Module { ops, ctx } => (ops.write)(*ctx, data.as_ptr(), data.len()),
                     Backend::None => 0 }
    }
    fn tx_used_idx(&self) -> u16 {
        match self { Backend::Virtio(d) => d.tx_used_idx(), Backend::Hda(d) => d.tx_used_idx(),
                     Backend::Module { ops, ctx } => (ops.tx_used_idx)(*ctx) as u16,
                     Backend::None => 0 }
    }
    /// Bytes handed to the device and not yet played.
    fn queued_bytes(&self) -> usize {
        match self {
            Backend::Virtio(d) => d.tx_level() as usize * drivers::snd::TX_BUF_BYTES,
            Backend::Hda(d) => d.queued_bytes() as usize,
            Backend::Module { ops, ctx } => (ops.queued_bytes)(*ctx),
            Backend::None => 0,
        }
    }
    fn top_up_silence(&mut self) {
        match self { Backend::Virtio(d) => d.top_up_silence(), Backend::Hda(d) => d.top_up_silence(),
                     Backend::Module { ops, ctx } => (ops.top_up_silence)(*ctx),
                     Backend::None => {} }
    }
    fn recover_stream(&mut self) {
        match self { Backend::Virtio(d) => d.recover_stream(), Backend::Hda(d) => d.recover_stream(),
                     Backend::Module { ops, ctx } => (ops.recover)(*ctx),
                     Backend::None => {} }
    }
    /// Captured 48 kHz s16le stereo (HD Audio only); Ok(0) = nothing yet.
    fn capture_read(&mut self, out: &mut [u8]) -> Result<usize, i32> {
        match self {
            Backend::Hda(d) => d.capture_read(out),
            Backend::Module { ops, ctx } if ops.flags & KSND_CAPTURE != 0 => {
                let r = (ops.cap_read)(*ctx, out.as_mut_ptr(), out.len());
                if r < 0 { Err(r) } else { Ok((r as usize).min(out.len())) }
            }
            _ => Err(-19), // ENODEV
        }
    }
    fn capture_close(&mut self) {
        match self {
            Backend::Hda(d) => d.capture_close(),
            Backend::Module { ops, ctx } if ops.flags & KSND_CAPTURE != 0 => (ops.cap_close)(*ctx),
            _ => {}
        }
    }
    fn capture_ensure(&mut self) -> Result<(), i32> {
        match self {
            Backend::Hda(d) => d.capture_ensure(),
            Backend::Module { ops, ctx } if ops.flags & KSND_CAPTURE != 0 => {
                match (ops.cap_ensure)(*ctx) { 0 => Ok(()), e => Err(if e > 0 { -e } else { e }) }
            }
            _ => Err(-19),
        }
    }
    fn capture_restart(&mut self) {
        match self {
            Backend::Hda(d) => { d.capture_restart(); }
            Backend::Module { ops, ctx } if ops.flags & KSND_CAPTURE != 0 => (ops.cap_restart)(*ctx),
            _ => {}
        }
    }
    fn capture_pending(&self) -> usize {
        match self {
            Backend::Hda(d) => d.capture_pending(),
            Backend::Module { ops, ctx } if ops.flags & KSND_CAPTURE != 0 => (ops.cap_pending)(*ctx),
            _ => 0,
        }
    }
    fn jack_service(&mut self) {
        match self {
            Backend::Hda(d) => d.jack_service(),
            Backend::Module { ops, ctx } if ops.flags & KSND_CAPTURE != 0 => (ops.jack_service)(*ctx),
            _ => {}
        }
    }
    fn info(&mut self) -> alloc::string::String {
        match self {
            Backend::Virtio(_) => alloc::string::String::from("backend: virtio-sound\n"),
            Backend::Hda(d) => d.info(),
            Backend::Module { ops, ctx } => module_text(|b, c| (ops.info)(*ctx, b, c)),
            Backend::None => alloc::string::String::from("backend: none\n"),
        }
    }
}

/// Last `key=value` of `key` on the kernel command line.
fn cmdline_opt<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    let mut v = None;
    for tok in cmdline.split_ascii_whitespace() {
        if let Some(rest) = tok.strip_prefix(key) {
            if let Some(val) = rest.strip_prefix('=') { v = Some(val); }
        }
    }
    v
}

pub const PW_SOCKET_PATH: &str = "/run/pipewire/pipewire-0";

/// Latency knob: spool depth. With backpressure the spool runs full, so this
/// plus the driver's TX in-flight bytes is the steady-state audio latency
/// (176,400 B ≈ 1 s at 44.1 kHz stereo S16). It is also the cushion that
/// absorbs producer scheduling hiccups — too small and the stream underruns,
/// which QEMU 11.x punishes by killing the voice (recovery = audible gap).
const SPOOL_BYTES: usize = 64 * 1024;

struct PipeWireState {
    snd_driver: Backend,
    bound_port: u32,
    initialized: bool,
    
    // Spooling buffer for non-blocking audio
    spool: [u8; SPOOL_BYTES],
    spool_head: usize,
    spool_tail: usize,
    spool_len: usize,
    last_pcm_us: u64,
}

impl PipeWireState {
    pub const fn new() -> Self {
        Self {
            snd_driver: Backend::None,
            bound_port: 0,
            initialized: false,
            spool: [0u8; SPOOL_BYTES],
            spool_head: 0,
            spool_tail: 0,
            spool_len: 0,
            last_pcm_us: 0,
        }
    }

    fn push_spool(&mut self, data: &[u8]) -> usize {
        let mut total = 0;
        for &b in data {
            if self.spool_len >= self.spool.len() { break; }
            self.spool[self.spool_tail] = b;
            self.spool_tail = (self.spool_tail + 1) % self.spool.len();
            self.spool_len += 1;
            total += 1;
        }
        total
    }

    fn drain_spool(&mut self) {
        if !self.initialized { return; }
        
        while self.spool_len > 0 {
            // Read from spool in chunks of up to TX_BUF_BYTES bytes
            let mut chunk = [0u8; drivers::snd::TX_BUF_BYTES];
            let n = self.spool_len.min(drivers::snd::TX_BUF_BYTES);
            
            // Handle wrapping
            for i in 0..n {
                chunk[i] = self.spool[(self.spool_head + i) % self.spool.len()];
            }
            
            let accepted = self.snd_driver.send_pcm_data(&chunk[..n]);
            // 0 = hardware ring full, or a sub-frame tail that must wait
            // for the producer's next push (the driver only sends whole
            // frames).
            if accepted == 0 { break; }
            
            self.spool_head = (self.spool_head + accepted) % self.spool.len();
            self.spool_len -= accepted;
        }
    }
}

// ── Single-writer arbitration ─────────────────────────────────────────────────

/// Device number of /dev/pipewire: the ALSA major, pcmC0D0p's minor.
const DEV_MAJOR: u32 = vfs_server::SOUND_MAJOR;
const DEV_MINOR: u32 = 16;

/// The holder: 0 = free, `HOLD_OPEN | open_id` = an open of /dev/pipewire
/// (released by its VFS_CLOSE), `HOLD_IPC | tgid` = a legacy IPC client
/// (released when that process is gone, checked lazily by the next claimant).
/// An atomic, not a field of STATE: arbitration runs before STATE is taken,
/// and the liveness probe takes the run-queue lock.
static WRITER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
const HOLD_OPEN: u64 = 1 << 32;
const HOLD_IPC:  u64 = 2 << 32;

fn ipc_holder_alive(tgid: u32) -> bool {
    matches!(sched::proc_stat_of(tgid), Some((_, _, _, st)) if st != b'Z')
}

/// Take the device for `want`, or EBUSY. Idempotent for the holder.
fn claim(want: u64) -> Result<(), i32> {
    use core::sync::atomic::Ordering::AcqRel;
    use core::sync::atomic::Ordering::Acquire;
    loop {
        let cur = WRITER.load(Acquire);
        if cur == want { return Ok(()); }
        let free = cur == 0
            || (cur & !0xffff_ffff == HOLD_IPC && !ipc_holder_alive(cur as u32));
        if !free { return Err(-16); } // EBUSY
        if WRITER.compare_exchange(cur, want, AcqRel, Acquire).is_ok() {
            if cur != 0 {
                pci::serial_debug("[PW] device holder pid ");
                pci::serial_debug_hex(cur as u32);
                pci::serial_debug(" is gone, released\n");
            }
            return Ok(());
        }
    }
}

/// The capture reader: 0 = none, else `HOLD_OPEN | open_id` of the open of
/// /dev/pipewire that reads the microphone (one at a time, like the writer).
static READER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn claim_reader(want: u64) -> Result<(), i32> {
    use core::sync::atomic::Ordering::{AcqRel, Acquire};
    match READER.compare_exchange(0, want, AcqRel, Acquire) {
        Ok(_) => Ok(()),
        Err(cur) if cur == want => Ok(()),
        Err(_) => Err(-16), // EBUSY
    }
}

/// Open ids of O_RDONLY|O_NONBLOCK opens (recorded at VFS_OPEN; a later
/// fcntl(O_NONBLOCK) is not seen here).
static NB_OPENS: [core::sync::atomic::AtomicU64; 8] = [const { core::sync::atomic::AtomicU64::new(0) }; 8];

fn nb_add(id: u64) {
    use core::sync::atomic::Ordering::AcqRel;
    for s in NB_OPENS.iter() { if s.compare_exchange(0, id, AcqRel, core::sync::atomic::Ordering::Acquire).is_ok() { return; } }
}
fn nb_has(id: u64) -> bool { NB_OPENS.iter().any(|s| s.load(core::sync::atomic::Ordering::Acquire) == id) }
fn nb_del(id: u64) {
    use core::sync::atomic::Ordering::AcqRel;
    for s in NB_OPENS.iter() { let _ = s.compare_exchange(id, 0, AcqRel, core::sync::atomic::Ordering::Acquire); }
}

/// The poll tag of the capture side, woken by `tick_pump` when a period of
/// audio is ready.
const fn capture_tag() -> u64 { sched::poll_tag(sched::poll_class::STATIC, 0x5043) }
/// One 10 ms period of 48 kHz s16le stereo.
const CAPTURE_PERIOD: usize = 1920;

/// VFS_READ for the reader `h`. Never returns 0: O_NONBLOCK gets -EAGAIN
/// when nothing is ready; a blocking read waits for data, restarts a stream
/// that delivers nothing for 2 s once, and then fails -EIO.
fn capture_read(buf_ptr: usize, count: usize, h: u64) -> Message {
    if count < 4 { return err_reply(-22); }
    let nb = nb_has(h);
    let mut kbuf = [0u8; 4096];
    let mut total = 0usize;
    let mut since = sched::monotonic_us();
    let mut restarted = false;
    while total < count {
        let want = (count - total).min(kbuf.len()) & !3;
        if want == 0 { break; }
        let r = {
            let mut state = STATE.lock();
            if !state.initialized { return err_reply(-19); }
            // The close that revoked the claim may have run since the last
            // pass: never (re)start the stream for a reader that is gone.
            if READER.load(core::sync::atomic::Ordering::Acquire) != h {
                return if total > 0 { val_reply(total as u64) } else { err_reply(-9) };
            }
            match state.snd_driver.capture_ensure() {
                Ok(()) => state.snd_driver.capture_read(&mut kbuf[..want]),
                Err(e) => Err(e),
            }
        };
        match r {
            Err(-11) => {
                if total > 0 { break; }
                if nb { return err_reply(-11); }
                if sched::monotonic_us().wrapping_sub(since) > 2_000_000 {
                    if restarted { return err_reply(-5); }
                    restarted = true;
                    since = sched::monotonic_us();
                    STATE.lock().snd_driver.capture_restart();
                }
                sched::kthread::sleep_ns(5_000_000);
            }
            Err(e) => return if total > 0 { val_reply(total as u64) } else { err_reply(e) },
            Ok(n) => {
                if unsafe { sched::uaccess::copy_to_user(buf_ptr + total, kbuf.as_ptr(), n) } != 0 {
                    return if total > 0 { val_reply(total as u64) } else { err_reply(-14) };
                }
                total += n;
            }
        }
    }
    val_reply(total as u64)
}

fn release(holder: u64) {
    use core::sync::atomic::Ordering::{AcqRel, Acquire};
    let _ = WRITER.compare_exchange(holder, 0, AcqRel, Acquire);
}

/// Gate for the legacy IPC protocol (tags 0x100/0x200): the caller must be
/// allowed to write the device node, and then must hold (or take) it.
fn ipc_claim(caller_pid: u32) -> Result<(), i32> {
    let want = HOLD_IPC | sched::tgid_of(caller_pid) as u64;
    if WRITER.load(core::sync::atomic::Ordering::Acquire) == want { return Ok(()); }
    if !vfs_server::dyn_dev_may_write(DEV_MAJOR, DEV_MINOR, caller_pid) { return Err(-13); } // EACCES
    claim(want)
}

fn open_id_of(msg: &Message) -> u64 { arg(msg, 4) & 0xffff_ffff }

static STATE: sched::lockwatch::TrackedMutex<PipeWireState> =
    sched::lockwatch::TrackedMutex::new(sched::lockwatch::L_PIPEWIRE, PipeWireState::new());

/// Push with backpressure: when the spool is full, drain to the hardware
/// ring and wait for QEMU to consume (it drains at exactly realtime rate)
/// instead of silently dropping. This paces the producer to the audio
/// clock. Bounded so a stalled stream can't wedge the caller.
///
/// The wait does NOT hold STATE: it used to busy-spin with the lock held for
/// as long as the device queue stayed full, which starved the 100 Hz tick
/// pump (`try_lock`) and every other request (GET_DELAY from the sink's
/// timer, a second open's arbitration) and burned the CPU it ran on. Now
/// each round takes the lock to drain and push, drops it, and sleeps
/// `PUSH_WAIT_NS` on the one-shot timer when nothing fit.
///
/// Stall recovery: QEMU 11.x permanently stops consuming an output
/// stream that ever underruns (audio-core auto-disable; virtio-snd never
/// re-enables the voice). A live stream completes a TX_BUF_BYTES buffer
/// every ~3 ms, so if the TX used index freezes while we sit here with
/// data blocked, the stream is dead — recover it. This works precisely
/// because we are mid-push: data flows immediately after the restart, so
/// the revived stream doesn't underrun again.
fn push_spool_blocking(data: &[u8]) -> usize {
    use sched::monotonic_us;
    let mut off = 0;
    let start = monotonic_us();
    let mut last_used = STATE.lock().snd_driver.tx_used_idx();
    let mut stall_start = start;
    while off < data.len() {
        {
            let mut state = STATE.lock();
            if !state.initialized { break; }
            state.drain_spool();
            let pushed = state.push_spool(&data[off..]);
            off += pushed;
            if pushed != 0 { continue; }
            // Pad from here too: after a recovery the spool is nearly empty
            // and QEMU's next gulp would otherwise hit an empty queue — the
            // one remaining path into the permanent-stall spiral.
            state.snd_driver.top_up_silence();
            let now = monotonic_us();
            let used = state.snd_driver.tx_used_idx();
            if used != last_used {
                last_used = used;
                stall_start = now;
            } else if now.wrapping_sub(stall_start) > 250_000 {
                // 250 ms with zero completions while data is blocked.
                // QEMU's audio timer consumes in bursts every ~12.5 ms,
                // so a live stream never goes quiet for more than a few
                // tens of ms — spin-count thresholds false-triggered on
                // normal inter-tick gaps and thrashed the stream with
                // recoveries. Wall-clock 250 ms is ~20 tick periods.
                state.snd_driver.recover_stream();
                state.drain_spool();
                last_used = state.snd_driver.tx_used_idx();
                stall_start = monotonic_us();
            }
            if now.wrapping_sub(start) > 3_000_000 {
                // Give up and drop the rest. Say so: a producer that keeps
                // hitting this is what a "silent" audio death looks like.
                pci::serial_debug("[PW] push blocked 3 s with no TX progress, dropping ");
                pci::serial_debug_hex((data.len() - off) as u32);
                pci::serial_debug(" bytes\n");
                break;
            }
        }
        // STATE released: the tick pump and other requests run meanwhile.
        sched::kthread::sleep_ns(PUSH_WAIT_NS);
    }
    off
}

/// How long a blocked push sleeps between attempts. One TX buffer
/// (TX_BUF_BYTES) plays in ~3 ms, so 1 ms keeps the device queue topped up
/// without a busy loop.
const PUSH_WAIT_NS: u64 = 1_000_000;

// ── Protocol helper ──────────────────────────────────────────────────────────

fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off+8].try_into().unwrap())
}

fn make_reply(val: i64) -> Message {
    let mut m = Message::empty();
    m.tag = 0x8000_0000_0000_0000; // VFS reply tag
    m.data[0..8].copy_from_slice(&val.to_le_bytes());
    m
}

fn val_reply(v: u64) -> Message { make_reply(v as i64) }
fn err_reply(e: i32) -> Message { make_reply(e as i64) }

// ── Service Implementation ───────────────────────────────────────────────────

/// `cmdline`: the kernel command line. `snd.backend=auto` (default:
/// virtio-sound when present, else Intel HD Audio), `virtio` or `hda`.
/// True once `init` found a device and registered the server.
pub fn is_initialized() -> bool { STATE.lock().initialized }

pub fn init(cmdline: &str) -> Result<u32, i32> {
    let mut state = STATE.lock();
    // Idempotent: a sound module may have attached (and registered the server) before the boot path ran.
    if state.initialized { return Ok(state.bound_port); }

    pci::rdebug("[PW] Initializing...\n");
    let want = cmdline_opt(cmdline, "snd.backend").unwrap_or("auto");
    if want != "hda" {
        let mut v = VirtioSnd::new();
        if v.probe().is_ok() {
            state.snd_driver = Backend::Virtio(v);
        }
    }
    if matches!(state.snd_driver, Backend::None) && want != "virtio" {
        let mut h = Hda::new();
        if h.probe().is_ok() {
            pci::serial_debug("[PW] playback device: Intel HD Audio\n");
            state.snd_driver = Backend::Hda(h);
        }
    }
    if matches!(state.snd_driver, Backend::None) && (want == "auto" || want == "hdmi") {
        // The built-in HDMI backend goes through the same KSndOps adapter a module would use.
        if let Some((ops, ctx)) = hdmi_ops::probe(cmdline) {
            pci::serial_debug("[PW] playback device: HDMI audio (hdmi0)\n");
            state.snd_driver = Backend::Module { ops, ctx };
        }
    }
    if matches!(state.snd_driver, Backend::None) {
        pci::serial_debug("[PW] no sound device (virtio-sound, HD Audio or HDMI)\n");
        return Err(-19); // ENODEV
    }
    register_server(&mut state)
}

/// Attach a sound module's backend (`kapi_snd_register`). When no built-in device was found (so
/// `init` registered nothing) this also registers `/dev/pipewire` and the socket, lazily, so
/// machines without sound are unchanged. Err(-16) when a backend is already present.
pub fn attach(ops: &'static KSndOps, ctx: usize) -> Result<u32, i32> {
    let mut state = STATE.lock();
    if !matches!(state.snd_driver, Backend::None) { return Err(-16); }
    state.snd_driver = Backend::Module { ops, ctx };
    let port = if state.initialized {
        state.bound_port
    } else {
        match register_server(&mut state) {
            Ok(p) => p,
            Err(e) => { state.snd_driver = Backend::None; return Err(e); }
        }
    };
    if ops.flags & KSND_CAPTURE != 0 { let _ = sched::kthread::spawn("[audio]", audio_service); }
    Ok(port)
}

/// Create the server port, `/dev/pipewire` and the socket for the backend already chosen.
fn register_server(state: &mut PipeWireState) -> Result<u32, i32> {
    let server_port = port::create(0).ok_or(-12)?; // ENOMEM
    if !port::register_handler(server_port, handle_msg) {
        return Err(-1);
    }
    // The port is in every process's auxv (258) for the legacy PCM protocol,
    // but the VFS tags carry kernel-filled identity (open_id): user code may
    // only send the legacy tags, or it could act as another process's open
    // (claim/release HOLD_OPEN, holder-only SET_PARAMS).
    port::register_user_filter(server_port, user_may_send);
    state.bound_port = server_port;

    vfs_server::register_device_notify_open("/dev/pipewire", server_port, 0, DEV_MAJOR, DEV_MINOR);
    net_server::force_bind_unix(PW_SOCKET_PATH, server_port);

    state.initialized = true;
    sched::register_tick_hook(tick_pump);
    if matches!(state.snd_driver, Backend::Hda(_)) { let _ = sched::kthread::spawn("[audio]", audio_service); }
    pci::rdebug("[PW] Ready on port ");
    pci::rdebug_hex(server_port);
    pci::rdebug("\n");
    Ok(server_port)
}

/// The user-sendable tags: the legacy protocol (0x100 SET_PARAMS, 0x200 PCM,
/// both arbitrated by the kernel-supplied caller pid), PUMP and PING.
fn user_may_send(msg: &Message) -> bool {
    matches!(msg.tag, 0x100 | 0x200 | 0x300 | 0x1000)
}

fn handle_msg(msg: &Message, caller_pid: u32, _target_port: u32) -> Message {
    // Arbitration first, with no lock held (see WRITER).
    match msg.tag {
        0x10 => { // VFS_OPEN {dev_id, flags, _, pid, open_id}
            let flags = arg(msg, 1) as u32;
            if flags & 3 == 0 { // O_RDONLY: no claim
                if flags & 0o4000 != 0 { nb_add(HOLD_OPEN | open_id_of(msg)); }
                return val_reply(0);
            }
            return match claim(HOLD_OPEN | open_id_of(msg)) {
                Ok(()) => val_reply(0),
                Err(e) => err_reply(e),
            };
        }
        0x13 => { // VFS_CLOSE {dev_id, open_id}: the open's last fd is gone
            let h = HOLD_OPEN | (arg(msg, 1) & 0xffff_ffff);
            release(h);
            nb_del(h);
            if READER.compare_exchange(h, 0, core::sync::atomic::Ordering::AcqRel,
                                       core::sync::atomic::Ordering::Acquire).is_ok() {
                let mut state = STATE.lock();
                if state.initialized { state.snd_driver.capture_close(); }
            }
            return val_reply(0);
        }
        0x11 => { // VFS_READ: microphone capture, one reading open at a time
            let h = HOLD_OPEN | open_id_of(msg);
            if let Err(e) = claim_reader(h) { return err_reply(e); }
            if !STATE.lock().initialized { return err_reply(-19); }
            return capture_read(arg(msg, 1) as usize, arg(msg, 2) as usize, h);
        }
        0x12 => { // VFS_WRITE: only through the holding open
            if let Err(e) = claim(HOLD_OPEN | open_id_of(msg)) { return err_reply(e); }
        }
        0x28 if arg(msg, 1) as u32 == 0x101 => { // SET_PARAMS: holder only
            if let Err(e) = claim(HOLD_OPEN | open_id_of(msg)) { return err_reply(e); }
        }
        0x100 | 0x200 => {
            if let Err(e) = ipc_claim(caller_pid) { return err_reply(e); }
        }
        vfs_server::VFS_POLL => {
            // Always writable (a write blocks inside this server, or is
            // refused EBUSY at arbitration). The capture holder is also
            // readable once a period is ready, and parks on `capture_tag`,
            // which `tick_pump` wakes; polling claims and starts the capture
            // stream, so poll-before-read clients work. Everyone else (the
            // writer, a second reader, a backend with no capture) gets the
            // constant static tag, which nothing has to wake.
            let h = HOLD_OPEN | open_id_of(msg);
            let mut revents = vfs_server::POLLOUT as u64;
            let mut tag = vfs_server::static_poll_tag();
            if WRITER.load(core::sync::atomic::Ordering::Acquire) != h && claim_reader(h).is_ok() {
                let mut state = STATE.lock();
                if state.initialized && state.snd_driver.capture_ensure().is_ok() {
                    tag = capture_tag();
                    if state.snd_driver.capture_pending() >= CAPTURE_PERIOD { revents |= vfs_server::POLLIN as u64; }
                } else {
                    drop(state);
                    let _ = READER.compare_exchange(h, 0, core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Acquire);
                }
            }
            let mut m = Message::empty();
            m.data[0..8].copy_from_slice(&revents.to_le_bytes());
            m.data[24..32].copy_from_slice(&tag.to_le_bytes());
            m.data[32] = 1;
            return m;
        }
        _ => {}
    }
    handle(msg)
}

/// 100 Hz pump, run from the BSP timer IRQ (sched::register_tick_hook).
/// Drains spooled PCM to the device and pads with silence below the
/// watermark so the device-side queue is NEVER seen empty — QEMU 11.x's
/// audio backend permanently loses its refill wakeup on the first empty
/// poll. try_lock: if a producer is mid-push it holds the lock and its own
/// blocked loop is already pumping; skipping a tick is fine.
/// Reboot / power-off quiesce (kernel/src/power.rs): silence and stop the
/// playback device for good. HD Audio: streams stopped, codec amplifiers
/// (EAPD) off, pins disabled, codec to D3, CORB/RIRB off (`Hda::shutdown`);
/// `link_reset` also holds the HDA link in reset. virtio-sound needs nothing
/// (the reset takes the virtual device down with the machine). Waits at most
/// 200 ms for the server lock; the server is dead afterwards (`initialized`
/// = false makes the tick pump and every request a no-op). Returns a report
/// line and the PCI function whose bus mastering the caller may now clear.
pub fn shutdown(link_reset: bool) -> (alloc::string::String, Option<drivers::pci::PciFunction>) {
    let mut state = match STATE.try_lock_spin(200_000_000) {
        Some(s) => s,
        None => return (alloc::string::String::from("audio server busy for 200 ms; device left as is"), None),
    };
    state.initialized = false;
    match &mut state.snd_driver {
        Backend::Hda(h) => {
            let f = h.pci_function();
            (alloc::format!("HD Audio: {}", h.shutdown(link_reset, 300_000)), f)
        }
        Backend::Virtio(_) => (alloc::string::String::from("virtio-sound: nothing to do"), None),
        Backend::Module { ops, ctx } => {
            let t = module_text(|b, c| (ops.shutdown)(*ctx, link_reset as u32, b, c));
            (alloc::format!("sound module: {}", t), None)
        }
        Backend::None => (alloc::string::String::from("no sound device"), None),
    }
}

fn tick_pump() {
    let mut ready = false;
    if let Some(mut state) = STATE.try_lock() {
        if state.initialized {
            state.drain_spool();
            state.snd_driver.top_up_silence();
            ready = READER.load(core::sync::atomic::Ordering::Acquire) != 0
                && state.snd_driver.capture_pending() >= CAPTURE_PERIOD;
        }
    }
    if ready { sched::try_wake_poll_tagged(capture_tag()); }
}

/// Thread context, every 100 ms: jack sense, capture health. Verbs and
/// stream restarts never run in the timer IRQ.
fn audio_service() {
    loop {
        sched::kthread::sleep_ns(100_000_000);
        if let Some(mut state) = STATE.try_lock() {
            if !state.initialized { return; }
            state.snd_driver.jack_service();
        }
    }
}

pub fn handle(msg: &Message) -> Message {
    if !STATE.lock().initialized {
        return Message::empty();
    }
    // The two pushes take STATE per round themselves (push_spool_blocking).
    match msg.tag {
        0x12 => { // VFS_WRITE
            let buf_ptr = arg(msg, 1) as usize;
            let count = arg(msg, 2) as usize;
            let mut kbuf = [0u8; 1024];
            let mut total = 0;
            while total < count {
                let n = (count - total).min(1024);
                // Fault-tolerant copy, no lock held (push_spool_blocking takes
                // STATE per round itself).
                if unsafe { sched::uaccess::copy_from_user(kbuf.as_mut_ptr(), buf_ptr + total, n) } != 0 {
                    return if total > 0 { val_reply(total as u64) } else { err_reply(-14) };
                }
                let pushed = push_spool_blocking(&kbuf[..n]);
                total += pushed;
                if pushed < n { break; } // Stream stalled
            }
            STATE.lock().drain_spool();
            return val_reply(total as u64);
        }
        0x200 => { // Legacy PCM
            let len = (u16::from_le_bytes([msg.data[0], msg.data[1]]) as usize).min(msg.data.len() - 2);
            {
                // Diagnostic: surface large producer gaps (rare, cold path).
                let mut state = STATE.lock();
                let now = sched::monotonic_us();
                let last = state.last_pcm_us;
                state.last_pcm_us = now;
                if last != 0 && now.wrapping_sub(last) > 100_000 {
                    pci::serial_debug("[PW] producer gap ms=");
                    pci::serial_debug_hex((now.wrapping_sub(last) / 1000) as u32);
                    pci::serial_debug("\n");
                }
            }
            push_spool_blocking(&msg.data[2..2 + len]);
            STATE.lock().drain_spool();
            return Message::empty();
        }
        _ => {}
    }

    // VFS_IOCTL: user memory is copied in before STATE is taken and copied
    // out after it is released, through the fault-tolerant uaccess copy. It
    // used to be read and written with STATE held, so a fault there (or the
    // address-space lock it took) ran under the lock the 100 Hz tick pump
    // and every producer contend on.
    if msg.tag == 0x28 {
        return ioctl(arg(msg, 1) as u32, arg(msg, 2) as usize);
    }

    let mut state = STATE.lock();
    if !state.initialized {
        return Message::empty();
    }

    state.drain_spool();

    let reply = match msg.tag {
        0x100 => { // Legacy SET_PARAMS
            let freq = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
            state.snd_driver.reconfigure_stream(0, freq, msg.data[4]);
            Message::empty()
        }
        0x300 => { // PUMP
            state.drain_spool();
            Message::empty()
        }
        0x1000 => { // PING
            let mut resp = Message::empty();
            resp.tag = 0x1001; // PONG
            resp
        },
        _ => Message::empty()
    };

    state.drain_spool();
    reply
}

/// The device's private ioctls (0x101 SET_PARAMS, 0x102 GET_DELAY,
/// 0x103 SND_GET_INFO). STATE is held only between the copies.
fn ioctl(cmd: u32, arg_val: usize) -> Message {
    use sched::uaccess::{copy_from_user, copy_to_user};
    match cmd {
        0x101 => { // SET_PARAMS: { u32 freq; u8 channels; ... }
            let mut params = [0u8; 8];
            if unsafe { copy_from_user(params.as_mut_ptr(), arg_val, 8) } != 0 { return err_reply(-14); }
            let freq = u32::from_le_bytes([params[0], params[1], params[2], params[3]]);
            let channels = params[4];
            let mut state = STATE.lock();
            if !state.initialized { return Message::empty(); }
            state.drain_spool();
            state.snd_driver.reconfigure_stream(0, freq, channels);
            state.drain_spool();
            val_reply(0)
        }
        0x102 => { // GET_DELAY: u32 bytes queued ahead of the DAC
            // Spooled bytes plus the device ring's uncompleted buffers
            // (each TX_BUF_BYTES; the last one may be partly played). This
            // is what SNDCTL_DSP_GETODELAY / snd_pcm_delay report, and what
            // a timer-driven producer (ports/pipewire's leandros-snd-sink)
            // steers its rate against, so it never has to block here.
            let delay = {
                let mut state = STATE.lock();
                if !state.initialized { return Message::empty(); }
                state.drain_spool();
                (state.spool_len + state.snd_driver.queued_bytes()) as u32
            };
            let b = delay.to_le_bytes();
            if unsafe { copy_to_user(arg_val, b.as_ptr(), 4) } != 0 { return err_reply(-14); }
            val_reply(0)
        }
        0x103 => { // SND_GET_INFO: NUL-terminated text into a 4096-byte buffer
            let mut text = {
                let mut state = STATE.lock();
                if !state.initialized { return Message::empty(); }
                state.drain_spool();
                state.snd_driver.info()
            };
            text.truncate(4095);
            let n = text.len();
            text.push('\0');
            if unsafe { copy_to_user(arg_val, text.as_ptr(), n + 1) } != 0 { return err_reply(-14); }
            val_reply(n as u64)
        }
        _ => err_reply(-25), // ENOTTY
    }
}
