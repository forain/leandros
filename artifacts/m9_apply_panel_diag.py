#!/usr/bin/env python3
"""LEANDROS-DEBUG (m9) DIAGNOSTIC patch for cosmic-panel.

NOT A SHIPPABLE FIX. Applies to the local build tree
~/code/leandros-artifacts/m6-session-bins/src/cosmic-panel (which already carries
M7t-era LEANDROS-DEBUG checkpoints), never to ~/code/cosmic-epoch.

Answers exactly one question: which of PanelSpace::render's three gates
(space_event / is_dirty / has_frame) stays shut after the panel's first and only
bar frame, and whether the applet's 1 Hz commits reach the panel's embedded
compositor at all.

Requires kernel/src/syscall.rs DBG_SERIAL_WRITE = true (SYS_DBG_SERIAL_WRITE=590),
which frames each line as `[UCK] ...` straight on the serial TX.
"""
import os
import sys

ROOT = os.path.expanduser(
    "~/code/leandros-artifacts/m6-session-bins/src/cosmic-panel/cosmic-panel-bin/src")

HELPER = '''
/// LEANDROS-DEBUG (m9): rate-limited counting checkpoint. Emits on the first
/// call and then every `every`-th, so a per-frame path costs one serial line
/// per N hits instead of drowning the capture.
pub fn ckpt_every(
    site: &str,
    ctr: &std::sync::atomic::AtomicU64,
    every: u64,
    extra: &str,
) {
    use std::sync::atomic::Ordering;
    let n = ctr.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n % every == 0 {
        ckpt(&format!("m9 {site} n={n} {extra}"));
    }
}

fn main() -> Result<()> {'''

RENDER_OLD = '''    ) -> anyhow::Result<()> {
        if self.space_event.get().is_some()'''

RENDER_NEW = '''    ) -> anyhow::Result<()> {
        // LEANDROS-DEBUG (m9): which gate is shut? Emitted before the early
        // return so a permanently-closed space_event gate is visible too.
        {
            use std::sync::atomic::AtomicU64;
            static N: AtomicU64 = AtomicU64::new(0);
            crate::ckpt_every(
                "render_gate",
                &N,
                64,
                &format!(
                    "space_event={} actual={}x{} dims={}x{} is_dirty={} has_frame={}",
                    self.space_event.get().is_some(),
                    self.actual_size.w,
                    self.actual_size.h,
                    self.dimensions.w,
                    self.dimensions.h,
                    self.is_dirty,
                    self.has_frame
                ),
            );
        }
        if self.space_event.get().is_some()'''

FRAME_OLD = '''        if Some(surface) == self.layer.as_ref().map(|l| l.wl_surface()) {
            self.has_frame = true;'''

FRAME_NEW = '''        if Some(surface) == self.layer.as_ref().map(|l| l.wl_surface()) {
            // LEANDROS-DEBUG (m9): did cosmic-comp send a frame callback for
            // our layer surface? This is the only writer of has_frame.
            {
                use std::sync::atomic::AtomicU64;
                static N: AtomicU64 = AtomicU64::new(0);
                crate::ckpt_every("layer_frame_cb", &N, 16, "");
            }
            self.has_frame = true;'''

COMMIT_OLD = '''        if role == "xdg_toplevel".into() {
            on_commit_buffer_handler::<GlobalState>(surface);
            self.space.dirty_window(&dh, surface);'''

COMMIT_NEW = '''        if role == "xdg_toplevel".into() {
            // LEANDROS-DEBUG (m9): does the applet's 1 Hz commit reach the
            // panel's embedded compositor? This is what sets is_dirty.
            {
                use std::sync::atomic::AtomicU64;
                static N: AtomicU64 = AtomicU64::new(0);
                crate::ckpt_every("applet_commit", &N, 16, "");
            }
            on_commit_buffer_handler::<GlobalState>(surface);
            self.space.dirty_window(&dh, surface);'''

EDITS = [
    ("main.rs", "fn main() -> Result<()> {", HELPER),
    ("space/render.rs", RENDER_OLD, RENDER_NEW),
    ("space/wrapper_space.rs", FRAME_OLD, FRAME_NEW),
    ("xdg_shell_wrapper/server/handlers/compositor.rs", COMMIT_OLD, COMMIT_NEW),
]


def main():
    revert = len(sys.argv) > 1 and sys.argv[1] == "--revert"
    for rel, old, new in EDITS:
        p = os.path.join(ROOT, rel)
        src = open(p).read()
        a, b = (new, old) if revert else (old, new)
        if src.count(a) != 1:
            print(f"SKIP {rel}: anchor count={src.count(a)} (already {'reverted' if revert else 'applied'}?)")
            continue
        open(p, "w").write(src.replace(a, b, 1))
        print(f"{'REVERTED' if revert else 'PATCHED'} {rel}")


if __name__ == "__main__":
    main()
