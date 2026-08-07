// wlinput — a Wayland client that maps a real xdg_toplevel, binds wl_pointer /
// wl_keyboard / wl_touch, and counts every input event the compositor sends it.
//
// WHY THIS EXISTS. TODO entry 6 says no input of any kind reaches COSMIC. A
// previous lane exonerated, by measurement, everything below and including
// libinput: the libudev shim enumerates both devices and resolves
// dev_from_devnum; the libseat shim opens /dev/input/event0 and event1; the
// kernel's evdev census shows reads and POLLINs against those nodes; and
// libinput itself, run in-process by `liprobe`, produced motion_abs=62 key=8
// with dispatch_err=0. So the break is ABOVE libinput's queue — either
// smithay's drain of that queue, or cosmic-comp's own routing.
//
// This program splits that remaining space in half, and it is the only
// instrument that can, because cosmic-comp must not be patched and its log
// route is dead twice over (logger/mod.rs pins smithay=warn via add_directive,
// which RUST_LOG cannot override, and its fmt::layer() writes to stdout, which
// cosmic-session does not capture). A Wayland client sits on the OUTPUT side of
// cosmic-comp's seat: if events arrive here, everything from libinput through
// process_input_event through seat routing works and the failure is
// cursor/render-side; if nothing arrives, cosmic-comp is dropping them.
//
// INSTRUMENT NOTES (read before trusting any output):
//
//  * It does NOT use `Connection::connect_to_env()`. Same trap wl-globals
//    documents: cosmic-panel hands its applets a WAYLAND_SOCKET fd pointing at
//    cosmic-PANEL's own embedded wayland server, and that server has a wl_seat
//    that would never carry compositor input. The socket is named explicitly
//    and echoed on every line that matters.
//
//  * Absence of input events IS the result being looked for, and absence is
//    also what a client that never mapped produces. So the run is built around
//    POSITIVE CONTROLS that fail loudly and separately:
//      - BOUND  proves the four globals resolved;
//      - SEATCAP proves the seat advertised pointer/keyboard/touch, and is
//        printed even when it is 0, so "no capability" and "no seat" differ;
//      - CONFIGURE proves cosmic-comp PROCESSED one of our requests and
//        answered it — the client-to-compositor direction is alive;
//      - MAPPED proves a buffer was attached and committed, i.e. we are a
//        surface the compositor could route a pointer into. Without MAPPED,
//        zero pointer events is meaningless.
//    A run with no CONFIGURE line is a broken run and must not be read as a
//    negative about input.
//
//  * Every census is a SERIES, printed on an interval, never a single point at
//    the end: a counter that was already climbing before the injection, or one
//    that stops mid-run, is only visible against its own history.
//
//  * The first few events of each kind are printed individually with their
//    coordinates, then suppressed. Counts alone cannot distinguish "the
//    compositor sent us the pointer at one fixed position 400 times" from "the
//    pointer moved", and that difference is the whole question for a tablet
//    (absolute) device.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_callback::WlCallback,
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_pointer::{self, WlPointer},
    wl_registry::WlRegistry,
    wl_seat::{self, WlSeat},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
    wl_touch::{self, WlTouch},
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

const TAG: &str = "[WLI]";

// ------------------------------------------------------------------ counters --
// Statics, not fields, so the census thread can read them without sharing the
// event queue (which is not Send) with anything.
macro_rules! counters {
    ($($n:ident),* $(,)?) => {
        $(static $n: AtomicU64 = AtomicU64::new(0);)*
        fn census_line(t: f64) -> String {
            let mut s = format!("{TAG} CENSUS t={t:.1}s");
            $( s.push_str(&format!(" {}={}", stringify!($n).to_lowercase(),
                                   $n.load(Ordering::Relaxed))); )*
            s
        }
    };
}

counters!(
    PTR_ENTER, PTR_LEAVE, PTR_MOTION, PTR_BUTTON, PTR_AXIS, PTR_FRAME,
    KBD_KEYMAP, KBD_ENTER, KBD_LEAVE, KBD_KEY, KBD_MODS, KBD_REPEAT,
    TCH_DOWN, TCH_UP, TCH_MOTION, TCH_FRAME,
    SURF_ENTER, SURF_LEAVE, CONFIGURE, PING, FRAME_CB,
);

static MAPPED: AtomicBool = AtomicBool::new(false);

/// Per-kind print budget: the first few of each event kind are printed in full,
/// the rest only counted. Keeps a 45 s pointer sweep from burying the census.
const DETAIL: u64 = 6;

fn detail(c: &AtomicU64) -> bool {
    c.load(Ordering::Relaxed) <= DETAIL
}

// -------------------------------------------------------------------- state --

struct App {
    shm: WlShm,
    surface: Option<WlSurface>,
    pool: Option<WlShmPool>,
    buffer: Option<WlBuffer>,
    pixels: *mut u32,
    w: i32,
    h: i32,
    seat_caps: u32,
    seat_name: String,
    configured: bool,
    closed: bool,
}

impl App {
    /// Allocate the shm pool once and keep it mapped for the process lifetime.
    /// A pool per frame means a memfd per frame, and memfds are not reclaimed on
    /// this kernel — the m7w applet exhausted the 128-slot tmpfs pool that way.
    fn alloc(&mut self, qh: &QueueHandle<Self>) -> bool {
        let size = (self.w * self.h * 4) as usize;
        let fd = unsafe {
            let name = b"wlinput\0";
            let fd = libc::memfd_create(name.as_ptr() as *const libc::c_char, 0);
            if fd < 0 {
                println!("{TAG} FAIL stage=memfd_create errno={}", errno());
                return false;
            }
            if libc::ftruncate(fd, size as libc::off_t) != 0 {
                println!("{TAG} FAIL stage=ftruncate errno={}", errno());
                libc::close(fd);
                return false;
            }
            let p = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                println!("{TAG} FAIL stage=mmap errno={}", errno());
                libc::close(fd);
                return false;
            }
            self.pixels = p as *mut u32;
            OwnedFd::from_raw_fd(fd)
        };
        self.pool = Some(self.shm.create_pool(fd.as_fd(), size as i32, qh, ()));
        true
    }

    /// Paint a flat, obvious colour and commit. The colour matters: it is what
    /// a screenshot has to show for "the client is on screen" to be checkable
    /// independently of anything this program prints.
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if self.pixels.is_null() && !self.alloc(qh) {
            return;
        }
        unsafe {
            for i in 0..(self.w * self.h) as usize {
                // magenta field, white 32px grid — a solid fill can be confused
                // with a compositor clear of the same colour.
                let x = i as i32 % self.w;
                let y = i as i32 / self.w;
                *self.pixels.add(i) = if x % 32 == 0 || y % 32 == 0 {
                    0x00FF_FFFF
                } else {
                    0x00FF_00FF
                };
            }
        }
        let buffer = self.pool.as_ref().unwrap().create_buffer(
            0,
            self.w,
            self.h,
            self.w * 4,
            wl_shm::Format::Xrgb8888,
            qh,
            (),
        );
        let surface = self.surface.as_ref().unwrap();
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, self.w, self.h);
        surface.frame(qh, ());
        surface.commit();
        if let Some(old) = self.buffer.replace(buffer) {
            old.destroy();
        }
        if !MAPPED.swap(true, Ordering::Relaxed) {
            println!("{TAG} MAPPED w={} h={}", self.w, self.h);
        }
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// ---------------------------------------------------------------- dispatch --

macro_rules! ignore {
    ($($t:ty),* $(,)?) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(),
                     _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore!(WlCompositor, WlShm, WlShmPool, WlBuffer);

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

impl Dispatch<WlCallback, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlCallback,
        _: <WlCallback as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        FRAME_CB.fetch_add(1, Ordering::Relaxed);
    }
}

impl Dispatch<WlSurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        e: <WlSurface as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_surface::Event;
        match e {
            Event::Enter { .. } => {
                SURF_ENTER.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV surface.enter");
            }
            Event::Leave { .. } => {
                SURF_LEAVE.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        e: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            PING.fetch_add(1, Ordering::Relaxed);
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for App {
    fn event(
        app: &mut Self,
        xs: &XdgSurface,
        e: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = e {
            let n = CONFIGURE.fetch_add(1, Ordering::Relaxed) + 1;
            xs.ack_configure(serial);
            app.configured = true;
            if n <= DETAIL {
                println!("{TAG} CONFIGURE n={n} serial={serial} w={} h={}", app.w, app.h);
            }
        }
    }
}

impl Dispatch<XdgToplevel, ()> for App {
    fn event(
        app: &mut Self,
        _: &XdgToplevel,
        e: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 && (width != app.w || height != app.h) {
                    println!("{TAG} TOPLEVEL resize {}x{} -> {width}x{height}", app.w, app.h);
                    // The pool is sized once; only shrink into it, never grow
                    // past it, or create_buffer would read past the mapping.
                    if width * height <= app.w * app.h || app.pixels.is_null() {
                        app.w = width;
                        app.h = height;
                    }
                }
            }
            xdg_toplevel::Event::Close => {
                println!("{TAG} TOPLEVEL close");
                app.closed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(
        app: &mut Self,
        _: &WlSeat,
        e: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            wl_seat::Event::Capabilities { capabilities } => {
                app.seat_caps = match capabilities {
                    wayland_client::WEnum::Value(v) => v.bits(),
                    wayland_client::WEnum::Unknown(v) => v,
                };
            }
            wl_seat::Event::Name { name } => app.seat_name = name,
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlPointer,
        e: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            wl_pointer::Event::Enter {
                surface_x, surface_y, ..
            } => {
                PTR_ENTER.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV pointer.enter x={surface_x:.1} y={surface_y:.1}");
            }
            wl_pointer::Event::Leave { .. } => {
                PTR_LEAVE.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV pointer.leave");
            }
            wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            } => {
                PTR_MOTION.fetch_add(1, Ordering::Relaxed);
                if detail(&PTR_MOTION) {
                    println!("{TAG} EV pointer.motion t={time} x={surface_x:.1} y={surface_y:.1}");
                }
            }
            wl_pointer::Event::Button {
                button, state, ..
            } => {
                PTR_BUTTON.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV pointer.button b={button} state={state:?}");
            }
            wl_pointer::Event::Axis { .. } => {
                PTR_AXIS.fetch_add(1, Ordering::Relaxed);
            }
            wl_pointer::Event::Frame => {
                PTR_FRAME.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlKeyboard,
        e: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            wl_keyboard::Event::Keymap { format, size, .. } => {
                KBD_KEYMAP.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV keyboard.keymap format={format:?} size={size}");
            }
            wl_keyboard::Event::Enter { keys, .. } => {
                KBD_ENTER.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV keyboard.enter held={}", keys.len() / 4);
            }
            wl_keyboard::Event::Leave { .. } => {
                KBD_LEAVE.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV keyboard.leave");
            }
            wl_keyboard::Event::Key {
                key, state, time, ..
            } => {
                KBD_KEY.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV keyboard.key t={time} key={key} state={state:?}");
            }
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                KBD_MODS.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV keyboard.modifiers depressed={mods_depressed}");
            }
            wl_keyboard::Event::RepeatInfo { .. } => {
                KBD_REPEAT.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlTouch, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlTouch,
        e: wl_touch::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            wl_touch::Event::Down { x, y, .. } => {
                TCH_DOWN.fetch_add(1, Ordering::Relaxed);
                println!("{TAG} EV touch.down x={x:.1} y={y:.1}");
            }
            wl_touch::Event::Up { .. } => {
                TCH_UP.fetch_add(1, Ordering::Relaxed);
            }
            wl_touch::Event::Motion { x, y, .. } => {
                TCH_MOTION.fetch_add(1, Ordering::Relaxed);
                if detail(&TCH_MOTION) {
                    println!("{TAG} EV touch.motion x={x:.1} y={y:.1}");
                }
            }
            wl_touch::Event::Frame => {
                TCH_FRAME.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------------- main --

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sock = args.get(1).cloned().unwrap_or_else(|| "wayland-1".into());
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(150);
    let interval: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);

    let uid = unsafe { libc::geteuid() };
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
    let path = if sock.starts_with('/') {
        sock.clone()
    } else {
        format!("{dir}/{sock}")
    };

    println!(
        "{TAG} BEGIN pid={} uid={uid} path={path} secs={secs} interval={interval}",
        std::process::id()
    );

    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            println!("{TAG} FAIL stage=connect path={path} err={e}");
            println!("{TAG} END");
            return;
        }
    };
    let conn = match Connection::from_socket(stream) {
        Ok(c) => c,
        Err(e) => {
            println!("{TAG} FAIL stage=from_socket err={e}");
            println!("{TAG} END");
            return;
        }
    };
    let (globals, mut queue) = match registry_queue_init::<App>(&conn) {
        Ok(v) => v,
        Err(e) => {
            println!("{TAG} FAIL stage=registry_init err={e}");
            println!("{TAG} END");
            return;
        }
    };
    let qh = queue.handle();

    let compositor = globals.bind::<WlCompositor, _, _>(&qh, 1..=6, ());
    let shm = globals.bind::<WlShm, _, _>(&qh, 1..=1, ());
    let wm_base = globals.bind::<XdgWmBase, _, _>(&qh, 1..=6, ());
    let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=9, ());
    println!(
        "{TAG} BOUND globals={} compositor={} shm={} wm_base={} seat={}",
        globals.contents().clone_list().len(),
        compositor.is_ok() as u8,
        shm.is_ok() as u8,
        wm_base.is_ok() as u8,
        seat.is_ok() as u8
    );
    let (compositor, shm, wm_base, seat) = match (compositor, shm, wm_base, seat) {
        (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
        _ => {
            println!("{TAG} FAIL stage=bind — cannot continue");
            println!("{TAG} END");
            return;
        }
    };

    let mut app = App {
        shm,
        surface: None,
        pool: None,
        buffer: None,
        pixels: std::ptr::null_mut(),
        w: 640,
        h: 480,
        seat_caps: 0,
        seat_name: String::new(),
        configured: false,
        closed: false,
    };

    // Seat capabilities first: they decide which device objects exist, and the
    // line is printed whatever the value so 0 and "no seat at all" differ.
    if queue.roundtrip(&mut app).is_err() {
        println!("{TAG} FAIL stage=roundtrip_seat");
        println!("{TAG} END");
        return;
    }
    println!(
        "{TAG} SEATCAP name={:?} caps=0x{:x} pointer={} keyboard={} touch={}",
        app.seat_name,
        app.seat_caps,
        app.seat_caps & 1,
        (app.seat_caps >> 1) & 1,
        (app.seat_caps >> 2) & 1
    );

    // Create the device objects unconditionally. cosmic-comp advertises 0x7 and
    // adds all three at seat creation regardless of hardware, so gating on the
    // capability bits here would only hide a real seat with a wrong bitmask.
    let _pointer: WlPointer = seat.get_pointer(&qh, ());
    let _keyboard: WlKeyboard = seat.get_keyboard(&qh, ());
    let _touch: WlTouch = seat.get_touch(&qh, ());

    // Map the toplevel.
    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("wlinput".into());
    toplevel.set_app_id("com.leandros.wlinput".into());
    surface.commit();
    app.surface = Some(surface);

    // Wait for the first configure. Bounded, and the bound is reported: a
    // client that never got configure has learned nothing about input.
    let t0 = Instant::now();
    while !app.configured && t0.elapsed() < Duration::from_secs(20) {
        if queue.roundtrip(&mut app).is_err() {
            println!("{TAG} FAIL stage=roundtrip_configure");
            break;
        }
        if !app.configured {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    if !app.configured {
        println!(
            "{TAG} NOCONFIGURE after {:.1}s — the compositor did not answer our \
             xdg_surface. Any input result below is UNINTERPRETABLE.",
            t0.elapsed().as_secs_f64()
        );
    } else {
        app.draw(&qh);
        let _ = queue.roundtrip(&mut app);
    }

    // Census thread: prints the series and hard-stops the process, so a hang in
    // blocking_dispatch cannot turn into a run with no result.
    let start = Instant::now();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(interval.max(1)));
            let t = start.elapsed().as_secs_f64();
            println!("{}", census_line(t));
            if t >= secs as f64 {
                println!("{TAG} FINAL mapped={}", MAPPED.load(Ordering::Relaxed) as u8);
                println!("{}", census_line(t));
                println!("{TAG} END");
                std::io::Write::flush(&mut std::io::stdout()).ok();
                std::process::exit(0);
            }
        }
    });

    loop {
        if app.closed {
            println!("{TAG} closed by compositor");
            break;
        }
        if queue.blocking_dispatch(&mut app).is_err() {
            println!("{TAG} FAIL stage=dispatch (connection lost)");
            break;
        }
    }
    println!("{}", census_line(start.elapsed().as_secs_f64()));
    println!("{TAG} END");
}
