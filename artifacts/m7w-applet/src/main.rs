// leandros-applet — a minimal, dependency-free COSMIC-panel applet stand-in.
//
// The real cosmic applets are libcosmic/iced apps that pull tokio+zbus and talk
// to system services (timedate1, logind, upower, …) that do not exist on
// LeandrOS. cosmic-panel refuses to render a bar with no applet content (its
// render() early-returns while actual_size<=20), so with zero applets the bar
// never appears. This program is a tiny xdg_toplevel + wl_shm client that draws
// one solid opaque block and sits. cosmic-panel embeds ANY client xdg_toplevel
// as a panel window, so this gives the panel real content and forces frame 0.
//
// It connects to the panel's EMBEDDED wayland server via the inherited
// WAYLAND_SOCKET fd (cosmic-panel hands each applet one). Pure-Rust wayland
// backend => the only runtime dependency is ld-musl (libc).

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_registry::WlRegistry,
    wl_shm::{Format, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

const WIDTH: i32 = 220;
const HEIGHT: i32 = 32;
// XRGB8888 little-endian in memory is [B, G, R, X]; 0x00RRGGBB gives that color.
// Match cosmic-panel's ThemeDefault bar (27,27,27) so the applet reads as part
// of the bar rather than a coloured rectangle sitting on it. Liveness is no
// longer signalled by a garish colour — the clock ticks once a second.
const BG: u32 = 0x001B_1B1B;
const FG: u32 = 0x00E6_E6E6;

/// 5x7 bitmap glyphs for `0`-`9` and `:`, one `u8` per row, bit 4 = leftmost.
/// Hand-rolled because the alternative is pulling a font crate (and then a
/// rasteriser) into a binary whose entire point is having no dependencies.
const GLYPH_W: i32 = 5;
const GLYPH_H: i32 = 7;
const SCALE: i32 = 3;
const ADVANCE: i32 = (GLYPH_W + 1) * SCALE;
const FONT: [[u8; GLYPH_H as usize]; 11] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110], // 0
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110], // 1
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111], // 2
    [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110], // 3
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010], // 4
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110], // 5
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110], // 6
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000], // 7
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110], // 8
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100], // 9
    [0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000], // :
];

/// `HH:MM:SS` for the current time.
///
/// The guest has no RTC and no tzdata, so this is UTC from a clock that starts
/// at the epoch — i.e. time since boot. That is still a genuinely live readout,
/// and it is what makes the panel visibly *running* rather than painted.
fn clock_text() -> [u8; 8] {
    let secs = unsafe { libc::time(std::ptr::null_mut()) }.max(0) as u64;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let d = |v: u64| [b'0' + (v / 10) as u8, b'0' + (v % 10) as u8];
    let (hh, mm, ss) = (d(h), d(m), d(s));
    [hh[0], hh[1], b':', mm[0], mm[1], b':', ss[0], ss[1]]
}

/// Blit `text` into an XRGB8888 buffer, horizontally and vertically centred.
fn draw_text(px: *mut u32, text: &[u8]) {
    let text_w = text.len() as i32 * ADVANCE - SCALE; // no trailing gap
    let x0 = (WIDTH - text_w) / 2;
    let y0 = (HEIGHT - GLYPH_H * SCALE) / 2;
    for (i, &ch) in text.iter().enumerate() {
        let glyph = match ch {
            b'0'..=b'9' => FONT[(ch - b'0') as usize],
            b':' => FONT[10],
            _ => continue,
        };
        let gx = x0 + i as i32 * ADVANCE;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..GLYPH_W {
                if bits & (1 << (GLYPH_W - 1 - col)) == 0 { continue; }
                // Expand one font pixel to a SCALE x SCALE square.
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let x = gx + col * SCALE + dx;
                        let y = y0 + row as i32 * SCALE + dy;
                        if x < 0 || x >= WIDTH || y < 0 || y >= HEIGHT { continue; }
                        unsafe { *px.add((y * WIDTH + x) as usize) = FG; }
                    }
                }
            }
        }
    }
}
// cosmic-comp only recomputes idle-inhibit state inside its refresh/repaint
// cycle. A fully static applet surface never repaints once mapped, so on a
// truly idle desktop smithay's ext-idle-notify timer is never armed and
// cosmic-idle's fade/screen-off can never fire. Re-committing at ~1Hz keeps
// the compositor's refresh loop alive, matching what a real clock applet
// does on Linux.
const TICK_MS: i32 = 1000;

struct App {
    compositor: WlCompositor,
    shm: WlShm,
    wm_base: XdgWmBase,
    surface: Option<WlSurface>,
    xdg_surface: Option<XdgSurface>,
    toplevel: Option<XdgToplevel>,
    buffer: Option<WlBuffer>,
    /// Shm pool and its mapping, allocated ONCE and kept for the process
    /// lifetime. A pool per frame means a memfd per frame, and memfds are not
    /// reclaimed on this kernel (see sys_memfd_create) — a 1 Hz repaint
    /// exhausted the 128-slot tmpfs pool in ~100 frames and then every
    /// memfd_create in the session failed forever.
    pool: Option<WlShmPool>,
    pixels: *mut u32,
    drawn: bool,
    closed: bool,
}

impl App {
    /// Allocate the shm pool and its mapping. Called once; false on failure.
    fn alloc(&mut self, qh: &QueueHandle<Self>) -> bool {
        let stride = WIDTH * 4;
        let size = (stride * HEIGHT) as usize;
        let fd = unsafe {
            let name = b"leandros-applet\0";
            let fd = libc::memfd_create(name.as_ptr() as *const libc::c_char, 0);
            if fd < 0 {
                eprintln!("leandros-applet: memfd_create failed");
                return false;
            }
            if libc::ftruncate(fd, size as libc::off_t) != 0 {
                libc::close(fd);
                eprintln!("leandros-applet: ftruncate failed");
                return false;
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if ptr == libc::MAP_FAILED {
                libc::close(fd);
                eprintln!("leandros-applet: mmap failed");
                return false;
            }
            // Kept mapped for the process lifetime so each tick repaints in place.
            self.pixels = ptr as *mut u32;
            OwnedFd::from_raw_fd(fd)
        };
        // The pool is kept alive too — every frame's wl_buffer is carved from it.
        self.pool = Some(self.shm.create_pool(fd.as_fd(), size as i32, qh, ()));
        true
    }

    /// Repaint the clock and commit it.
    ///
    /// The wl_buffer is recreated every frame even though the pixels behind it are
    /// the same memory: re-attaching a buffer the compositor has not released
    /// left the panel with no bar at all. A wl_buffer is just a protocol
    /// object, so this costs nothing — unlike recreating the pool.
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if self.pixels.is_null() && !self.alloc(qh) {
            return;
        }
        let stride = WIDTH * 4;
        unsafe {
            for i in 0..(WIDTH * HEIGHT) as usize {
                *self.pixels.add(i) = BG;
            }
            draw_text(self.pixels, &clock_text());
        }

        let buffer = self.pool.as_ref().unwrap()
            .create_buffer(0, WIDTH, HEIGHT, stride, Format::Xrgb8888, qh, ());

        let surface = self.surface.as_ref().unwrap();
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, WIDTH, HEIGHT);
        surface.commit();

        if let Some(old) = self.buffer.replace(buffer) {
            old.destroy();
        }
        // Only announce the first commit; the 1 Hz tick re-runs this same path
        // and must not spam the serial log forever.
        if !self.drawn {
            eprintln!("leandros-applet: committed {}x{} clock", WIDTH, HEIGHT);
        }
        self.drawn = true;
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! ignore_dispatch {
    ($iface:ty) => {
        impl Dispatch<$iface, ()> for App {
            fn event(
                _: &mut Self,
                _: &$iface,
                _: <$iface as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

ignore_dispatch!(WlCompositor);
ignore_dispatch!(WlShm);
ignore_dispatch!(WlShmPool);
ignore_dispatch!(WlSurface);
ignore_dispatch!(WlBuffer);

impl Dispatch<XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for App {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            // (Re)draw on every configure; the panel sends s.size=None so we keep
            // our own size. Always re-attach so a post-map configure still yields
            // a committed buffer the panel can lay out.
            state.draw(qh);
        }
    }
}

impl Dispatch<XdgToplevel, ()> for App {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

fn main() {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("leandros-applet: connect_to_env failed: {e}");
            std::process::exit(1);
        }
    };
    let (globals, mut event_queue) = match registry_queue_init::<App>(&conn) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("leandros-applet: registry init failed: {e}");
            std::process::exit(1);
        }
    };
    let qh = event_queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .expect("wl_compositor missing");
    let shm: WlShm = globals.bind(&qh, 1..=2, ()).expect("wl_shm missing");
    let wm_base: XdgWmBase = globals.bind(&qh, 1..=6, ()).expect("xdg_wm_base missing");

    let mut app = App {
        compositor,
        shm,
        wm_base,
        surface: None,
        xdg_surface: None,
        toplevel: None,
        buffer: None,
        pool: None,
        pixels: std::ptr::null_mut(),

        drawn: false,
        closed: false,
    };

    let surface = app.compositor.create_surface(&qh, ());
    let xdg_surface = app.wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_app_id("com.system76.CosmicAppletTime".to_string());
    toplevel.set_title("leandros".to_string());
    surface.commit(); // elicit the initial xdg_surface.configure

    app.surface = Some(surface);
    app.xdg_surface = Some(xdg_surface);
    app.toplevel = Some(toplevel);

    eprintln!("leandros-applet: entering event loop");
    eprintln!("leandros-applet: tick: {TICK_MS}ms");

    // A plain blocking_dispatch() would sleep until the compositor sends us
    // something, which is fine for correctness but gives us no way to also
    // commit on our own ~1Hz schedule. Do the same prepare_read/poll/read
    // dance blocking_dispatch() does internally, but poll with a timeout so
    // a quiet socket falls through to a periodic re-commit instead of
    // sleeping forever.
    // Second currently painted into the surface. Repainting is driven by this
    // going stale rather than by the poll timing out: a busy compositor keeps
    // the socket readable, and a timeout-only repaint would leave the clock
    // frozen for exactly as long as the desktop is interesting.
    let mut shown_sec: i64 = -1;
    while !app.closed {
        if let Err(e) = event_queue.dispatch_pending(&mut app) {
            eprintln!("leandros-applet: dispatch error: {e}");
            break;
        }
        if let Err(e) = event_queue.flush() {
            eprintln!("leandros-applet: flush error: {e}");
            break;
        }

        let guard = match event_queue.prepare_read() {
            Some(g) => g,
            // Another read already queued events for us; go dispatch them.
            None => continue,
        };

        let mut pfd = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, TICK_MS) };

        if ret > 0 {
            match guard.read() {
                Ok(_) => {}
                // A successful poll (POLLIN) can still yield no complete
                // message: on the very first configure/enter burst the bytes
                // may not form a whole message yet, or dispatch_pending/another
                // read already drained the socket. wayland-client signals that
                // with a WouldBlock (EAGAIN, os error 11) Io error — a NORMAL,
                // expected outcome of the prepare_read/poll/read protocol, not a
                // fatal one. blocking_dispatch() swallows it internally; so must
                // we. Treating it as fatal (the old `break`) killed the applet
                // ~150 ms after its first commit, so the panel lost its embedded
                // toplevel (dark bar, no teal block) and the 1 Hz idle-keepalive
                // tick never ran.
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    eprintln!("leandros-applet: read error: {e}");
                    break;
                }
            }
        } else if ret == 0 {
            // Timeout, nothing from the compositor: just cancel the prepared
            // read (Drop). The repaint decision is made below.
            drop(guard);
        } else {
            drop(guard);
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                eprintln!("leandros-applet: poll error: {err}");
                break;
            }
        }

        // Re-commit once a second. Besides advancing the clock this keeps
        // cosmic-comp's refresh loop alive: it only recomputes idle-inhibit
        // state inside a repaint cycle, so a fully static applet surface means
        // smithay's ext-idle-notify timer is never armed and cosmic-idle can
        // never fade the screen.
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        if app.drawn && now != shown_sec {
            shown_sec = now;
            app.draw(&qh);
        }
    }
}
