//! mintest -- COSMIC toplevel probe over zcosmic_toplevel_info / _manager.
//!   mintest list                 one line per toplevel: app_id, title, states
//!   mintest min|unmin <app_id>   zcosmic_toplevel_manager set/unset_minimized, then list
//! Output lines start with `TL ` (state names: maximized minimized activated fullscreen sticky).
//! Exit 0 on success, 1 if the target toplevel was not found, 2 on setup failure.
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use cosmic_protocols::toplevel_info::v1::client::{zcosmic_toplevel_handle_v1, zcosmic_toplevel_info_v1};
use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry;
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1,
};

#[derive(Default)]
struct Tl {
    ext: Option<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1>,
    cosmic: Option<zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1>,
    app_id: String,
    title: String,
    states: Vec<u32>,
    closed: bool,
}

#[derive(Default)]
struct St {
    tls: Vec<Tl>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for St {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event,
             _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for St {
    fn event(st: &mut Self, _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
             ev: ext_foreign_toplevel_list_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = ev {
            st.tls.push(Tl { ext: Some(toplevel), ..Default::default() });
        }
    }
    event_created_child!(St, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for St {
    fn event(st: &mut Self, h: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
             ev: ext_foreign_toplevel_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(t) = st.tls.iter_mut().find(|t| t.ext.as_ref() == Some(h)) else { return };
        match ev {
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => t.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Title { title } => t.title = title,
            ext_foreign_toplevel_handle_v1::Event::Closed => t.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, ()> for St {
    fn event(_: &mut Self, _: &zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
             _: zcosmic_toplevel_info_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1, usize> for St {
    fn event(st: &mut Self, _: &zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
             ev: zcosmic_toplevel_handle_v1::Event, i: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let zcosmic_toplevel_handle_v1::Event::State { state } = ev {
            st.tls[*i].states = state.chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect();
        }
    }
}

impl Dispatch<zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1, ()> for St {
    fn event(_: &mut Self, _: &zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1,
             _: zcosmic_toplevel_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

fn pump(q: &mut EventQueue<St>, st: &mut St, ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        q.dispatch_pending(st).expect("dispatch");
        q.flush().expect("flush");
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        if let Some(guard) = q.prepare_read() {
            let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, (deadline - now).as_millis().max(1) as i32) } > 0 {
                guard.read().expect("read events");
            }
        }
    }
}

fn names(s: &[u32]) -> String {
    let n = ["maximized", "minimized", "activated", "fullscreen", "sticky"];
    let v: Vec<&str> = s.iter().map(|x| n.get(*x as usize).copied().unwrap_or("?")).collect();
    if v.is_empty() { "-".into() } else { v.join(",") }
}

fn list(st: &St) {
    for t in st.tls.iter().filter(|t| !t.closed) {
        println!("TL app_id={} title={:?} states={}", t.app_id, t.title, names(&t.states));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("list");
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => { println!("mintest: SETUP no compositor: {e}"); std::process::exit(2) }
    };
    let (globals, mut q) = registry_queue_init::<St>(&conn).expect("registry");
    let qh = q.handle();
    let mut st = St::default();
    let _list: ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1 = match globals.bind(&qh, 1..=1, ()) {
        Ok(x) => x,
        Err(e) => { println!("mintest: SETUP no ext_foreign_toplevel_list_v1: {e}"); std::process::exit(2) }
    };
    let info: zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1 = match globals.bind(&qh, 2..=3, ()) {
        Ok(x) => x,
        Err(e) => { println!("mintest: SETUP no zcosmic_toplevel_info_v1 >= 2: {e}"); std::process::exit(2) }
    };
    let mgr: Option<zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1> = globals.bind(&qh, 1..=4, ()).ok();
    let mut seen = 0;
    let mut settle = |q: &mut EventQueue<St>, st: &mut St, ms: u64| {
        for _ in 0..4 {
            pump(q, st, ms / 3);
            for i in seen..st.tls.len() {
                let c = info.get_cosmic_toplevel(st.tls[i].ext.as_ref().unwrap(), &qh, i);
                st.tls[i].cosmic = Some(c);
            }
            seen = st.tls.len();
        }
    };
    settle(&mut q, &mut st, 1500);
    match cmd {
        "list" => list(&st),
        "min" | "unmin" => {
            let want = args.get(2).cloned().unwrap_or_default();
            let Some(i) = st.tls.iter().position(|t| !t.closed && t.app_id == want) else {
                println!("mintest: NOTFOUND app_id={want}");
                list(&st);
                std::process::exit(1);
            };
            let Some(mgr) = mgr else { println!("mintest: SETUP no zcosmic_toplevel_manager_v1"); std::process::exit(2) };
            let h = st.tls[i].cosmic.as_ref().unwrap();
            if cmd == "min" { mgr.set_minimized(h) } else { mgr.unset_minimized(h) }
            settle(&mut q, &mut st, 1500);
            list(&st);
        }
        _ => { println!("usage: mintest list | min <app_id> | unmin <app_id>"); std::process::exit(2) }
    }
}
