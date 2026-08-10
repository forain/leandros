# Items 14 and 10 — the shim's third tier, and a divergence that runs both ways

Implemented 2026-08-10 on the Linux box, following the two measurement notes
`item14-libseat-vt-measurement.md` and `item10-busd-activation-not-shipped.md`.
Those notes established the facts; this one records the decisions and what the
decisions turned up.

## Item 14 — why the fix is neither of the two candidates

The measurement offered two ways forward: export `XDG_VTNR` in the session
launch path, or give the session a real VT controlling terminal so
`ttyname_r()` works. **Both describe an ownership that does not exist**, and the
tree already contains a third answer that does.

### The console does not live on a VT

`servers/tty/src/vt.rs`'s `console_out` writes into `SCREENS[ACTIVE]` — the
kernel console mirror follows whichever VT is in the foreground. There is one
getty, one `login`, one shell, and it is on `/dev/console`. Switch to VT 2 and
the same shell keeps echoing there; `959710d` measured exactly that (`uname -a`
/ `id` / `ls /etc` on VT 2 for 15.8 s while a DRM client held VT 1). No process
in the chain ever opens `/dev/ttyN`.

So the login session genuinely owns no VT. `XDG_VTNR=1` would assert that it
owns VT 1; a `/dev/tty1` controlling terminal would assert the same thing one
layer down. Start COSMIC after a `Ctrl+Alt+F2` and both assertions are false —
and false in the worst possible way, because the DRM master gate would disagree.

### The DRM layer already answers this question, and answers it by derivation

`959710d` ("drm: master follows the VT"): a grant records `vt::active()` at the
instant it is made, and `master_ok` compares that against the live value. The
DRM layer never reads `XDG_VTNR` and never asks for a controlling terminal. It
asks "which VT was on screen when you claimed the display", and that is the
whole of its rule.

A libseat shim that resolved ownership from a *different* source than the master
gate would be free to disagree with it, and a disagreement here is not a cosmetic
bug: the compositor would believe it is active on a VT where every present is
refused with EACCES, which is precisely the 20 s black screen the measurement
photographed.

### What was implemented

A third tier in `owned_vt()`, below the two that already existed:

    XDG_VTNR  ->  /dev/ttyN controlling terminal  ->  foreground VT at open_seat()

`vt_probe()` now opens `/dev/tty0` and issues `VT_GETSTATE` *before* resolving
ownership, and hands the resulting `struct vt_stat` to `owned_vt()`. When the
first two tiers decline, `v_active` is adopted and latched. A compositor calls
`open_seat()` during start-up and claims the display moments later on the same
VT, so this is the same derivation the master gate uses, one instant earlier —
the two agree by construction rather than by configuration.

This is also what seatd does when nothing tells it: its `terminal.c` reads the
current VT out of `/dev/tty0` and binds the seat to it. We do it in-process only
because there is no daemon.

The precedence order is deliberate and is decreasing *authority*, not
convenience. `XDG_VTNR` is somebody telling us; a `/dev/ttyN` ctty is the kernel
telling us; the foreground VT is us inferring it. Both upper tiers stay in
place, so a future greetd or logind that really does place the session on a VT
wins outright without this file changing — and so does a real per-VT getty, the
day one exists.

### Recorded so it is not re-derived

`owned_vt_from_ctty()` now carries the measured reason it declines:
`ttyname_r` returns ENOTTY (25), not `/dev/console`, because musl calls
`isatty()` first and that fails on the fd `open("/dev/tty")` yields for a
session whose ctty is the kernel console. `init` runs `TIOCSCTTY` on the console
fast path, which records no per-session ctty. **Correcting only the name would
have fixed nothing.**

### Measured, both arches, production configuration (no `XDG_VTNR` anywhere)

Trace shape identical on aarch64/TCG and x86_64/KVM:

    owned_vt_from_ctty: ttyname_r failed rc=25       <- tier 2 declines, as measured before
    owned_vt: 1 (foreground VT at open_seat)         <- tier 3 resolves it
    vt_probe -> vt_fd=23 own_vtnr=1 v_active=1 active=1
    get_fd  -> 23 (vt_fd=23 conn_fd=22)              <- the live fd, not the inert eventfd
    open_device event0 -> 27 ; event1 -> 28 ; card0 -> 29
    -- Ctrl+Alt+F2 --
    dispatch -> disable_seat (VT 1 lost foreground to 2)
    close_device id=27 rc=0 ; close_device id=28 rc=0
    -- Ctrl+Alt+F1 --
    dispatch -> enable_seat (VT 1 became foreground)
    open_device event0 -> 28 ; event1 -> 35

**Exactly one `disable_seat` and exactly one `enable_seat`, in that order, each on the
correct edge**, and cosmic-comp acted on both — it closed the two evdev fds on deactivate
and reopened them on activate. `card0` (id=29) is not closed, so the master auto-rearm
from `959710d` covers the return. The desktop came back intact on both arches: aarch64
clock 00:03:40, x86_64 clock 00:14:30, panel + wallpaper + dock, no re-initialisation.

This reproduces the `XDG_VTNR=1` control run from the previous measurement **without the
env var**, and extends it to aarch64, which that run never covered.

### The differential test is the one that matters

Same image, same everything, except `Ctrl+Alt+F2` was injected **before** COSMIC was
started, so the compositor came up while VT 2 was in the foreground:

    owned_vt: 2 (foreground VT at open_seat)
    vt_probe -> vt_fd=23 own_vtnr=2 v_active=2 active=1

A hardcoded `XDG_VTNR=1` would have produced `own_vtnr=1 v_active=2 active=0` here: the
compositor would have started **deactivated**, on the VT it was actually running on, and
never presented. That is the concrete harm the derivation avoids, and it is why "export
`XDG_VTNR`" was not the fix even though it unblocks the shim.

Incidental corroboration of the premise: after the pre-start `Ctrl+Alt+F2` the login
shell's prompt redrew at row 5 of an empty VT 2 rather than staying at row 48 of VT 1 —
the console followed the switch, exactly as `console_out`'s `SCREENS[ACTIVE]` says it
must. Later, with the compositor backgrounded on VT 1, `id` ran and printed
`uid=0(root) gid=0(root) groups=0(root)` on VT 2.

### When option B becomes right

Giving the session a real VT controlling terminal is the correct fix *after*
per-VT gettys exist — six sessions, input routed per VT, the console bound per
VT rather than following the foreground. Today it would be inventing an
ownership the rest of the system does not honour. The tier is already there
waiting for it.

## Item 10 — the artifacts tree had drifted in BOTH directions

The item-10 note found `busd` and `session.conf` staged from before `84ec91a`.
Fixing that by declaring `ports/` the source of truth and copying it over the
staged tree would have **shipped a desktop that does not start**.

`ports/dbus/session-pkg/dbus-run-session` is tracked, was committed once in
`b8e8be5`, and is the *superseded* version. It reads `BUSD_PID=$!` on the line
after `busd ... &`; under brush that is the empty string, `kill -0 ""` fails on
the first poll iteration, and the launcher exits 1 with "busd exited before
signaling readiness" — no session at all. The working launcher, which records
busd's real pid through an `sh -c` wrapper before `exec`, existed **only** in
the untracked artifacts tree.

So the three staged files were stale in opposite directions at the same time:
two behind the repo, one ahead of it. Diffing before choosing a direction is not
optional here.

### What was implemented

`ports/busd/build.sh` now owns the whole staged D-Bus payload and writes every
file of it from tracked sources on every run: `busd` built, and
`dbus-run-session`, `session.conf` and `services/*.service` copied out of
`ports/dbus/session-pkg/`. The staged tree is a build output for this subtree,
not somewhere anyone edits.

Two further changes make the failure impossible to repeat quietly:

* **The actual trap is gone.** The old script guarded extraction *and patching*
  with `if [ ! -d "$SRC" ]`. Adding `start-service-activation.patch` therefore
  left the already-extracted tree untouched, and the next build recompiled the
  old sources into a byte-identical binary. It now re-extracts and re-patches
  every run against the cached `.crate`. Confirmed: the pre-existing `.work`
  tree had **zero** hits for `StartServiceByName`.
* **`scripts/build-all.sh` calls it** (`stage_dbus_session`, beside
  `build_input_stack_shims`, which exists for exactly this class of drift), and
  **`mkfs-f2fs-populated.py` refuses to build a stale image**
  (`verify_dbus_staging`): the two verbatim files are compared byte-for-byte
  against `ports/`, and `busd` — which mkfs cannot rebuild — is compared by
  mtime against the patch set that defines it. Warn in the build, refuse in the
  image; never silent in either.

The guard was tested rather than asserted: drifting the staged `session.conf`
and back-dating the staged `busd` each raise `SystemExit(1)` with the fix
command, and a clean tree passes. The busd message it prints is a literal
description of today's bug — "`ports/busd/start-service-activation.patch` is
newer than the staged binary, so the staged busd was built without it."

### The activation self-test ships

`ports/dbus/session-pkg/services/org.leandros.ActivationProbe.service` is in the
image on both arches. busd scans its servicedirs **once at startup**, so a
`.service` file dropped into a running session is never seen — being in the
image before busd starts is the only way activation is testable at all.

The name is deliberately one nothing else asks for. Attaching the test to a name
the session really wants (the census in `service-unknown-reply.patch` lists six:
`com.system76.PowerDaemon`, `org.a11y.Bus`, `org.freedesktop.UPower`,
`org.freedesktop.locale1`, `org.freedesktop.timedate1`,
`com.system76.CosmicSettingsDaemon`) would make every boot pay busd's 5 s
`ACTIVATION_TIMEOUT` and would entangle a desktop regression with an activation
regression. `Exec=` names an ELF, not a script, because the kernel has no
`#!` binfmt and busd `execve`s the argv directly.

### One more gap the test surfaced: activated services were told nothing

busd spawns an activated `.service` with a plain `std::process::Command` and injects
nothing beyond `UpdateActivationEnvironment` entries — no `DBUS_STARTER_ADDRESS`, no
`DBUS_SESSION_BUS_ADDRESS`. The child therefore inherits **busd's** environment, and
`dbus-run-session` exported the address only for COMMAND, *after* busd was already
launched. Measured against real busd on the host: an activated child that guessed the
wrong bus never claimed its name and `StartServiceByName` burned the full 5 s timeout.
It happened to work for a root session, where the compiled-in fallback
`/run/user/0/bus` is the right answer, and would have failed for every other uid.
Fixed by setting both variables on busd itself, in the env-prefix at the launch site.

## What the boot showed

`ports/dbus/session-pkg/services/` and the rebuilt busd are in both images.

1. **Session comes up cleanly, both arches.** aarch64 and x86_64 each reached a full
   COSMIC desktop — panel, wallpaper, dock, ticking clock. No busd regression.
2. **Servicedir scan — proven by behaviour, not by a log line.** `load_servicedirs`
   emits **nothing on success** at any level; the only `warn!` is for an invalid `Name`
   and the only `debug!` is for a duplicate. So "busd logs scanning the servicedirs" is
   not observable in this build, and `RUST_LOG` (already exported as `info` by
   `start-cosmic-leandros`, so `warn!`/`info!` were visible throughout) does not change
   that. Worth adding an `info!` there. The scan is proven instead by (3).
3. **Activation spawns, and completes — both arches.** aarch64, in-guest at HEAD:
   `STARTSERVICE: result=1 82ms`, `UNOWNED: …ServiceUnknown 4ms`,
   `IMPLICIT: success 20ms`, desktop up (clock 00:04:54). x86_64, `dbusprobe`:
   `STARTSERVICE: result=1 55ms` on the first call — `DBUS_START_REPLY_SUCCESS`, i.e.
   busd found the `.service` file, spawned `/bin/dbusprobe --serve`, and saw it claim
   `org.leandros.ActivationProbe`, all inside 55 ms. A second run minutes later:
   `STARTSERVICE: result=2 2ms` — `DBUS_START_REPLY_ALREADY_RUNNING`, the activated
   service still alive and still owning the name. `IMPLICIT: success` on both runs, so
   the `send_msg` hook routes to it too. None of that is reachable unless the servicedir
   was scanned and the file parsed.
4. **The ServiceUnknown fast path is intact.** `UNOWNED:
   org.freedesktop.DBus.Error.ServiceUnknown` in **3 ms** and **2 ms** on the two x86_64
   runs and **4 ms** on aarch64/TCG.
   And live during aarch64 session start-up, the new busd answered 20 unowned names
   promptly — `org.a11y.Bus`, `com.system76.CosmicAppLibrary`, `CosmicLauncher`,
   `CosmicWorkspaces`, `CosmicOnScreenDisplay`, `CosmicSettingsDaemon` ×4,
   `org.freedesktop.UPower` ×2, `locale1`, `portal.Desktop` ×6, `login1` — clustered
   across nine seconds with nothing blocking, and every applet went on to draw. The hang
   `service-unknown-reply.patch` fixed has not come back.

### An artefact to distrust, recorded so nobody reads it as evidence

`dbusprobe`'s `ACTIVATABLE:` list is **unreliable and should not be cited**.
`list_activatable_names` returns the union of `.service`-provided and currently-owned
names, so `org.leandros.ActivationProbe` must be in it — and it never appeared, on either
run, while the count varied between runs (15, then 17). That signature is a partial read
in the probe's `wait_for_reply`, not a busd defect: the `StartServiceByName` results
above are unambiguous and contradict any reading in which the name was absent from the
map. The probe's reply reassembly needs fixing before that line means anything.
