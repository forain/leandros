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

use std::os::unix::net::UnixStream;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, QueueHandle};

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

struct S;

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

/// Connect to one socket, dump its registry. Returns the number of globals, or
/// None if the socket could not be brought up.
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
    let (globals, _queue) = match registry_queue_init::<S>(&conn) {
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
