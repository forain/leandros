//! blanktest: Wayland clients keep running while the outputs are powered off.
//!
//! A compositor that powers an output off (DPMS, `zwlr_output_power_v1` mode Off, which
//! is what cosmic-idle uses after its idle timeout) must keep sending frame callbacks, at
//! a throttled rate, and must not turn the output back on by itself. Otherwise every
//! client that paces itself on frame callbacks stops, and a window mapped while the
//! screen is off never gets its first callback (Firefox then never draws or loads a
//! page).
//!
//! Steps (run inside a Wayland session, WAYLAND_DISPLAY set):
//!   1. window A animates on frame callbacks with the outputs on (sanity);
//!   2. every output is powered off; A keeps animating for --off-secs (default 6 s) and
//!      must get at least --min-off callbacks (default 3);
//!   3. window B is mapped while the outputs are off and must get its first callback
//!      within 5 s;
//!   4. an idle inhibitor on a window of a powered-off output must not inhibit idle: an
//!      ext_idle_notification that went idle must not be `resumed` by it (idle daemons
//!      such as cosmic-idle power the outputs on when it is; zwp_idle_inhibit_v1 only
//!      applies while the surface is visible);
//!   5. no output may report mode On while the test holds it off;
//!   6. the outputs are powered on again.
//!
//! `blanktest --off` / `blanktest --on` only switch every output off or on and exit (the
//! compositor keeps the mode after the client is gone): a way to blank a screen remotely.
//! `blanktest --status` prints each output's power mode.
//!
//! Exit status: 0 pass, 1 fail, 2 setup error.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1, ext_idle_notifier_v1,
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1, zwlr_output_power_v1,
};

const W: i32 = 256;
const H: i32 = 256;

struct Win {
    surface: wl_surface::WlSurface,
    _xdg: xdg_surface::XdgSurface,
    _top: xdg_toplevel::XdgToplevel,
    buffer: Option<wl_buffer::WlBuffer>,
    configured: bool,
    animate: bool,
    frames: u32,
    first_frame: Option<Instant>,
}

#[derive(Default)]
struct OutPower {
    power: Option<zwlr_output_power_v1::ZwlrOutputPowerV1>,
    mode: Option<zwlr_output_power_v1::Mode>,
    failed: bool,
    turned_on: u32,
}

#[derive(Default)]
struct St {
    wins: Vec<Win>,
    outs: Vec<OutPower>,
    idled: u32,
    resumed: u32,
}

impl St {
    fn redraw(&mut self, i: usize, qh: &QueueHandle<St>) {
        let w = &mut self.wins[i];
        w.surface.frame(qh, i);
        w.surface.attach(w.buffer.as_ref(), 0, 0);
        w.surface.damage_buffer(0, 0, W, H);
        w.surface.commit();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for St {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event,
             _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for St {
    fn event(_: &mut Self, base: &xdg_wm_base::XdgWmBase, ev: xdg_wm_base::Event, _: &(),
             _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, usize> for St {
    fn event(st: &mut Self, xdg: &xdg_surface::XdgSurface, ev: xdg_surface::Event, i: &usize,
             _: &Connection, qh: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xdg.ack_configure(serial);
            if !st.wins[*i].configured {
                st.wins[*i].configured = true;
                st.redraw(*i, qh);
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, usize> for St {
    fn event(st: &mut Self, _: &wl_callback::WlCallback, ev: wl_callback::Event, i: &usize,
             _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = ev {
            let w = &mut st.wins[*i];
            w.frames += 1;
            w.first_frame.get_or_insert_with(Instant::now);
            if w.animate {
                st.redraw(*i, qh);
            }
        }
    }
}

impl Dispatch<zwlr_output_power_v1::ZwlrOutputPowerV1, usize> for St {
    fn event(st: &mut Self, _: &zwlr_output_power_v1::ZwlrOutputPowerV1,
             ev: zwlr_output_power_v1::Event, i: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let o = &mut st.outs[*i];
        match ev {
            zwlr_output_power_v1::Event::Mode { mode: WEnum::Value(m) } => {
                if m == zwlr_output_power_v1::Mode::On {
                    o.turned_on += 1;
                }
                o.mode = Some(m);
            }
            zwlr_output_power_v1::Event::Failed => o.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for St {
    fn event(st: &mut Self, _: &ext_idle_notification_v1::ExtIdleNotificationV1,
             ev: ext_idle_notification_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            ext_idle_notification_v1::Event::Idled => st.idled += 1,
            ext_idle_notification_v1::Event::Resumed => st.resumed += 1,
            _ => {}
        }
    }
}

delegate_noop!(St: wl_compositor::WlCompositor);
delegate_noop!(St: ext_idle_notifier_v1::ExtIdleNotifierV1);
delegate_noop!(St: zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1);
delegate_noop!(St: zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1);
delegate_noop!(St: ignore wl_seat::WlSeat);
delegate_noop!(St: wl_shm_pool::WlShmPool);
delegate_noop!(St: zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1);
delegate_noop!(St: ignore wl_shm::WlShm);
delegate_noop!(St: ignore wl_buffer::WlBuffer);
delegate_noop!(St: ignore wl_surface::WlSurface);
delegate_noop!(St: ignore wl_output::WlOutput);
delegate_noop!(St: ignore xdg_toplevel::XdgToplevel);

/// Dispatch events for up to `ms`, returning early once `done` holds.
fn pump(q: &mut EventQueue<St>, st: &mut St, ms: u64, done: impl Fn(&St) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        q.dispatch_pending(st).expect("dispatch");
        if done(st) {
            return true;
        }
        q.flush().expect("flush");
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let rem = (deadline - now).as_millis().max(1) as i32;
        if let Some(guard) = q.prepare_read() {
            let mut pfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, rem) } > 0 {
                guard.read().expect("read events");
            }
        }
    }
}

fn shm_file() -> File {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = format!("{dir}/blanktest-{}", std::process::id());
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true)
        .open(&path).expect("shm file");
    let _ = std::fs::remove_file(&path);
    f
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opt = |name: &str, d: u64| -> u64 {
        args.iter().position(|a| a == name)
            .and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(d)
    };
    let only = if args.iter().any(|a| a == "--off") {
        Some(zwlr_output_power_v1::Mode::Off)
    } else if args.iter().any(|a| a == "--on") {
        Some(zwlr_output_power_v1::Mode::On)
    } else {
        None
    };
    let off_secs = opt("--off-secs", 6);
    let min_off = opt("--min-off", 3) as u32;

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            println!("blanktest: SETUP no Wayland compositor: {e}");
            std::process::exit(2);
        }
    };
    let (globals, mut q) = registry_queue_init::<St>(&conn).expect("registry");
    let qh = q.handle();
    let mut st = St::default();

    let pm: zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1 =
        match globals.bind(&qh, 1..=1, ()) {
            Ok(p) => p,
            Err(e) => {
                println!("blanktest: SETUP no zwlr_output_power_manager_v1: {e}");
                std::process::exit(2);
            }
        };
    let outputs: Vec<wl_output::WlOutput> = globals.contents().clone_list().iter()
        .filter(|g| g.interface == "wl_output")
        .map(|g| globals.registry().bind(g.name, g.version.min(3), &qh, ()))
        .collect();
    for (i, o) in outputs.iter().enumerate() {
        st.outs.push(OutPower { power: Some(pm.get_output_power(o, &qh, i)), ..Default::default() });
    }
    pump(&mut q, &mut st, 2000, |s| s.outs.iter().all(|o| o.mode.is_some() || o.failed));
    let set_all = |st: &St, m| {
        for o in &st.outs {
            if let Some(p) = &o.power {
                p.set_mode(m);
            }
        }
    };
    let all_mode = |m| move |s: &St| s.outs.iter().all(|o| o.failed || o.mode == Some(m));

    if args.iter().any(|a| a == "--status") {
        println!("blanktest: {} outputs, modes {:?}", st.outs.len(),
                 st.outs.iter().map(|o| o.mode).collect::<Vec<_>>());
        std::process::exit(0);
    }
    if let Some(m) = only {
        set_all(&st, m);
        let ok = pump(&mut q, &mut st, 3000, all_mode(m));
        println!("blanktest: {} outputs -> {:?}: {}", st.outs.len(), m, if ok { "ok" } else { "timeout" });
        std::process::exit(if ok { 0 } else { 1 });
    }

    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).expect("wl_compositor");
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).expect("wl_shm");
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=2, ()).expect("xdg_wm_base");
    println!("blanktest: {} outputs, modes {:?}", st.outs.len(),
             st.outs.iter().map(|o| o.mode).collect::<Vec<_>>());

    let file = shm_file();
    let size = (W * H * 4) as u64;
    file.set_len(size * 2).expect("shm size");
    {
        use std::io::Write;
        let px: Vec<u8> = (0..(W * H * 2)).flat_map(|i| {
            if i < W * H { [0x40, 0x80, 0xc0, 0xff] } else { [0xc0, 0x40, 0x40, 0xff] }
        }).collect();
        (&file).write_all(&px).expect("shm fill");
    }
    let pool = shm.create_pool(file.as_fd(), (size * 2) as i32, &qh, ());
    let new_win = |st: &mut St, idx: usize, title: &str| {
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, idx);
        let top = xdg.get_toplevel(&qh, ());
        top.set_title(title.into());
        top.set_app_id("blanktest".into());
        let buffer = pool.create_buffer((idx as i32) * (W * H * 4), W, H, W * 4,
                                        wl_shm::Format::Xrgb8888, &qh, ());
        surface.commit();
        st.wins.push(Win { surface, _xdg: xdg, _top: top, buffer: Some(buffer), configured: false,
                           animate: true, frames: 0, first_frame: None });
    };

    let mut fails: Vec<String> = Vec::new();

    // 1. Outputs on: window A animates.
    set_all(&st, zwlr_output_power_v1::Mode::On);
    pump(&mut q, &mut st, 2000, all_mode(zwlr_output_power_v1::Mode::On));
    new_win(&mut st, 0, "blanktest A");
    let t = Instant::now();
    pump(&mut q, &mut st, 10_000, |s| s.wins[0].frames >= 10);
    let on_frames = st.wins[0].frames;
    let on_rate = on_frames as f64 / t.elapsed().as_secs_f64();
    println!("blanktest: on: window A {on_frames} callbacks, {on_rate:.1}/s");
    if on_frames < 3 {
        fails.push(format!("only {on_frames} callbacks with the outputs on"));
    }

    // 2. Outputs off: A keeps getting callbacks.
    set_all(&st, zwlr_output_power_v1::Mode::Off);
    let off_ok = pump(&mut q, &mut st, 3000, all_mode(zwlr_output_power_v1::Mode::Off));
    if !off_ok {
        fails.push("outputs did not report mode Off".into());
    }
    for o in &mut st.outs {
        o.turned_on = 0;
    }
    pump(&mut q, &mut st, 1500, |_| false);
    let base = st.wins[0].frames;
    let t = Instant::now();
    pump(&mut q, &mut st, off_secs * 1000, |_| false);
    let off_frames = st.wins[0].frames - base;
    println!("blanktest: off: window A {off_frames} callbacks in {:.1} s (need >= {min_off})",
             t.elapsed().as_secs_f64());
    if off_frames < min_off {
        fails.push(format!("window A got {off_frames} callbacks in {off_secs} s with the outputs off"));
    }

    // 3. A window mapped while the outputs are off gets its first callback.
    let t = Instant::now();
    new_win(&mut st, 1, "blanktest B");
    let configured = pump(&mut q, &mut st, 5000, |s| s.wins[1].configured);
    let first = pump(&mut q, &mut st, 5000, |s| s.wins[1].frames > 0);
    match (configured, first) {
        (false, _) => fails.push("window B (mapped while off) got no configure in 5 s".into()),
        (true, false) => fails.push("window B (mapped while off) got no frame callback in 5 s".into()),
        (true, true) => println!("blanktest: off: window B first callback after {:.2} s",
                                 (st.wins[1].first_frame.unwrap() - t).as_secs_f64()),
    }

    // 4. An idle inhibitor on a powered-off output's window does not resume idle.
    let notifier: Option<ext_idle_notifier_v1::ExtIdleNotifierV1> = globals.bind(&qh, 1..=1, ()).ok();
    let inhibit_mgr: Option<zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1> =
        globals.bind(&qh, 1..=1, ()).ok();
    let seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 1..=1, ()).ok();
    match (notifier, inhibit_mgr, seat) {
        (Some(notifier), Some(inhibit_mgr), Some(seat)) => {
            let note = notifier.get_idle_notification(1000, &seat, &qh, ());
            if pump(&mut q, &mut st, 5000, |s| s.idled > 0) {
                let inhibitor = inhibit_mgr.create_inhibitor(&st.wins[0].surface, &qh, ());
                pump(&mut q, &mut st, 2000, |s| s.resumed > 0);
                println!("blanktest: off: idle inhibitor on window A, resumed {}", st.resumed);
                if st.resumed > 0 {
                    fails.push("an idle inhibitor on a powered-off output resumed idle".into());
                }
                inhibitor.destroy();
            } else {
                println!("blanktest: off: idle notification never idled (input?), inhibit check skipped");
            }
            note.destroy();
        }
        _ => println!("blanktest: no ext_idle_notifier_v1/zwp_idle_inhibit_manager_v1, inhibit check skipped"),
    }

    // 5. The compositor kept the outputs off.
    let woke: u32 = st.outs.iter().map(|o| o.turned_on).sum();
    let still_off = st.outs.iter().all(|o| o.failed || o.mode == Some(zwlr_output_power_v1::Mode::Off));
    println!("blanktest: off: mode On events {woke}, still off {still_off}");
    if woke > 0 || !still_off {
        fails.push("an output was turned back on while the test held it off".into());
    }

    // 6. Outputs back on.
    st.wins.iter_mut().for_each(|w| w.animate = false);
    set_all(&st, zwlr_output_power_v1::Mode::On);
    pump(&mut q, &mut st, 3000, all_mode(zwlr_output_power_v1::Mode::On));

    if fails.is_empty() {
        println!("blanktest: PASS");
    } else {
        for f in &fails {
            println!("blanktest: FAIL {f}");
        }
    }
    std::process::exit(if fails.is_empty() { 0 } else { 1 });
}
