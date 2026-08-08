# libcosmic applications do not fail to render — they block before they ever render

**aarch64, Mac, 2026-08-08.** Four QEMU runs, fresh images each time, no COSMIC
source modified at any point (`git -C ../cosmic-epoch status --porcelain` empty
throughout, submodules pinned at `epoch-1.3.0`).

Harnesses: `artifacts/m15_iced.py` + `artifacts/m6-session-data/m15-iced`,
`artifacts/m15b_iced.py` + `artifacts/m6-session-data/m15b-iced`.

## The claim this replaces

The standing record said a raw `wl_shm` client draws instantly while
`cosmic-settings` is *alive, owns its D-Bus name, logs nothing and paints 0 px*,
and concluded that the gap is **libcosmic / iced / `tiny-skia`** — the same shape
as cosmic-panel "presenting fresh buffers while rendering nothing into them", and
that the two were probably one investigation.

Every part of that is wrong except the pixel count.

## What was measured

### Run 1 — get the subject's own stderr, and turn on `WAYLAND_DEBUG`

`cosmic-session` spawns children through `launch_pad` with stderr piped and
registers no `on_stderr` handler (`cosmic-session/src/comp.rs:122-134`), so a
child's diagnostics go nowhere. The guest script therefore does not launch the
subject through `cosmic-session` at all: it brings the session up with its output
redirected to a file (which also keeps the console responsive past the ~180 s
saturation point), then starts `wlclient` and `cosmic-settings` from the shell
with the environment `cosmic-session`'s children would have inherited.

`cosmic-settings` wrote **1775 bytes and then nothing for 75 s**:

```
INFO  Current Locale: []
INFO  Selecting translations for domain "cosmic_settings"
ERROR error loading system dark theme, error: GetKey("list_button", ...)
        at libcosmic/96a8204/src/theme/mod.rs:95 on main
```

`WAYLAND_DEBUG=1` was set for that entire window and produced **not one line**.
No `wl_display.get_registry`, no surface, no buffer, no commit. The process never
opened a Wayland connection. It is not a rasteriser that draws nothing; it never
reaches a renderer.

The same boot's screendump (`control-desktop-wlclient-t19.png`) shows the Orion
wallpaper, the panel bar, a legible ticking clock and `wlclient`'s 480x320
gradient window, all composited correctly. The compositor is exonerated by
photograph rather than by argument.

The session log carries one line at the exact second `cosmic-settings` started:

```
busd::peers: unknown destination: com.system76.CosmicSettings
```

### The mechanism

`cosmic-settings` `main()` builds `cosmic::app::Settings::default()` — which is
where the theme error comes from — and then calls
`cosmic::app::run_single_instance` (`cosmic-settings/src/main.rs:216-221`). That
function makes a **blocking** zbus method call to its own `APP_ID` to discover
whether another instance is already running, and constructs the iced application
only in the else-branch (`libcosmic/96a8204/src/app/mod.rs:214-252`).

`busd` 0.5.0 — which we ship — answers a method call addressed to a well-known
name nobody owns by dropping it:

```rust
// busd-0.5.0/src/peers.rs:265-270
BusName::WellKnown(name) => {
    let dest = match self.name_registry().await.lookup(name.clone()) {
        Some(dest) => dest,
        None => bail!("unknown destination: {}", name),
    };
```

and the caller merely `warn!`s (`:222-227`). No
`org.freedesktop.DBus.Error.ServiceUnknown` is ever sent back, and busd has no
server-side reply timeout the way `dbus-daemon` does. zbus imposes no client-side
one either. **A blocking caller waits forever.**

The process is blocked, not dead: `run_single_instance`'s two exit paths both log
(`"Successfully activated another instance"` / `"Another instance is running"`,
or a `color_eyre` report on `Err`) and neither line appears.

### Run 2 — the discriminator

`run_single_instance` honours `COSMIC_SINGLE_INSTANCE=false` and returns
`run::<App>()` before touching D-Bus (`mod.rs:207-212`). So the same binary, in
the same session, one environment variable apart. CTRL runs first on purpose: if
the working arm ran first it would own the name and CTRL's probe would then
succeed and exit 0, which looks like a pass for entirely the wrong reason.

| arm | env | stderr | screendump |
|---|---|---|---|
| CTRL | default | **704 B**, stops at the theme line | nothing appears in 3 shots over 35 s (`ctrl-single-instance-on-t29.png`) |
| FIX | `COSMIC_SINGLE_INSTANCE=false` | **38923 B**, continues into `cosmic-text`/`fontdb` and then `-> wl_display#1.get_registry(new id wl_registry#2)` | window appears by t+36 s, box `(180,8)-(1100,664)` (`fix-single-instance-off-t74.png`) |

`fix-single-instance-off-t74.png` is `cosmic-settings` fully rendered — sidebar,
icons, text, list rows, scrollbar, rounded corners, theming. **libcosmic + iced +
`tiny-skia` work correctly on LeandrOS/aarch64.**

### Run 3 — the obvious fix regresses the session, and is NOT landed

A two-hunk patch to `busd/src/peers.rs` that replies
`org.freedesktop.DBus.Error.ServiceUnknown` to an undeliverable method call
(gated on `Type::MethodCall` without `NoReplyExpected`) compiles clean and was
staged for both arches. The image built from it **crash-loops**:
`run3-busd-patch-regression-serial.log` has **9 `[EXC] EL0 Fault!` records**,
most of them a null dereference at `FAR=0x880` with `x0=0` at the same code
offset in successively higher PIDs, and the session never got past
`wayland-1 after 1s`. Runs 1, 2 and 4 have **zero** `EL0 Fault` lines. The image
delta against run 2 was busd and nothing else, so the attribution is clean.

busd was rebuilt from stock source and re-staged; the patch is not in the tree.
**The right fix is known but is not this patch as written**, and finding out what
the reply breaks is its own investigation.

### Run 4 — the revert is proven, and the result replicates

Same harness, image rebuilt with stock busd:
`run4-revert-replication-serial.log` has **0 `EL0 Fault` lines**, and the two arms
reproduce run 2 on an independent boot — CTRL nothing across 3 shots in 35 s, FIX
the same window at the same place at t+36 s (changed samples 32145 against run 2's
32147, `fix-window-appears-t36.png`). So the finding is not a one-boot artifact,
and the tree is back to a state whose session is healthy.

## Consequences for other items

`strings` over the staged aarch64 binaries: `COSMIC_SINGLE_INSTANCE` appears in
**cosmic-settings, cosmic-launcher, cosmic-app-library, cosmic-workspaces and
cosmic-osd** — and in none of the others. Three of those are the components
recorded elsewhere as "built, staged, successfully launched every boot, zero
restarts, and permanently invisible", attributed entirely to there being no
keybinding able to raise them. That attribution now has a competing explanation
that must be excluded before the keybinding one is believed: they may have been
blocked in this same probe at startup, every boot, since they were first staged.

**The cheapest next measurement is a census, not an experiment**: capture a full
session log (not a tail) and count `busd::peers: unknown destination:` by name.
One `com.system76.Cosmic*` line per single-instance component is confirmation;
their absence refutes it.

## What this says about the panel

They are not one bug. cosmic-panel rasterises its bar with `iced_tiny_skia` into
a `MemoryRenderBuffer` and then composites that through smithay's `GlesRenderer`
over EGL (`cosmic-panel-bin/src/space/panel_space.rs:1397-1434`,
`src/space/render.rs:280-308`); the `mesa-shared:*` memfds seen by `mm::gap2` are
Mesa's swrast Wayland WSI, not a toolkit allocation. libcosmic applications reach
`wl_shm` through `softbuffer` instead. The shared component is
`iced_tiny_skia::Renderer::draw` — and this run photographs that code drawing a
complete, correct settings window, so it is not the panel's problem either.
cosmic-panel also reaches its render path, which cosmic-settings never did.

## Instrument notes for whoever runs this next

- `WAYLAND_DEBUG=1` is the right first instrument for "never commits" vs "commits
  blank": it separates them with no source change on either side, and its
  *absence* of output is as informative as its content.
- `$!` does not yield a usable pid under brush here, and `/proc` lists no numeric
  pid entries, so neither `ls -d /proc/$!` nor an `ls /proc` diff can witness
  aliveness. Aliveness had to be argued from which log lines did *not* appear.
- A solid-colour census misses a gradient window. The first pass of run 1 scored
  `wlclient` as invisible because its 480x320 window is a gradient and never
  registers as a dominant colour; the screenshot shows it plainly. **Look at the
  frame before trusting arithmetic over it.**
- `head -c` / `tail -c` on a `/data` (f2fs) log is the safe exfiltration route:
  `WAYLAND_DEBUG` on a spinning client would fill a tmpfs and OOM a 2 GiB guest.
