# M7z — pink-rectangle desktop regression (aarch64)

## Task
12:15 aarch64 image (first to stage /bin/cosmic-idle + /bin/cosmic-greeter) shows a
solid PINK rectangle instead of the panel+wallpaper; cursor frozen. Reference-good =
M7w. Only kernel-unchanged; delta = the two new session binaries.

## Static analysis (done first, before booting)
- **cosmic-idle** (`m6-session-bins/src/cosmic-idle/`):
  - `cosmic-idle-config/src/lib.rs` default `screen_off_time = Some(15*60*1000)` = **15 min**.
    No CosmicIdle config staged anywhere (grep of m6-session-data = none) → default applies →
    the fade fires only after ~15 min of no input. Headless boot has zero input → idle
    accrues from session start.
  - `fade_black.rs`: on idle it creates a `zwlr_layer_shell` **Overlay** surface,
    `set_anchor(all)`, `set_exclusive_zone(-1)`, viewport-scaled to full output, attached to a
    `wp_single_pixel_buffer` `create_u32_rgba_buffer(0,0,0,alpha)` (**black**, rising alpha),
    5 s `EaseInOut` fade driven by frame callbacks.
  - On pointer `Enter` to the fade surface it calls `pointer.set_cursor(serial, None,…)` →
    **hides the cursor**. The overlay has no input region set (default = whole surface) so it
    grabs pointer focus. → explains "cursor does not move/respond".
  - After 5 s (`fade_done`): `output_power.set_mode(Off)` (DPMS) then `fade_surface=None`
    (overlay dropped), then after 500 ms `lock_screen()` runs `loginctl lock-session` (absent →
    just logs).
  - **Pink hypothesis**: our softpipe/GLES2 stack renders the 1x1 single-pixel-buffer scaled via
    viewporter wrong (magenta = classic missing/format-mismatch texture). Black(0,0,0,alpha)
    coming out pink ⇒ NOT a plain R/B swap (RGB all 0) ⇒ uninitialized/garbage sampling of the
    single-pixel buffer. **Persistent** pink (vs 5 s transient) ⇒ frame callbacks stall so
    `fade_done` never runs, leaving a stuck partial-alpha frame + hidden cursor.
- **cosmic-greeter** (`ports/cosmic-greeter/0001-...patch`): startup `already_locked =
  lockfile.exists()` where lockfile = `$XDG_RUNTIME_DIR/cosmic-greeter-<sid>.lock`.
  XDG_RUNTIME_DIR = /run/user/0 = **tmpfs, reset each boot** → no lock file on fresh boot →
  `already_locked=false` → `Task::none()` (idle, does NOT lock). ⇒ **H2 weak** on a fresh boot.

## Leading hypothesis: **H1 (cosmic-idle fade)**.

## Repro log

### Run 1 (12:15 image, forced screen_off_time=4000ms) — NON-REPRO, pivotal finding
- Desktop rendered GOOD throughout t=55..130s: center=(190,142,130) Orion nebula,
  top_panel=(51,214,200) teal leandros-applet block, wallpaper_lower=(167,120,138).
  **No pink at any sampled time.**
- Serial: `cosmic-idle: failed to start process: No such file or directory (os error 2)`
  — and identically for cosmic-greeter, cosmic-workspaces, cosmic-files-applet.
- Config write verified applied (`10 /root/.config/.../screen_off_time`, content `Some(4000)`).
- **Direct `ls` on the 12:15 image**: `/bin/cosmic-idle`, `/bin/cosmic-greeter`,
  `/usr/lib/libpam.so.0` all **DO NOT EXIST**. cosmic-bg/cosmic-panel present & fine.
- ⇒ The on-disk 12:15 f2fs-data0-aarch64.img **never staged idle/greeter/libpam**.
  It is effectively the "idle+greeter UNSTAGED" control, and it renders good (no pink).
  The mkfs idle/greeter/libpam staging is **uncommitted** working-tree code (last commit
  to mkfs = 8a76fa2); libpam.so.0 is only staged if the shim was pre-built into the GL
  sysroot (it was not). The 12:15 build predates/omitted the idle+greeter staging.

### Consequence for hypotheses
- With idle/greeter absent, neither can be the pink cause in the 12:15 image as it sits.
- To reproduce the actual regression the report describes, must rebuild an image that
  DOES run cosmic-idle, then observe.

### Run 2 (REBUILT full image: idle+greeter+workspaces+libpam staged, forced 4s idle)
- Rebuilt f2fs via mkfs (stages all four now that binaries+shim exist). Confirmed via ls.
- Desktop STILL rendered good t=55..130s (nebula + teal applet). **No pink/black.**
- cosmic-idle: runs STABLY (1 start, no restart, bound all globals ⇒ our comp advertises
  ext_idle_notifier/zwlr_output_power/single_pixel/layer_shell/viewporter; no config error
  ⇒ Some(4000) honored). Entered event loop. But **no fade ever** over 60+s at 4s timeout.
- cosmic-greeter: **crash-loops** — panics with EMFILE ("No file descriptors available",
  os err 24) at the first calloop Ping / winit event-loop creation, restart 7+. Dies
  before rendering any surface ⇒ cannot draw pink.
- cosmic-workspaces: runs idle (i18n/zbus up, asks for CosmicOnScreenDisplay) — no overlay.

### Run 3 (REBUILT image, 6s idle, REAL input arming at t=62: sendkey + tablet abs, qmp_ok)
- Desktop **pixel-identical good** t=55..125s. No fade, no pink, no black.
- ⇒ Definitive: our cosmic-comp's ext_idle_notifier **never delivers `Idled`** (smithay
  arms the timer at notification creation and fires after `timeout` regardless of activity
  — yet never fires here; likely set_is_inhibited(true) from refresh_idle_inhibit, or the
  calloop idle timer isn't serviced). The single-pixel fade therefore never triggers.

## CONCLUSION
- **H1 (cosmic-idle fade → pink): FALSE.** Fade never fires (4s & 6s, 60+s each, ±input);
  and even if it did, its buffer is black `(0,0,0,alpha)` via draw_solid (smithay
  surface.rs:302,471) → black, not pink.
- **H2 (cosmic-greeter lock → pink): FALSE.** Starts idle (no lock file) AND crash-loops
  on EMFILE before drawing.
- **H3: FALSE.** cosmic-workspaces idle, no overlay.
- **The 12:15 on-disk image doesn't contain idle/greeter** — functionally M7y-good, renders
  good. Task premise factually incorrect.
- **PINK NOT REPRODUCED** in: 12:15 image, full-rebuild@4s, full-rebuild@6s+input.

## Real regressions found (independent of pink)
1. 12:15 image never staged idle/greeter/libpam (staging uncommitted; 12:15 build ran an
   earlier script state). Fixed by rebuilding → .full-rebuild.
2. cosmic-greeter crash-loops on EMFILE — fd table near-full at greeter start ⇒ likely
   non-CLOEXEC fds inherited from launch_pad (greeter spawns late) + low RLIMIT_NOFILE.
3. cosmic-idle idle-notify inert (Idled never delivered) — screen-off/suspend don't work.

## Image state left
- f2fs-data0-aarch64.img            = restored ORIGINAL 12:15 (idle/greeter ABSENT)
- f2fs-data0-aarch64.img.12h15-orig = backup of same
- f2fs-data0-aarch64.img.full-rebuild = clean image WITH idle+greeter+workspaces+libpam
- No binaries renamed; m6-session-bins/out/ untouched.
- Screenshots: ~/code/leandros-artifacts/notes/m7z-screenshots/
