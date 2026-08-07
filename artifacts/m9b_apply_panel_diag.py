#!/usr/bin/env python3
"""LEANDROS-DEBUG (m9b) DIAGNOSTIC patch for cosmic-panel — PER-SPACE keying.

NOT A SHIPPABLE FIX. Applies to the local build tree
~/code/leandros-artifacts/m6-session-bins/src/cosmic-panel, never to
~/code/cosmic-epoch.

Supersedes m9_apply_panel_diag.py, whose `n % 64` rate limit was per-SITE. With
two PanelSpaces (default config: "Panel" top + "Dock" bottom) calling render()
in strict alternation, every 64th call is always the SAME space, so one space is
invisible in the capture. Here the checkpoint is keyed by "<config name>:<anchor>"
and emitted whenever THAT key's value CHANGES (plus a heartbeat every 4096 hits
of that key), so every space reports itself.

Also instruments the toplevel->space routing, because
SpaceContainer::add_window silently DROPS a toplevel whose wl client matches no
space's clients_{left,center,right} — the exact shape of "applet alive and
committing, but actual_size stays padding-only".

Requires kernel/src/syscall.rs DBG_SERIAL_WRITE = true (SYS_DBG_SERIAL_WRITE=590),
which frames each line as `[UCK] ...` straight on the serial TX.
"""
import os
import sys

ROOT = os.path.expanduser(
    "~/code/leandros-artifacts/m6-session-bins/src/cosmic-panel/cosmic-panel-bin/src")

HELPER = '''
/// LEANDROS-DEBUG (m9b): per-KEY change-triggered checkpoint. Emits when this
/// key's value differs from the last one emitted for the same key, and as a
/// heartbeat every 4096 hits. Unlike a per-site `n % N` sampler this cannot
/// alias away a space that shares the call site with a busier sibling.
pub fn ckpt_space(key: &str, val: &str) {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static M: Mutex<Option<HashMap<String, (String, u64)>>> = Mutex::new(None);
    let emit = {
        let mut g = match M.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let m = g.get_or_insert_with(HashMap::new);
        let e = m.entry(key.to_string()).or_insert((String::new(), 0));
        e.1 += 1;
        let n = e.1;
        if e.0 != val || n % 4096 == 0 {
            e.0 = val.to_string();
            Some(n)
        } else {
            None
        }
    };
    if let Some(n) = emit {
        ckpt(&format!("m9b [{key}] n={n} {val}"));
    }
}

fn main() -> Result<()> {'''

RENDER_OLD = '''    ) -> anyhow::Result<()> {
        if self.space_event.get().is_some()'''

RENDER_NEW = '''    ) -> anyhow::Result<()> {
        // LEANDROS-DEBUG (m9b): which SPACE is gated, and on what. Keyed by
        // config name + anchor so "Panel" and "Dock" cannot alias each other.
        {
            let (nc, nc_bound) = self
                .clients_center
                .try_lock()
                .map(|v| (v.len(), v.iter().filter(|c| c.client.is_some()).count()))
                .unwrap_or((999, 999));
            let (nl, nl_bound) = self
                .clients_left
                .try_lock()
                .map(|v| (v.len(), v.iter().filter(|c| c.client.is_some()).count()))
                .unwrap_or((999, 999));
            let (nr, nr_bound) = self
                .clients_right
                .try_lock()
                .map(|v| (v.len(), v.iter().filter(|c| c.client.is_some()).count()))
                .unwrap_or((999, 999));
            crate::ckpt_space(
                &format!("{}:{:?}", self.config.name, self.config.anchor),
                &format!(
                    "actual={}x{} dims={}x{} dirty={} frame={} sev={} elems={} unmapped={} \\
                     c_l={}/{} c_c={}/{} c_r={}/{}",
                    self.actual_size.w,
                    self.actual_size.h,
                    self.dimensions.w,
                    self.dimensions.h,
                    self.is_dirty,
                    self.has_frame,
                    self.space_event.get().is_some(),
                    self.space.elements().count(),
                    self.unmapped_windows.len(),
                    nl_bound, nl, nc_bound, nc, nr_bound, nr
                ),
            );
        }
        if self.space_event.get().is_some()'''

# The ONLY writer of has_frame=true: cosmic-comp's frame callback for our layer
# surface. Its RATE is the panel's maximum redraw rate, because render() sets
# has_frame=false again on every successful commit.
FRAME_OLD = '''    fn frame(&mut self, surface: &c_wl_surface::WlSurface, _time: u32) {
        if Some(surface) == self.layer.as_ref().map(|l| l.wl_surface()) {
            self.has_frame = true;'''

FRAME_NEW = '''    fn frame(&mut self, surface: &c_wl_surface::WlSurface, _time: u32) {
        if Some(surface) == self.layer.as_ref().map(|l| l.wl_surface()) {
            // LEANDROS-DEBUG (m9b): count layer-surface frame callbacks. This
            // rate, not the size gate, bounds how often the bar can redraw.
            {
                use std::sync::atomic::{AtomicU64, Ordering};
                static N: AtomicU64 = AtomicU64::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 4 || n % 8 == 0 {
                    crate::ckpt(&format!("m9b layer_frame_cb n={n} {}", self.config.name));
                }
            }
            self.has_frame = true;'''

# A completed bar render (the commit that actually puts new pixels on screen).
RENDERED_OLD = '''                let wl_surface = self.layer.as_ref().unwrap().wl_surface().clone();
                wl_surface.frame(qh, wl_surface.clone());
                wl_surface.commit();

                self.is_dirty = false;
                self.has_frame = false;'''

RENDERED_NEW = '''                let wl_surface = self.layer.as_ref().unwrap().wl_surface().clone();
                wl_surface.frame(qh, wl_surface.clone());
                wl_surface.commit();

                // LEANDROS-DEBUG (m9b): a bar frame actually reached the
                // compositor. Compare its count with layer_frame_cb's.
                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static N: AtomicU64 = AtomicU64::new(0);
                    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
                    if n <= 4 || n % 8 == 0 {
                        crate::ckpt(&format!("m9b bar_committed n={n} {}", self.config.name));
                    }
                }
                self.is_dirty = false;
                self.has_frame = false;'''

# SpaceContainer::add_window silently drops an unmatched toplevel.
ADDWIN_OLD = '''        if let Some(space) = self.space_list.iter_mut().find(|space| {
            space
                .clients_center
                .lock()
                .unwrap()
                .iter()
                .chain(space.clients_left.lock().unwrap().iter())
                .chain(space.clients_right.lock().unwrap().iter())
                .any(|c| c.client.as_ref().zip(w_client.as_ref()).is_some_and(|c| c.0.id() == *c.1))
        }) {
            space.add_window(s_top_level);
        }
    }'''

ADDWIN_NEW = '''        // LEANDROS-DEBUG (m9b): does the new toplevel find a home space? An
        // unmatched client is DROPPED here with no log at any level.
        {
            // Only BOUND clients (client.is_some()) are listed: the serial
            // checkpoint is capped at 256 bytes and the unbound ones (no
            // process staged for that applet) carry no id to compare.
            let mut who = String::new();
            for space in self.space_list.iter() {
                for (tag, cl) in [
                    ("L", &space.clients_left),
                    ("C", &space.clients_center),
                    ("R", &space.clients_right),
                ] {
                    if let Ok(v) = cl.try_lock() {
                        for c in v.iter() {
                            if let Some(x) = c.client.as_ref() {
                                who.push_str(&format!(
                                    " {}/{}/{:?}",
                                    space.config.name,
                                    tag,
                                    x.id()
                                ));
                            }
                        }
                    }
                }
            }
            crate::ckpt(&format!("m9b add_window w={:?} bound:{}", w_client, who));
        }
        if let Some(space) = self.space_list.iter_mut().find(|space| {
            space
                .clients_center
                .lock()
                .unwrap()
                .iter()
                .chain(space.clients_left.lock().unwrap().iter())
                .chain(space.clients_right.lock().unwrap().iter())
                .any(|c| c.client.as_ref().zip(w_client.as_ref()).is_some_and(|c| c.0.id() == *c.1))
        }) {
            crate::ckpt(&format!("m9b add_window MATCHED space={}", space.config.name));
            space.add_window(s_top_level);
        } else {
            crate::ckpt("m9b add_window UNMATCHED -- toplevel DROPPED");
        }
    }'''

TOPLEVEL_OLD = '''    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());

        self.space.add_window(window);'''

TOPLEVEL_NEW = '''    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // LEANDROS-DEBUG (m9b): every toplevel the embedded server sees.
        crate::ckpt("m9b new_toplevel");
        let window = Window::new_wayland_window(surface.clone());

        self.space.add_window(window);'''

EDITS = [
    ("main.rs", "fn main() -> Result<()> {", HELPER),
    ("space/render.rs", RENDER_OLD, RENDER_NEW),
    ("space/render.rs", RENDERED_OLD, RENDERED_NEW),
    ("space/wrapper_space.rs", FRAME_OLD, FRAME_NEW),
    ("space_container/wrapper_space.rs", ADDWIN_OLD, ADDWIN_NEW),
    ("xdg_shell_wrapper/server/handlers/xdg_shell.rs", TOPLEVEL_OLD, TOPLEVEL_NEW),
]


def main():
    revert = len(sys.argv) > 1 and sys.argv[1] == "--revert"
    rc = 0
    for rel, old, new in EDITS:
        p = os.path.join(ROOT, rel)
        src = open(p).read()
        # The main.rs helper ENDS with its own anchor line, so the anchor count
        # stays 1 after a successful apply and a re-run would duplicate the
        # helper. Guard on the marker instead.
        marker = "pub fn ckpt_space"
        if rel == "main.rs" and (marker in src) != revert:
            print(f"SKIP {rel}: already {'applied' if not revert else 'reverted'}")
            continue
        a, b = (new, old) if revert else (old, new)
        if src.count(a) != 1:
            print(f"SKIP {rel}: anchor count={src.count(a)} "
                  f"(already {'reverted' if revert else 'applied'}?)")
            rc = 1
            continue
        open(p, "w").write(src.replace(a, b, 1))
        print(f"{'REVERTED' if revert else 'PATCHED'} {rel}")
    return rc


if __name__ == "__main__":
    sys.exit(main())
