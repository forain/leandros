# COSMIC capability matrix: what a user can actually do, and where it stops

**2026-08-07, x86_64/KVM, Linux box, fresh images, `--venus`.** Two runs on the
desktop as it actually runs today, not on whether it boots. Harnesses:
`artifacts/m12_caps.py` + `/bin/m12-caps` (the capability sweep),
`artifacts/m12c_input.py` + `/bin/m12c-input` (the follow-up that attributes the
one null result that mattered), `artifacts/m12_analyze.py` (the reduction).

Reproduce:

```
python3 scripts/mkfs-f2fs-populated.py f2fs-data0-x86_64.img x86_64
cp f2fs-data0-x86_64.img f2fs-data1-x86_64.img
LEANDROS_QEMU_EXTRA='-qmp unix:/tmp/leandros-qmp.sock,server,nowait' \
  python3 .claude/skills/run-leandros/driver.py start x86_64 uefi --venus
python3 .claude/skills/run-leandros/driver.py login root root
python3 -u artifacts/m12_caps.py /tmp/m12          # ~13 min, 36 captures
python3 -u artifacts/m12_analyze.py /tmp/m12
```

`[DRMSTAT]` requires `drivers/src/drm_device_interface.rs` `DRM_STATS = true`;
it is `false` on `main` and was flipped only for these runs. Nothing else about
the build differs.

## The matrix

| capability | works? | evidence | where it stops |
|---|---|---|---|
| Desktop composites | **yes** | wallpaper + full-width panel, 334k+ distinct colours, `m12-x86_64-idle-desktop.png` | — |
| Panel bar renders | **yes** | full-width dark bar, top edge | — |
| Clock ticks | **yes** | 216 px then 459 px change in `x=961..1029 y=5..25` across three idle captures 18 s apart | — |
| Client opens a toplevel | **yes** | `wlclient` maps: 208 563 px appear at `x=685..1233 y=133..521`, `m12-x86_64-one-toplevel.png` | — |
| Two toplevels at once | **yes** | second maps cascaded down-right, both stay visible, `m12-x86_64-two-toplevels.png` | — |
| Window borders / focus ring | **yes** | 1 px cyan active-window border, rounded corners, drawn on exactly one of the two windows | no titlebar, no buttons — `zxdg_decoration_manager_v1` IS advertised, `wlclient` never binds it |
| Compositor's advertised protocol surface | **yes, 54 globals** | `m12c-input-serial.log` `[WLG]` block: `wl_seat` v9, `xdg_wm_base` v7, `zwlr_layer_shell_v1` v5, `zxdg_decoration_manager_v1`, `ext_session_lock`, `ext_workspace_manager_v1`, `zcosmic_toplevel_manager_v1`, `zwp_virtual_keyboard_manager_v1`, `wp_cursor_shape_manager_v1` … | only `zwp_linux_dmabuf_v1` and `wl_drm` absent — expected, gated on `!is_software` |
| All COSMIC components stay alive | **yes** | pid census: session, comp, panel, bg, notifications, osd, launcher, app-library, workspaces, greeter, files-applet, idle, settings-daemon, busd, leandros-applet — **zero crashes in 13 min** | — |
| **Pointer moves / clicks** | **NO** | 868 evdev events reach the kernel ring during the sweep; flips stay at **1.00→1.21/s** — and CLICK, with **24** evdev events, also runs at 1.19/s, so the 0.2 excess does not track input volume and is the clock's own label width, not a response; frames at opposite screen corners **byte-identical** | nothing above raw evdev responds |
| **Cursor visible** | **NO** | `curs_up` = **0 per phase** for the whole session (1 upload ever, at startup); no cursor at any parked position | compositor never uploads a cursor image |
| **Keyboard reaches clients** | **NO** | `wlclient` logs `seat has keyboard` and then **never** `keyboard focus ENTER`; typing `abc`/`def`/`xyz` changes 0 px | no `wl_keyboard.enter` is ever sent |
| **Keybindings (Super, Super+/, Super+A)** | **NO** | all three: 0 px changed outside the clock | *and* `ERROR cosmic_settings_config::shortcuts: failed to read system shortcuts config 'system_actions': NoConfigDirectory` |
| **Move / resize / close a window** | **NO** | Super+drag, Super+right-drag, Super+Q ×2: 0 px changed; both windows still present in the final frame | input never arrives |
| **Applet menus open on click** | **NO** | three click targets on the panel: 0 px changed | input never arrives |
| Launcher / app library / terminal | **runs, but unreachable and empty** | `cosmic-launcher` and `cosmic-app-library` alive as daemons; **no terminal emulator anywhere in the 175-name `/bin`**; `/usr/share/applications` holds **one** `.desktop`, our own applet stub | nothing to launch, and no way to ask |
| Launching an application | **NO** | `cosmic-settings` started directly, alive as 3 pids, owns its `com.system76.CosmicSettings` socket, **empty log**, 0 px drawn at +6 s, +26 s and +74 s | runs but renders nothing |
| Applets in the panel | **1 of N** | only `com.system76.CosmicAppletTime.desktop` exists → `leandros-applet`, our stand-in; `ERROR Panel Entry Error: NoConfigDirectory` | real applets have no `.desktop`, so the panel never spawns them |

The one number that invites a wrong reading is the flip rate. It is not flat at
1.00/s across the run: IDLE 1.00, POINTER 1.21, CLICK 1.19, KEY_SUPER 1.15,
WIN1 1.08, SETTINGS 1.02. But POINTER carries **868** evdev events and CLICK
**24** — 36x the input for 0.02/s more flips — so the excess is uncorrelated
with input and is the clock repainting a wider label, not the compositor
reacting. Read the full per-phase table in `m12-caps-analysis.log`.

## Why every "NO" above is readable

The panel clock repaints once a second. So two captures seconds apart **must**
differ somewhere even when the thing under test did nothing, and a capture that
is byte-identical to the previous one is a *stale frame*, not a quiet desktop.
Measured first, before anything was provoked: `I1→I2` 216 px and `I2→I3` 459 px,
both confined to `x=961..1029 y=5..25`. Every "0 px changed" below that is
therefore a live compositor declining to react — not a frozen one, and not a
capture that missed.

The same cut in the other direction: `wlclient` mapping a window moved 208 563
px through the identical capture route minutes later. The instrument can see a
change of that size when there is one.

## The input chain, cut

`evpush` (the kernel's own evdev counter) rising while the compositor does
nothing is consistent with three different defects, so the second run split
them with an instrument that has no libinput in it:

```
open_event1: PASS          EVIOCGABS_max_32767: PASS
EVIOCGNAME_tablet: PASS    no_INPUT_PROP_DIRECT: PASS
EVIOCGBIT_has_EV_ABS: PASS EVIOCSCLOCKID_monotonic: PASS
EVIOCGBIT_ABS_has_XY: PASS
motion_abs_frame: PASS     motion_events=32
motion_ts_monotonic: PASS  motion_ts_subtick: PASS
```

**QEMU → virtio-tablet → kernel evdev ring → userspace `open`/`epoll`/`read`
works completely.** The break is strictly above raw evdev.

(`epoll_idle_no_false_wake: FAIL` in that run is the harness's own fault, not a
defect: the host was already injecting motion during the window in which
`evtest2` checks that an *idle* device does not wake epoll. Disregard it.)

Above evdev the trail goes cold with the shipped binaries. `RUST_LOG=…
smithay::backend::libinput=debug` is rejected — `would enable the DEBUG level
for the smithay::backend::libinput target` — because DEBUG is compiled out of
these builds. In 148 lines of session log, across every component, **nothing
mentions libinput, a seat, or an input device even once.**

Two facts that bear on it, neither yet proven causal:

* `/dev/input` is **not a listable directory** (`ls: cannot access
  '/dev/input'`) and `/sys/class/input` **does not exist** — while
  `/dev/input/event1` opens and reads fine. That asymmetry is deliberate and
  documented at `servers/vfs/src/lib.rs:1672`: `/dev/dri` was made enumerable
  and `/dev/input` was explicitly *not*, to avoid changing compositor input
  behaviour. The `libudev` shim compensates with a hardcoded event0/event1
  table (`ports/input-stack/shims/libudev/libudev.c:107-143`), but libinput
  does its own `fstat`/sysfs work on whatever udev hands it.
* Input to a compositor has **never** been demonstrated on LeandrOS. The M4
  mission was exactly "cursor follows virtio-tablet, keyboard reaches client";
  `artifacts/notes/m4-progress.md` shows anvil reaching `event0/1 via libseat ->
  libinput` in its init log and then dying at root cause #7 (PRIME/dmabuf)
  before any client ever saw an event. The lane pivoted to cosmic-comp and the
  question was never reopened.

## The three most limiting things

**1. No input reaches the compositor.** Not "the pointer is slow" — *nothing*.
Pointer motion, three click targets and three keybindings produced **zero**
changed pixels outside the clock and **zero** extra page flips, against a
compositor proven alive in the same seconds. This is limiting rather than merely
missing because everything else on the working side of the matrix is already
built and reachable only through it: windows map, composite, cascade, stack and
draw a focus ring correctly — and cannot be focused by clicking, moved, resized,
closed, or typed into. The desktop is a picture. Fix this first; several rows
above will flip without any other work.

**2. Nothing renders a window except a trivial test client.** `cosmic-settings`
is present, launches, stays alive as three pids, and claims its D-Bus name — and
draws nothing, with an empty log, over 74 s. `wlclient` (raw `wl_shm` +
`xdg_shell`, no toolkit) draws immediately through the same compositor. That
contrast localises the gap to the libcosmic/iced + `tiny-skia` path rather than
to Wayland or the compositor, and it is limiting because every real application
and every real applet is on that path. Note the same shape is already recorded
for the panel in `[[memfd-shm-gaps]]`.

**3. There is nothing to launch, and no way to ask.** `/bin` holds 175 names and
**not one terminal emulator**; `/usr/share/applications` holds exactly one
`.desktop`, which points at our own applet stand-in. So even with input fixed,
`Super` would open a launcher over an empty index, and the app library would list
nothing. This is limiting rather than cosmetic because it is the difference
between "a desktop you can demo" and "a desktop you can use" — and it is the
cheapest of the three to close (ship a terminal + real `.desktop` files).

A fourth, cheap and adjacent: `NoConfigDirectory` kills both the panel config
and the **shortcuts** config (`system_actions`). Even after input works,
keybindings may still do nothing until COSMIC's config directories are seeded.

## What was not tested, and why

* **aarch64 — wholly untested.** Both runs are x86_64/KVM. aarch64 is TCG-only
  on this box; the sweep is ~13 min under KVM and the pointer phases inject at
  60/s, which TCG will not sustain. Nothing here should be assumed to transfer.
* **`cosmic-workspaces` overview / `Super+Tab`** — not provoked. It is alive,
  but with no input path there was nothing to learn.
* **Whether `wl_seat` advertises a POINTER capability.** This is the single
  most useful next datum — it separates "libinput found no pointer device" from
  "found it, delivers nothing" — and no shipped tool prints seat capabilities.
  `wlclient` only reports the keyboard. It needs a ~20-line addition to
  `wl-globals` or to `wlclient`.
* **Anything below libinput inside the compositor**, for the compiled-out-DEBUG
  reason above.
* Out of scope by instruction and not probed: XWayland, PipeWire/audio,
  NetworkManager, UPower, accountsservice, greetd/cosmic-greeter, the
  `cosmic-workspaces` wgpu path, hotplug, VT switching, multi-seat.

## Landmines

* `wl-globals` was **absent from the x86_64 image** for the first run
  (`/bin/wl-globals: command not found`) — the crate source lives in this repo
  but the built binary only ever existed on the Mac. It is built and staged on
  the Linux box now. Check `Packed wl-globals` in the mkfs log before trusting a
  globals result.
* The `[DRMSTAT]` line is emitted from **IRQ context** and does not take the
  console lock, so it interleaves mid-line with guest `printf` output. Several
  lines in `m12-caps-serial.log` are spliced. Parse `[DRMSTAT]` by field NAME
  and expect corrupted neighbours; do not read a mangled application line as a
  crash.
* `cosmic-comp` writes nothing to the session log at `RUST_LOG=info`, and DEBUG
  is compiled out. A session log with no compositor lines is normal here and is
  not evidence that the compositor is quiet.
* Bounding boxes are useless unless the clock region is subtracted first: it
  differs between *any* two captures and drags every box up to the panel.
  `m12_analyze.py` measures that region from the idle phase rather than assuming
  it — and note the box grows when the clock gains a digit, which briefly looked
  like a successful window resize until it was checked.

Related: [[wayland-cosmic-plan]], [[gpu-accel-lane]], [[memfd-shm-gaps]],
[[console-authority]].
