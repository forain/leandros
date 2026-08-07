// wl-globals — dump every wl_registry global advertised by every Wayland
// server reachable in $XDG_RUNTIME_DIR, and exit.
//
// Purpose: M9 Stage 0a. Decide whether cosmic-comp advertises
// `zwp_linux_dmabuf_v1` on our software EGL device. cosmic-comp gates that
// global (and `wl_drm`) on `!is_software` — kms/device.rs:760 -> kms/socket.rs:57
// — so its absence kills the cross-open dmabuf route for M4.
//
// INSTRUMENT NOTES (read before trusting any output of this program):
//
//  * It does NOT use `Connection::connect_to_env()`. The environment is ignored
//    entirely. `leandros-applet`, the obvious thing to extend for this job,
//    connects through the inherited `WAYLAND_SOCKET` fd that cosmic-panel hands
//    its applets — i.e. to cosmic-PANEL's EMBEDDED wayland server, not to
//    cosmic-comp. That server advertises wl_compositor + wl_shm + xdg_wm_base
//    and no dmabuf, which would look exactly like the "real negative" this
//    measurement is trying to find. Connecting by explicit socket path, and
//    dumping EVERY socket found, is what keeps the two apart.
//
//  * Absence of a line is the answer being looked for, and absence is also what
//    a broken dumper produces. So every pass prints:
//      - `TRY` before each connect, so a hang or a failure is localised;
//      - one `G` line per global, straight out of the registry with no filter;
//      - a `COUNT` line, so truncation of the tail is detectable;
//      - a `MATCH` line whose flags are computed by ONE routine applied to both
//        the interfaces under test AND to interfaces that must be present. If
//        the matcher were broken, `wl_compositor=0` and `wl_shm=0` would say so.
//    A run with sockets but no `G` lines, or with `G` lines but no `END`, is a
//    broken run and must not be read as a negative.
//
//  * SEAT PROBE (added after the registry dump): binds `wl_seat` and reports
//    what capabilities/name the compositor advertises, then — if a pointer or
//    keyboard capability is present — binds the matching object and reports
//    whether a keymap fd arrived. `got_caps=0`/`got_name=0` mean "no event
//    arrived within the roundtrip budget", which is NOT the same thing as the
//    bitmask/name being empty. Conflating "absent" with "zero" is exactly the
//    kind of bug this instrument exists to catch, so the two are always kept
//    in separate fields.

use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixStream;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

const TAG: &str = "[WLG]";

/// The interfaces the `MATCH` line reports on.
///
/// The first four are the CONTROL half: any live wayland server we could be
/// talking to advertises them, so a `MATCH` line that reports them absent means
/// the matcher (or the dump) is broken, not that the compositor is unusual.
/// The last four are the interfaces under test.
const PROBES: &[&str] = &[
    // controls — expected present
    "wl_compositor",
    "wl_shm",
    "wl_seat",
    "xdg_wm_base",
    // corroborating: cosmic-comp-only, absent from the panel's embedded server
    "zwlr_layer_shell_v1",
    "wl_output",
    // under test — the M9 Stage 0a question
    "zwp_linux_dmabuf_v1",
    "wl_drm",
];

/// Per-connection dispatch state. Holds nothing across sockets — a fresh `S`
/// is created for every socket so one compositor's seat data can never leak
/// into another socket's summary line.
#[derive(Default)]
struct S {
    // wl_seat.capabilities / wl_seat.name
    got_caps: bool,
    caps: u32,
    got_name: bool,
    name: Option<String>,
    // wl_seat.get_pointer / get_keyboard
    pointer_obj: bool,
    keyboard_obj: bool,
    // wl_keyboard.keymap
    keymap_fd: Option<OwnedFd>,
    keymap_size: u32,
}

impl Dispatch<WlRegistry, GlobalListContents> for S {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for S {
    fn event(
        state: &mut Self,
        _: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Capabilities { capabilities } => {
                state.got_caps = true;
                state.caps = match capabilities {
                    WEnum::Value(c) => c.bits(),
                    WEnum::Unknown(v) => v,
                };
            }
            wl_seat::Event::Name { name } => {
                state.got_name = true;
                state.name = Some(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for S {
    fn event(
        _: &mut Self,
        _: &WlPointer,
        _: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // We only care that the object exists and that binding it didn't
        // wedge the queue; the individual motion/button/frame events carry
        // nothing this instrument reports on.
    }
}

impl Dispatch<WlKeyboard, ()> for S {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { fd, size, .. } = event {
            state.keymap_fd = Some(fd);
            state.keymap_size = size;
        }
    }
}

/// Every `wayland-*` socket in `dir`, sorted, `.lock` files excluded.
fn wayland_sockets(dir: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("wayland-") && !n.ends_with(".lock") {
                v.push(n);
            }
        }
    }
    v.sort();
    v
}

/// Bind `wl_seat`, roundtrip for its capabilities/name, and — if a pointer or
/// keyboard capability showed up — bind that object too and roundtrip once
/// more for `wl_keyboard.keymap`. Never panics: every fallible step prints a
/// `SEATFAIL` line and returns instead of unwrapping.
fn probe_seat(
    sock: &str,
    globals: &wayland_client::globals::GlobalList,
    queue: &mut wayland_client::EventQueue<S>,
    seat_version: u32,
) {
    let qh = queue.handle();
    let bind_ver = seat_version.min(9);

    let seat: WlSeat = match globals.bind(&qh, bind_ver..=bind_ver, ()) {
        Ok(s) => s,
        Err(e) => {
            println!("{TAG} SEATFAIL sock={sock} stage=bind_seat err={e}");
            return;
        }
    };

    let mut state = S::default();

    for _ in 0..2 {
        if let Err(e) = queue.roundtrip(&mut state) {
            println!("{TAG} SEATFAIL sock={sock} stage=roundtrip err={e}");
            return;
        }
    }

    let caps = state.caps;
    let pointer_bit = caps & 1 != 0;
    let keyboard_bit = caps & 2 != 0;
    let touch_bit = caps & 4 != 0;

    println!(
        "{TAG} SEAT sock={sock} got_caps={} caps=0x{:x} pointer={} keyboard={} touch={} got_name={} name={} ver={}",
        state.got_caps as u8,
        caps,
        pointer_bit as u8,
        keyboard_bit as u8,
        touch_bit as u8,
        state.got_name as u8,
        state.name.as_deref().unwrap_or("(none)"),
        bind_ver,
    );

    if pointer_bit {
        let _pointer: WlPointer = seat.get_pointer(&qh, ());
        state.pointer_obj = true;
    }
    if keyboard_bit {
        let _keyboard: WlKeyboard = seat.get_keyboard(&qh, ());
        state.keyboard_obj = true;
    }

    if let Err(e) = queue.roundtrip(&mut state) {
        println!("{TAG} SEATFAIL sock={sock} stage=roundtrip_obj err={e}");
        return;
    }

    // keymap_fd is dropped (closed) at the end of this function's scope —
    // never mmap'd, never leaked into a later socket's state.
    let (keymap_fd_flag, keymap_size) = match &state.keymap_fd {
        Some(_) => (1u8, state.keymap_size),
        None => (0u8, 0u32),
    };

    println!(
        "{TAG} SEATOBJ sock={sock} pointer_obj={} keyboard_obj={} keymap_fd={} keymap_size={}",
        state.pointer_obj as u8, state.keyboard_obj as u8, keymap_fd_flag, keymap_size,
    );
}

/// Connect to one socket, dump its registry, then probe wl_seat. Returns the
/// number of globals, or None if the socket could not be brought up.
fn dump(dir: &str, sock: &str) -> Option<usize> {
    let path = format!("{dir}/{sock}");
    println!("{TAG} TRY sock={sock} path={path}");

    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            println!("{TAG} FAIL sock={sock} stage=connect err={e}");
            return None;
        }
    };
    let conn = match Connection::from_socket(stream) {
        Ok(c) => c,
        Err(e) => {
            println!("{TAG} FAIL sock={sock} stage=from_socket err={e}");
            return None;
        }
    };
    let (globals, mut queue) = match registry_queue_init::<S>(&conn) {
        Ok(v) => v,
        Err(e) => {
            println!("{TAG} FAIL sock={sock} stage=registry_init err={e}");
            return None;
        }
    };

    let mut list = globals.contents().clone_list();
    list.sort_by(|a, b| a.interface.cmp(&b.interface).then(a.name.cmp(&b.name)));

    println!("{TAG} OPEN sock={sock}");
    for g in &list {
        println!(
            "{TAG} G sock={sock} name={} iface={} ver={}",
            g.name, g.interface, g.version
        );
    }
    println!("{TAG} COUNT sock={sock} n={}", list.len());

    // One matcher, applied to controls and to the interfaces under test alike.
    let mut flags = String::new();
    for p in PROBES {
        let present = list.iter().any(|g| g.interface == *p);
        flags.push_str(&format!(" {p}={}", if present { 1 } else { 0 }));
    }
    println!("{TAG} MATCH sock={sock}{flags}");

    match list.iter().find(|g| g.interface == "wl_seat") {
        Some(g) => probe_seat(sock, &globals, &mut queue, g.version),
        None => println!("{TAG} SEAT sock={sock} absent=1"),
    }

    Some(list.len())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: u64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let delay = arg(1, 0); // seconds before the first pass
    let passes = arg(2, 1).max(1); // number of passes
    let interval = arg(3, 0); // seconds between passes

    let uid = unsafe { libc::geteuid() };
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));

    println!(
        "{TAG} BEGIN pid={} uid={uid} dir={dir} delay={delay} passes={passes} interval={interval}",
        std::process::id()
    );

    if delay > 0 {
        std::thread::sleep(std::time::Duration::from_secs(delay));
    }

    for pass in 1..=passes {
        let socks = wayland_sockets(&dir);
        println!(
            "{TAG} PASS {pass}/{passes} sockets={} [{}]",
            socks.len(),
            socks.join(",")
        );
        for s in &socks {
            dump(&dir, s);
        }
        println!("{TAG} PASSEND {pass}/{passes}");
        if pass < passes && interval > 0 {
            std::thread::sleep(std::time::Duration::from_secs(interval));
        }
    }

    println!("{TAG} END");
}
