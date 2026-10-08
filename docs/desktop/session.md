# The COSMIC session

After a login, `start-cosmic-leandros` brings up a session bus, PipeWire and
`cosmic-session`, which in turn supervises the compositor, the panel and the rest of
COSMIC. This page covers that chain: the launcher, the supervisor's choreography, the
D-Bus broker and the system services LeandrOS provides in place of systemd's, the panel
applets, the launcher backend, the desktop portal, and Shut Down / Restart.

## Overview

| Piece | Where it comes from | Role |
|---|---|---|
| `start-cosmic-leandros` | [artifacts/m6-session-data/start-cosmic-leandros](../../artifacts/m6-session-data/start-cosmic-leandros) | replaces upstream's bash/systemd `start-cosmic`; sets the environment, starts PipeWire, then `dbus-run-session -- cosmic-session cosmic-comp --no-xwayland` |
| `dbus-run-session` + `busd` | [ports/dbus/session-pkg](../../ports/dbus/session-pkg/), [ports/busd](../../ports/busd/) | the session bus, with `.service` activation |
| `cosmic-session` | upstream `epoch-1.3.0`, unmodified ([ports/cosmic-session](../../ports/cosmic-session/README.md)) | launch-pad supervisor of the session |
| `leandros-sysbus` | [ports/sysbus](../../ports/sysbus/) | login1, locale1, UPower, power-profiles, PolicyKit1, RealtimeKit1 |
| xdg-desktop-portal | [ports/portal](../../ports/portal/) | `org.freedesktop.portal.Desktop` with the COSMIC backend |
| PipeWire | [ports/pipewire](../../ports/pipewire/) | audio graph, the virtio-snd sink, pipewire-pulse, ScreenCast streams |
| `pop-launcher` | [ports/pop-launcher](../../ports/pop-launcher/README.md) | the search backend behind `cosmic-launcher` |

## How it works

### The launcher

[`start-cosmic-leandros`](../../artifacts/m6-session-data/start-cosmic-leandros) is a POSIX
`sh` script that is brush-safe: no bashisms, no `sleep`, no FIFOs, no login-shell
recursion. Upstream's `start-cosmic` is bash and assumes systemd, `systemctl`, a
login-shell re-exec and gnome-keyring, none of which exist here. In order, it:

1. sets `XDG_RUNTIME_DIR` (`/run/user/<uid>`), `HOME` and the four `XDG_*_HOME`
   directories — cosmic-config `create_dir_all`s under `$XDG_CONFIG_HOME/cosmic` and fails
   with `NoConfigDirectory` without them, and cosmic-bg's per-output state needs
   `XDG_STATE_HOME`;
2. on first login, does what `xdg-user-dirs-update` does: creates `Desktop`, `Documents`,
   `Downloads`, … and writes `user-dirs.dirs` (an existing file is the user's and is left
   alone) — cosmic-files-applet and the file choosers look these up;
3. sets `PATH` to cover `/bin` (cosmic-session spawns children by bare name through Rust
   std's `PATH` lookup, and the session binaries live in `/bin`) and addresses everything
   brush itself executes by absolute path, because brush's own exec search does not fall
   through `/usr/bin` → `/bin`;
4. aliases the system bus to the session bus (below);
5. forces the KMS backend (`COSMIC_BACKEND=kms`, `COSMIC_RENDER_DEVICE=226:0`, `DISPLAY`
   and `WAYLAND_DISPLAY` unset so cosmic-comp's auto-detect cannot pick a nested backend),
   sources `/bin/gpu-env`, and refuses to start (exit 78) if there is no GPU renderer;
6. sets `COSMIC_DISABLE_OVERLAY_SCANOUT=1` — *not* `COSMIC_DISABLE_DIRECT_SCANOUT`, which in
   smithay clears a flag union that includes the cursor plane — and leaves
   `SMITHAY_USE_LEGACY` unset, because the atomic path is the only one that can program a
   cursor plane;
7. starts PipeWire once per runtime directory (it starts WirePlumber, the
   `leandros-snd-sink` virtio-snd sink and `pipewire-pulse` itself), unless
   `LEANDROS_NO_PIPEWIRE=1`;
8. execs `dbus-run-session -- /bin/cosmic-session /bin/cosmic-comp --no-xwayland` (or
   `cosmic-session` directly if a bus address is already set).

Under greetd the same environment arrives through `/etc/profile`, and the `gpu-env` step is
a no-op the second time (`LEANDROS_RENDERER_DECIDED`). From a root shell the session can
still be started by hand with `sh /bin/start-cosmic-leandros`; with the greeter running, a
second compositor started that way never gets DRM master on VT 1 and cannot present.

### cosmic-session's choreography

`cosmic-session` is a launch-pad `ProcessManager`, not systemd, and with `/run/systemd/system`
absent its systemd calls reduce to logged warnings
([m6-session-choreography](../../artifacts/notes/m6-session-choreography.md)).

- **One readiness gate.** cosmic-comp is started first with `COSMIC_SESSION_SOCK`, one end
  of a `socketpair`. cosmic-session then blocks on `env_rx.await` until the compositor
  sends `SetEnv{WAYLAND_DISPLAY}` over it. Nothing else in the session sleeps or polls.
- **Four children are fatal at spawn**: cosmic-comp, cosmic-settings-daemon,
  cosmic-notifications and cosmic-panel. A missing binary panics the whole session;
  everything else (cosmic-bg, -osd, -launcher, -app-library, -workspaces, -files-applet,
  -idle) is tolerated.
- **notifications and panel are a mutually restarting pair** sharing a socketpair
  (`DAEMON_NOTIFICATIONS_FD`/`PANEL_NOTIFICATIONS_FD`).
- **Children inherit nothing they were not given.** launch-pad pipes each child's stderr and
  registers no handler for it, so a child's diagnostics reach the log only through
  cosmic-session's relay — when cosmic-session dies, the session log goes quiet even though
  the desktop keeps running.

The kernel surfaces this choreography depends on are all ours and all guarded by
`userland/scmtest`: `F_SETFD` on socket fds (launch-pad clears `FD_CLOEXEC` on inherited
`SOCK_CLOEXEC` sockets before `execve`), each `memfd_create` being a distinct inode even with
the same name (smithay-client-toolkit names every `wl_shm` pool
`"smithay-client-toolkit"` and seals it), enough pipes for ~14 components each holding three
stdio pipes, and `shutdown(SHUT_WR)` being a half-close.

### busd

[busd](https://github.com/dbus2/busd) (crates.io 0.5.0) is the session broker: a pure-Rust
D-Bus broker from the zbus authors, built static-musl. The reference `dbus-daemon` was also
ported ([ports/dbus/RUNTIME-NOTES.md](../../ports/dbus/RUNTIME-NOTES.md)) and kept as a
reference; busd is what runs. `dbus-run-session` is a POSIX-sh script that starts busd on the
fixed address `$XDG_RUNTIME_DIR/bus`, waits for it by busy-polling a regular file, and exports
`DBUS_SESSION_BUS_ADDRESS` to the session.

busd is not COSMIC, so it is patched freely; `build.sh` applies the patches in name order:

| Patch | What it does |
|---|---|
| `current-thread-runtime.patch` | a single-threaded tokio runtime; kept from an investigation, neither necessary nor sufficient for anything now |
| `service-unknown-reply.patch` | replies `org.freedesktop.DBus.Error.ServiceUnknown` to a method call addressed to a well-known name nobody owns, instead of dropping it |
| `start-service-activation.patch` | real `.service`-file activation: `StartServiceByName`, `ListActivatableNames`, `UpdateActivationEnvironment`, and implicit activation of an unowned but activatable name |
| `unique-activation.patch` | a service is spawned once however many callers ask at the same moment; the activation timeout is dbus-daemon's 25 s instead of 5 s |

`session.conf` contains only what busd parses: `<type>`, `<auth>EXTERNAL</auth>`, a
default `<listen>`, and `<servicedir>/usr/share/dbus-1/services</servicedir>`. No
`<policy>` is shipped. `.service` files are scanned **once, at busd startup** (images do not
grow `.service` files at run time), so a file dropped into a running session is never seen.
`User=` is parsed but not honoured: busd has no setuid helper, so every activated service
runs as the session user.

Two facts that cost time: busd's `tracing` output goes to **stdout**, so redirecting only
stderr captures none of it; and after editing a patch, `ports/busd/.work` must be removed,
because `build.sh` applies patches only when it first extracts the crate.

### The system bus is the session bus

LeandrOS runs no system broker. `start-cosmic-leandros` sets
`DBUS_SYSTEM_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus`, so zbus `Connection::system()`
reaches the session busd, and the "system" services below are activated there, as the
session user. Without the alias, `Connection::system()` fails at `connect()` and
cosmic-settings-daemon's theme watcher retries in a loop.

The **greeter** is the opposite case: `/etc/profile` and `greeter-env` *unset*
`DBUS_SYSTEM_BUS_ADDRESS`. cosmic-greeter calls `user_data_dbus()` against
`com.system76.CosmicGreeter` before it builds any window; that name belongs to
cosmic-greeter-daemon, which is not shipped. With the variable unset, `Connection::system()`
fails fast and the greeter falls back to reading `/etc/passwd`.

### System services: `leandros-sysbus`

[ports/sysbus](../../ports/sysbus/README.md) is one static-musl zbus binary; `argv[1]` picks
the service, and busd activates each from a `.service` file in
[ports/dbus/session-pkg/services](../../ports/dbus/session-pkg/services/).

| Name | What works | What refuses |
|---|---|---|
| `org.freedesktop.login1` | sessions, seat and user objects; idle and locked hints; inhibitors (`Inhibit` returns a pipe fd, released when the client closes it); `PowerOff`/`Reboot`/`Halt` forwarded to init; `Can*` say `"yes"` while init listens | `Suspend`, `Hibernate` and the other sleep states: `NotSupported` (`Can*` → `"na"`) |
| `org.freedesktop.locale1` | read side; `SetX11Keyboard` takes effect on the bus and is persisted best-effort (it only mirrors cosmic-comp's own xkb config) | `SetLocale` → `AccessDenied` when `/etc/locale.conf` is not writable by the session user |
| `org.freedesktop.UPower` | a machine with no battery, as upower reports on a desktop | — |
| power-profiles | claims both `org.freedesktop.UPower.PowerProfiles` and `net.hadess.PowerProfiles`; Settings' Power page shows three modes | a placeholder driver |
| `org.freedesktop.PolicyKit1` | authentication-agent registry, so cosmic-osd's agent registers | only root is authorised |
| `org.freedesktop.RealtimeKit1` | answers, so PipeWire does not warn three times per start | zero limits; `NotSupported` for every request |

Anything that needs root fails with an error and never pretends to succeed.
`leandros-sysbus probe` exercises the services the way COSMIC components do and prints
PASS/FAIL lines (it never calls `PowerOff`/`Reboot`), which makes it the in-guest check;
`leandros-sysbus portal …` probes the portal. mkfs refuses to build an image whose staged
sysbus binary is missing or older than its sources, or whose `.service` `Exec=` names a
binary that is not there.

cosmic-settings' Region & language page also needs Debian's iso-codes JSON
([ports/iso-codes](../../ports/iso-codes/build.sh), three tables, sha256-pinned): locales-rs
fails with "No iso-codes json data found" before it even asks locale1.

### Shut Down and Restart

COSMIC's power dialog calls `login1.PowerOff`/`Reboot`, and the path from there is the
Linux one: login1 emits `PrepareForShutdown(true)` and forwards the request over
`/run/user/initctl` to init, which authorises it from the socket's `SO_PEERCRED` — uid 0, or
a peer whose ancestry or session reaches a login init supervises (logind's default
`allow_active` policy) — then sends `SIGTERM`+`SIGHUP`+`SIGCONT` to every process, waits up
to 5 s, `SIGKILL`s what is left, syncs, unmounts the other f2fs volumes, remounts `/`
read-only and calls `reboot(2)`. The kernel ends with ACPI S5 / `RESET_REG` on x86_64 and
PSCI `SYSTEM_OFF` / `SYSTEM_RESET` on aarch64, after writing a clean-unmount checkpoint on
every writable f2fs volume ([kernel/src/power.rs](../../kernel/src/power.rs)).

Measured on both arches: all processes gone in 54–58 ms after the click, QEMU exiting by
itself 0.5 s later, and the next boot logging `checkpoint ver N is a clean unmount` with
markers written just before the shutdown (unsynced) present. The initctl socket lives under
`/run/user` because sockets can only be bound on tmpfs here, and `/run/user` is root-only, so
the name cannot be squatted. `/bin/poweroff`, `/bin/reboot` and `/bin/halt` use the same
socket ([../userland/programs.md](../userland/programs.md)).

### The panel and its applets

Five real applets ship — `CosmicPanel{Launcher,App,Workspaces}Button`, `CosmicAppletTiling`
and `CosmicAppletMinimize` — plus the stand-in clock, and, when the PipeWire port is staged,
`cosmic-applet-audio` (the panel's Sound icon). Eight applet processes run for the first
six, because `entries` is `["Panel", "Dock"]`: the two spaces resolve their plugin lists
independently, and two applets appear in both. **A name is not a process**, and reasoning
about "how many applets" as a count is what made an earlier diagnosis wrong (below).

How the panel finds an applet shapes the staging
([scripts/mkfs-f2fs-populated.py](../../scripts/mkfs-f2fs-populated.py)):

- cosmic-panel resolves each configured name to a `.desktop` file under
  `/usr/share/applications` and spawns its `Exec`. A name with no desktop file resolves to
  nothing and is **silently never spawned**, so binary and desktop file are staged together.
- `cosmic-panel-button` backs three names from one binary, taking the target's app id as
  `argv[1]`. It resolves that id to a *file* and panics if `<id>.desktop` is missing, so the
  launch targets' desktop files are staged even though no panel config names them.
- **A panel button wears its target's icon, not its own.** A missing icon is silent and
  shaped like success: libcosmic falls back to an empty SVG, the widget keeps its size, the
  dock lays out around it, and zero pixels are painted — identical to "the applet never
  started". The staging therefore cross-checks every `Icon=` name against the staged themes
  and warns on a miss.
- The clock is `leandros-applet`, a ~230-line dependency-free `wl_shm` client staged under
  `com.system76.CosmicAppletTime.desktop`. It exists because cosmic-panel renders nothing
  while it has no content (`actual_size <= 20` early-returns), which is what kept the first
  desktop's bar invisible; the real time applet was not needed to prove the panel.
- The panel applets stay on iced's tiny-skia renderer: they are clients of cosmic-panel's
  embedded Wayland server, which only offers `zwp_linux_dmabuf_v1` if its GLES renderer
  already exists when the display is bound at startup.

`cosmic-applet-minimize` is a 1×1 hidden surface with zero toplevels when no windows are
open; that is upstream design, not a bug.

### The launcher backend

`cosmic-launcher` talks to [pop-launcher](../../ports/pop-launcher/README.md) over piped JSON
on stdin/stdout. pop-launcher (`a332a3a`, v1.2.7, the exact revision `cosmic-launcher` pins
— the wire protocol is a serde enum with no version field) is built unmodified. One multicall
binary serves the service and every plugin by `argv[0]`; the plugins are **hardlinks**
rather than upstream's symlinks, so `execve` needs no symlink resolution. Two plugins are
staged: `desktop_entries` (the launcher's core function) and `cosmic_toplevel` (window
switching over cosmic-comp's toplevel-info protocol). The others are left out because
their runtimes are absent (`qalc`, PulseAudio, `xdg-open`, `fd`, …).

Before it was staged, the launcher window opened and every query returned an empty list.
`desktop_entries` also asks the system bus for `net.hadess.SwitcherooControl` before it
serves its first search; busd's `ServiceUnknown` reply is what makes that return at once
instead of hanging the plugin.

### The desktop portal

[ports/portal](../../ports/portal/) ships `org.freedesktop.portal.Desktop`:

- **Frontend:** Alpine 3.21's prebuilt `xdg-desktop-portal 1.18.4` and
  `xdg-permission-store`, built in a container of the target architecture and ELF-fixed like
  the Firefox port (libc soname rewrite, the `__stack_chk_guard` shim). Its GLib closure is
  about 9 MB. Repackaging was chosen over cross-building a GLib C stack.
- **Backend:** `xdg-desktop-portal-cosmic`, cross-built **unmodified** from the pinned
  cosmic-epoch with the same toolchain as the rest of COSMIC (GLib/GIO taken from Alpine's
  `glib-dev`, because the portal hardcodes cosmic-files' `gvfs` feature).
- **Interfaces:** Access, FileChooser, Screenshot and Settings. ScreenCast is advertised
  only when the real PipeWire port is staged (its `cosmic.portal` overrides the portal
  port's ScreenCast-less copy); with it, a ScreenCast stream delivered 46–47 fps at full
  resolution on both arches.
- **Not shipped:** `xdg-document-portal`, which needs FUSE; host applications get plain
  `file://` URIs.

Bring-up found three bugs, all fixed outside COSMIC: the VFS had no `/proc/<pid>/root`
(xdg-desktop-portal opens it on every call to look for `.flatpak-info`, so every call was
refused); busd spawned an activated service once per concurrent caller (two frontends and
two 60 MB backends during the login storm); and the 5 s activation timeout was shorter than
the frontend's own start under load.

## Design decisions

### busd answers `ServiceUnknown`

A D-Bus call to a name nobody owns used to be dropped with a `warn!` and no reply, and
neither side has a timeout — so every libcosmic application blocked forever in a D-Bus probe
before it ever drew. This was recorded for a long time as "libcosmic/iced cannot render".
The toolkit was fine: `cosmic-settings` wrote 1775 bytes of log and then nothing for 75 s,
with `WAYLAND_DEBUG=1` producing not one line, because it had never opened a Wayland
connection ([m15](../../artifacts/notes/m15-libcosmic-single-instance/README.md)). A census
of one boot found **ten** distinct unowned names addressed in the first ten seconds, only
four of them single-instance probes
([m17](../../artifacts/notes/m17-servicename-census-20260808/README.md)).

The fix — reply `ServiceUnknown` — then spent a wave unapplied, and the reason was never in
busd. Unblocking four autostarted components took the kernel's 64-bucket IPC port table over
its ceiling; the resulting `ENOMEM` from a `stat` of `/usr/share/X11/xkb` made
`xkb_context_new` return `NULL`, which SCTK dereferenced at `0x880`. It was first recorded as
a failed `calloc`. Holding the busd binary constant and moving only the kernel's
`LIVE_BUCKETS` is the control that separates the two:

| kernel | panel bar + clock | `CR2=0x880` deaths |
|---|---|---|
| `LIVE_BUCKETS = 64` | gone | 1, then the session wedged |
| `LIVE_BUCKETS = 512` | present, clock ticking, all 9 screenshots | 0 |

That is the origin of the project rule that an errno must name its own cause
([../overview/design-principles.md](../overview/design-principles.md)), and of
`ipc/src/port.rs`'s 512 buckets ([../kernel/ipc.md](../kernel/ipc.md)).

### Activation, not just answers

`ServiceUnknown` made absent services fail fast; activation (`84ec91a`, shipped and booted
2026-08-10) made the present ones start on demand. Activation was implemented in busd rather
than by starting every service eagerly from the launcher because COSMIC addresses these
names lazily and the portal backend alone is a 60 MB iced runtime: a session that never opens
a file chooser should not pay for one. The "spawn once" and 25 s rules came from measuring the
login storm, where several applications ask for the portal in the same millisecond.

### Services as the session user, refusing honestly

With no system bus and no setuid helper, login1 and friends run as the session user. The
alternative — a privileged system broker — was not built; instead each service does what an
unprivileged process can, and refuses the rest with the D-Bus error a real service would
give. The one operation that genuinely needs root, powering off, is delegated to init, which
already is root and already knows which processes belong to a local login.

### The applet "ceiling" that was one listening socket

The recorded hypothesis was "more than ~4 concurrent applets kills the panel", with
descriptor exhaustion as the suspect and a count bisect as the next step. The working and
failing sets in fact differed qualitatively: `CosmicAppletTiling` and `CosmicAppletMinimize`
carry `X-HostWaylandDisplay=true`, which makes cosmic-panel hand them a privileged socket via
`wp_security_context_v1.create_listener`. That request marshals a *listening* `AF_UNIX`
socket, and libwayland dups every fd it marshals. `dup()` of a listening socket returned
`EINVAL`, and SCM_RIGHTS could not carry socket fds at all; a marshalling failure poisons
libwayland's whole connection, so the panel exited and the bar vanished. Both are fixed in
`servers/net`; the tiling applet then also needed `fork_dup` to keep `UnixPendingAccept` fds
and more f2fs open files (`0f56aab`, `cdd613e`), and the clock started ticking once a nested
epoll fd stopped reading as `POLLNVAL` (`4085b7f`)
([item 9b](../../artifacts/notes/item9b-applet-spawn.md)).

Later, five `cosmic-panel-button` instances starting in lockstep hung in
`pthread_cond_wait` because private futexes were keyed by address alone and the instances
share identical stack and libc addresses: one process's `FUTEX_WAKE` claimed another's
waiter, 33–50 times per session. Private futexes are now keyed by thread group, as Linux
keys them by `(mm, addr)` (`a19267d`,
[lane-applets](../../artifacts/notes/lane-applets-2026-09-24.md)).

### The historical "busd wedge"

[ports/busd/README.md](../../ports/busd/README.md) still carries an investigation titled "W1
is a userspace tokio-runtime wedge". That verdict was later overturned: a per-thread kernel
syscall trace showed busd servicing the real cosmic-comp end to end and parking normally; the
"wedge" was a capture confound (heavy compositor output stalling the serial line under HVF)
on top of kernel bugs that were real and fixed along the way (the poll-deadline lost wake,
the timed-futex wake race, `accept4` flags). Read that section as history.

## Limitations & open work

- No document portal (needs FUSE): one `unknown destination
  org.freedesktop.portal.Documents` warning per login.
- Not implemented: `timedate1` (Settings' Date & time page), `SwitcherooControl`, the a11y
  bus, `locale -a` (so "Add language" lists nothing); `SetLocale` as a normal user is
  `AccessDenied`.
- logind "block" inhibitors for shutdown are not enforced; Ctrl-Alt-Del does not reach init;
  sleep states are unsupported.
- The screenshot permission is per-user state in `xdg-permission-store`, so the first
  non-interactive screenshot asks once (upstream behaviour).
- cosmic-session's launch-pad output queue is unbounded upstream; init contains it with the
  rotating log and the memory-pressure guard rather than fixing it.

See [limitations.md](limitations.md) for the full list.

## Code & tests

| Where | What |
|---|---|
| [artifacts/m6-session-data/start-cosmic-leandros](../../artifacts/m6-session-data/start-cosmic-leandros) | the session launcher |
| [ports/busd/](../../ports/busd/) | busd recipe and patches |
| [ports/dbus/session-pkg/](../../ports/dbus/session-pkg/) | `dbus-run-session`, `session.conf`, `.service` files |
| [ports/sysbus/src/](../../ports/sysbus/src/) | the system services and the `probe` client |
| [ports/portal/](../../ports/portal/), [ports/pipewire/](../../ports/pipewire/) | portal and PipeWire staging |
| [userland/dbusprobe](../../userland/dbusprobe/src/main.rs) | raw-wire D-Bus client for activation and the `ServiceUnknown` fast path |
| [userland/scmtest](../../userland/scmtest/src/main.rs) | the socket semantics cosmic-session's handshake needs |
| [userland/spawnwedge](../../userland/spawnwedge/) | the musl thread/fork lock hand-off that wedged cosmic-comp's first keybinding spawn |
| [ipc/src/port.rs](../../ipc/src/port.rs) | `LIVE_BUCKETS` |

## See also

- [README.md](README.md), [login.md](login.md), [limitations.md](limitations.md)
- [../audio.md](../audio.md) — the PipeWire server and the virtio-snd path
- [../kernel/threads-and-futex.md](../kernel/threads-and-futex.md) — futex keying
- [../servers/networking.md](../servers/networking.md) — AF_UNIX, SCM_RIGHTS, `shutdown(2)`
- [../userland/ports.md](../userland/ports.md)
