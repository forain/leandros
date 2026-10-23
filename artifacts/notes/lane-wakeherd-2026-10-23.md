# lane/wakeherd (L3.5) 2026-10-23 -- FIXED

## (a) evdev -- REFUTED as a live bug (stale text), no code change
`wake_pollers()` no longer exists; herd1 (08be3f47) keyed the wake per device. Measured on main (aarch64 QEMU, `sched.wakestat=1`,
`evherdtest.py 6000`, `/proc/wakestat` diff of the `servers/evdev/src/lib.rs:585` row):
calls 117, tagged 117, woken 30 (0.26 per wake), **allmask 0** (herd1 baseline: 94 calls / 132 woken / 132 allmask; pre-herd1 broadcast 13/wake).
No broadcast-mask parker is reached by the evdev wake, so there is nothing left to key in `fd_wake_tag`. The stale TODO.md line is deleted.

## (b) net daemon 100 ms broadcast -- gated off by default
Audit: boot with `sched.wakestat=1 net.bcastnet=1` (= old behaviour), `nettag2test` + evherdtest; `outcome servers/net/src/lib.rs:3312` (the broadcast):
tgid 37: useful 0 / spurious 12; tgid 18: 0 / 12; tgid 42: 0 / 3. **Useful = 0**, 27 spurious. (`/proc/net/nic`: broadcast 14 in 2234 passes.)
Change: `net.bcastnet=1` knob (default off, `syncobj_herd` pattern) in servers/net/src/lib.rs. Default boot: `netd: broadcast 0`, tagged wakes 101 in 2320 passes
(old: broadcast 14, tagged 90), no 3312 outcome row. nettag2test 9/9 PASS both ways.
Not measured here: sustained transfer / laptop idle+YouTube (follow-up under the laptop lock).
Build note: the shared relibc worktree had another lane's uncommitted broken edits; built with LEANDROS_RELIBC_DIR pointing at a clean HEAD checkout.
