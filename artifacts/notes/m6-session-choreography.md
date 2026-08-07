# M6 COSMIC session choreography — execution-ready boot plan

Host-only, repo-read-only analysis. Sources: `../cosmic-epoch` (checkout of
2026-07-21), `~/.cargo/.../launch-pad-93ee12ad4ef22597/5b516ee`, prior lane
notes (m5-session-manifest, m6-bins-manifest, pipewire-gap-design). All claims
cited `file:line`. cosmic-session built `--no-default-features` (systemd/logind
OFF, autostart OFF) per m6-bins-manifest.md:13.

---

## 0. TL;DR

- The whole session is one supervised process tree rooted at `cosmic-session`,
  which is a **launch-pad `ProcessManager`** (an in-process supervisor — NOT
  systemd). No systemd/logind/seatd is reached in the built configuration.
- **The only readiness gate that matters is compositor-ready, and it is a
  blocking read on a `socketpair`** (comp.rs:109, cosmic-comp/session.rs:99-104).
  No `sleep`, no FIFO, no polling anywhere in cosmic-session. The one other wait
  (dbus bus-ready) is inside `dbus-run-session` and already solved with
  busy-poll-on-a-regular-file (m5-session-manifest.md:74-85).
- **Four children are fatal-at-spawn** (a missing/non-executable binary panics
  the entire session): `cosmic-comp`, `cosmic-settings-daemon`,
  `cosmic-notifications`, `cosmic-panel`. Everything else is tolerant.
- Children are spawned **by bare name via PATH** (launch-pad `Command::new`),
  so all binaries must live in a PATH dir (`/usr/bin`).

---

## 1. Process tree (who spawns whom)

```
getty → login → start-cosmic-leandros            (our launcher; sets env)
  └─ dbus-run-session                             (m5; starts busd, exports DBUS_SESSION_BUS_ADDRESS)
      └─ cosmic-session                           (launch-pad ProcessManager + owns bus name com.system76.CosmicSession)
          ├─ cosmic-comp        [FATAL] gets COSMIC_SESSION_SOCK; sends back WAYLAND_DISPLAY   (comp.rs:98-149)
          ├─ cosmic-settings-daemon [FATAL]  main.rs:228-255
          ├─ (a11y: orca/…)     [tolerant, spawned only if a11y configured]   main.rs:269, a11y.rs
          ├─ cosmic-notifications [FATAL]  gets DAEMON_NOTIFICATIONS_FD        main.rs:291-307
          ├─ cosmic-panel       [FATAL]  gets PANEL_NOTIFICATIONS_FD          main.rs:310-327
          ├─ cosmic-app-library [tolerant]                                    main.rs:330
          ├─ cosmic-launcher    [tolerant]                                    main.rs:333
          ├─ cosmic-workspaces  [tolerant]  (binary NOT built — logs & continues) main.rs:336
          ├─ cosmic-osd         [tolerant]                                    main.rs:339
          ├─ cosmic-bg          [tolerant]                                    main.rs:342
          ├─ cosmic-greeter     [tolerant]  (binary NOT built — logs & continues) main.rs:345
          ├─ cosmic-files-applet[tolerant]  (binary NOT built — logs & continues) main.rs:348
          └─ cosmic-idle        [tolerant]  (binary NOT built — logs & continues) main.rs:351
```

Notes:
- **Spawn order is strictly sequential and gated**: comp is started first;
  cosmic-session then **blocks on `env_rx.await`** (main.rs:148) until comp
  reports its env. Only after that are settings-daemon, notifications, panel,
  and the tolerant set started, in the source order above.
- `cosmic-settings-daemon` gets a `parent: None` span and is started
  *before* notifications/panel (main.rs:224-255).
- notifications and panel are started as a **mutually-restarting pair** sharing a
  `socketpair` (notifications.rs:15-30, main.rs:271-327): each holds one end
  (`DAEMON_NOTIFICATIONS_FD` / `PANEL_NOTIFICATIONS_FD`); if one exits the other
  is force-restarted with a fresh socket (notifications.rs:66-124).
- `autostart` feature is OFF → the `.desktop`-scanning block (main.rs:353-459)
  is compiled out. **No `/etc/xdg/autostart` scan, no dependence on desktop
  files at session start.**
- systemd target/env calls (main.rs:160,262,264; systemd.rs:28-47) are compiled
  IN but are `run_optional_command("systemctl", …)` → just log a warning when
  systemctl is absent (systemd.rs:95-108). `is_systemd_used()` = does
  `/run/systemd/system` exist = **false** (systemd.rs:51-54), so the
  `#[cfg(feature="systemd")]` env-import/logind-inhibit block (main.rs:162-222)
  is compiled out entirely.

---

## 2. Fatal-at-spawn set and peer tolerance

**Mechanism** (decisive): launch-pad `start_process` does
`command.spawn().map_err(Error::Process)?` (lib.rs:198). A missing or
non-executable binary makes `spawn()` return `Err`, so `ProcessManager::start`
returns `Err`. cosmic-session calls `.expect(...)` on exactly four of these:

| child | call site | on missing binary | on later runtime crash |
|---|---|---|---|
| `cosmic-comp` | main.rs:148-152 `env_rx.await.expect(…)` + comp.rs:149 `.expect("failed to launch compositor")` | **session panics/aborts** | if it dies **before** sending SetEnv → `env_rx` closes → `.expect` **panics session**; after → on_exit sends Restart/Exit over dbus channel, outer loop restarts (main.rs:92-107,131-146) |
| `cosmic-settings-daemon` | main.rs:255 `.expect("failed to start settings daemon")` | **session panics** | launch-pad restarts it (max_restarts=usize::MAX, main.rs:130) |
| `cosmic-notifications` | main.rs:306 `.expect("failed to start notifications daemon")` | **session panics** | launch-pad restarts; pair-restart of panel too (notifications.rs:66) |
| `cosmic-panel` | main.rs:325 `.expect("failed to start panel")` | **session panics** | launch-pad restarts; pair-restart of notifications |

**Tolerant set** — `start_component` (main.rs:505-560) logs `error!` and returns
on any failure, never panics: `cosmic-app-library`, `cosmic-launcher`,
`cosmic-workspaces`, `cosmic-osd`, `cosmic-bg`, `cosmic-greeter`,
`cosmic-files-applet`, `cosmic-idle`. Missing binary → one error line, session
keeps running. Of these, **only app-library, launcher, osd, bg are actually
built** (m6-bins-manifest); workspaces/greeter/files-applet/idle will just log
"failed to start …" — harmless.

**Crucial distinction**: `.expect()` only fires on **spawn** failure
(binary missing / not executable / bad interpreter). Once a child execs
successfully, a later panic/exit is caught by launch-pad's `process_loop`
(lib.rs:434-488) and **restarted** (never a session panic) — except the special
comp-before-env case above.

**Peer tolerance of each fatal child** (can it come up without its peers?):
- `cosmic-comp` needs no peer (it is first). It DOES need a working DRM/KMS
  device + GBM/EGL (M4/K4) and a writable `$XDG_RUNTIME_DIR` to bind its wayland
  socket. If KMS init fails it exits before SetEnv → session death.
- `cosmic-settings-daemon` needs the session bus (to export its config/a11y/…
  interfaces) but does not require comp; its pipewire backend is deliberately
  inert (pipewire-gap-design.md §2). It must simply not fail *at exec*.
- `cosmic-notifications` / `cosmic-panel` each need `WAYLAND_DISPLAY` (guaranteed
  set by the time they spawn) and their notification-socket FD; they tolerate the
  other being briefly down (pair-restart logic).

---

## 3. The readiness handshake (no sleep / no FIFO)

`run_compositor` (comp.rs:98-121):
1. `UnixStream::pair()` → `(session, comp)` (comp.rs:109).
2. comp end is marked **blocking** + not-CLOEXEC, its raw fd passed to
   cosmic-comp as env `COSMIC_SESSION_SOCK=<fd>` (comp.rs:112-130).
3. cosmic-comp, once its wayland socket is bound, calls `run_socket` →
   `get_env` = `{WAYLAND_DISPLAY: <socket name>, [DISPLAY if xwayland]}` and
   writes a **length-prefixed (native-endian u16) JSON `SetEnv`** message down
   the fd (cosmic-comp/session.rs:59-104).
4. cosmic-session's IPC task parses it (comp.rs:33-96) and fires the oneshot;
   `main` unblocks at `env_rx.await` (main.rs:148).

**This is the compositor-ready barrier.** It is a plain blocking read on a Unix
`socketpair`, which LeandrOS supports fully. **cosmic-session needs no `sleep`
binary and no FIFO.** The env received (`WAYLAND_DISPLAY`, +`XDG_SESSION_TYPE`
added at main.rs:159) is what every subsequent child inherits, so no child can
be started with a stale/absent `WAYLAND_DISPLAY`.

Downstream ordering is implicit and synchronous: `start(...).await` per child
returns as soon as the child is **spawned** (not "ready"). COSMIC does not gate
panel-start on comp being fully drawn beyond the SetEnv handshake — panel simply
retries its wayland connect. No additional readiness waits exist to translate.

Restart backoff (launch-pad `ExponentialBackoff(10ms)`, main.rs:131-135;
lib.rs:258-286) uses **`tokio::time::sleep`** (an async timer) — NOT
`/bin/sleep`. The single wall-clock `tokio::time::sleep(2s)` at main.rs:501 is on
shutdown only. **No process-level sleep is ever invoked.**

---

## 4. Per-process environment table

Legend: **L** = must be set by the launcher (start-cosmic-leandros); **C** =
injected by cosmic-session for that child; **H** = handshake-provided (from
cosmic-comp). "—" = must be unset.

| var | comp | settings-daemon | panel | notifications | app-lib/launcher/osd | bg | who sets |
|---|---|---|---|---|---|---|---|
| `XDG_RUNTIME_DIR` | ✅ (binds wayland sock) | ✅ | ✅ | ✅ | ✅ | ✅ | **L** (`/run/user/0`) |
| `WAYLAND_DISPLAY` | — (it is the server) | ✅ | ✅ | ✅¹ | ✅ | ✅ | **H→C** |
| `DBUS_SESSION_BUS_ADDRESS` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **L** (dbus-run-session) |
| `DISPLAY` | — | inherit | inherit | inherit | inherit | inherit | **L** unset |
| `COSMIC_BACKEND=kms` | ✅ | — | — | — | — | — | **L** |
| `COSMIC_DRM_ALLOW_DEVICES` | optional | — | — | — | — | — | **L** (unset=allow all) |
| `COSMIC_SESSION_SOCK` | ✅ (fd) | — | — | — | — | — | **C** (comp.rs:130) |
| `DAEMON_NOTIFICATIONS_FD` | — | — | — | ✅ (fd) | — | — | **C** (main.rs:275) |
| `PANEL_NOTIFICATIONS_FD` | — | — | ✅ (fd) | — | — | — | **C** (main.rs:280) |
| `ICED_BACKEND=tiny-skia` | ✅² | — | ✅ | ✅ | ✅ | — | **L** |
| `XDG_SESSION_TYPE=wayland` | set | ✅ | ✅ | ✅ | ✅ | ✅ | **L**+**C** (main.rs:159) |
| `XDG_CURRENT_DESKTOP=COSMIC` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **L** |
| `HOME` / `XDG_CONFIG_HOME` | ✅³ | ✅³ | ✅³ | ✅³ | ✅³ | ✅³ | **L** |
| `XDG_DATA_DIRS` | ✅⁴ | ✅ | ✅⁴ | ✅⁴ | ✅⁴ | ✅⁴ | **L** |
| `XDG_CONFIG_DIRS` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **L** (`/etc/xdg`) |
| `XCURSOR_THEME`/`_SIZE` | optional⁵ | — | — | — | — | — | **L** |
| `RUST_LOG` | optional | optional | optional | optional | optional | optional | **L** |

¹ notifications.rs:44 strips `WAYLAND_SOCKET` (not `WAYLAND_DISPLAY`) from the
  child env — a leftover-fd guard, not a wayland-connect disable.
² comp itself renders window titles/OSD via cosmic-text+iced_tiny_skia
  (m5-session-manifest.md:88-143); tiny-skia keeps it CPU-only.
³ cosmic-config `Config::new` does `create_dir_all($XDG_CONFIG_HOME/cosmic/…)`
  and errors `NoConfigDirectory` if neither HOME nor XDG_CONFIG_HOME is set
  (cosmic-config lib.rs:244,432-443). Components mostly `.ok()`→default, but set
  it so state can persist and to avoid the failure path entirely.
⁴ resolves icon themes, `/usr/share/backgrounds`, `.desktop` files, AND
  cosmic-config **system defaults** at `$XDG_DATA_DIR/cosmic/<name>/vN/<key>`
  (cosmic-config lib.rs:234-241,481-487).
⁵ optional — comp always has an embedded fallback cursor
  (cursor.rs:52,60-75); missing theme ⇒ fallback pointer, not a crash.

**cosmic-comp backend auto-detect** (backend/mod.rs:25-33): with `COSMIC_BACKEND`
unset it picks x11/winit iff `DISPLAY` **or** `WAYLAND_DISPLAY` is set, else kms.
For M6 we force `COSMIC_BACKEND=kms` **and** unset both, so a leaked var can't
divert it to the nested path.

---

## 5. D-Bus surface vs busd (zbus feature question)

**cosmic-session's own bus use is minimal** (service.rs, main.rs:83-90):
- Connects to the **session** bus (`zbus::connection::Builder::session()`).
- `RequestName("com.system76.CosmicSession")`.
- Serves one object `/com/system76/CosmicSession` implementing interface
  `com.system76.CosmicSession` with two methods: `exit()`, `restart()`
  (service.rs:14-25). No signals, no properties.
- zbus is `default-features=false, features=["tokio"]` (Cargo.toml) → pure-Rust
  D-Bus, **no libdbus link** (m6-bins-manifest.md:15).
- Because systemd/logind are compiled out, cosmic-session makes **zero
  system-bus calls** (the logind inhibit + systemd1 Manager proxy at
  main.rs:162-217 / systemd.rs:56-92 are `#[cfg]`-gated out).

**What busd must implement** for the session to come up: the standard
`org.freedesktop.DBus` driver surface that zbus exercises on connect + name
own + serving:
- `Hello`, `RequestName`, `ReleaseName`, `NameHasOwner`, `GetNameOwner`,
  `AddMatch`/`RemoveMatch`, `GetId`, and method **call routing** to a
  name-owning peer, plus the `NameOwnerChanged`/`NameAcquired`/`NameLost`
  signals zbus subscribes to. This is exactly the busd broker's core; it was
  already container-tested end-to-end (m5-session-manifest.md:57-72,
  ports/dbus/RUNTIME-NOTES.md). **No extra busd capability is required by
  cosmic-session beyond what a stock session broker provides.**

**Wider fan-out (the other children raise the busd bar, not cosmic-session):**
- `cosmic-settings-daemon` **owns several session-bus names** and serves config /
  a11y / power / … interfaces; the settings/panel/osd clients call them. This is
  ordinary name-own + method-call + **PropertiesChanged / arbitrary signals**.
  busd must therefore support signal broadcast + match-rule delivery (it does).
- Everything is **session bus only**. Nothing in the built set needs a **system**
  bus (no logind/UPower/NetworkManager path is compiled/reachable at bring-up;
  cosmic-settings' networking/bluetooth pages would want a system bus, but those
  pages failing is a settings-panel issue, not a session-boot issue).
- **zbus feature surface to keep on the busd side**: p2p is not used (cosmic uses
  the brokered bus), so busd must be a real message **broker** (route by
  destination, track name ownership) — a bare peer-to-peer zbus server is NOT
  enough. busd 0.5.0 is a broker; adequate.

**Action for M6**: confirm busd answers `RequestName`/`GetNameOwner` and routes
a method call between two clients (a settings-daemon name + a settings client).
If cosmic-session logs "Failed to request name com.system76.CosmicSession" it
will still *run* (the name-serve is not `?`-propagated past `_conn` — actually it
IS `?` at main.rs:83-90, so a RequestName failure aborts main). **So busd MUST
grant the name.** Flagged as risk R2.

---

## 6. LeandrOS-missing-primitive workarounds (every one)

| upstream primitive | where | LeandrOS gap | workaround (used in launcher/plan) |
|---|---|---|---|
| `systemctl` (systemd user mgr) | start-cosmic; systemd.rs:28-47; main.rs:160,262,264 | no systemd | none needed — `run_optional_command` logs a warning and continues (systemd.rs:95-108); `is_systemd_used()`=false skips the whole import block. Launcher omits it. |
| `exec -l "$SHELL"` login-shell re-exec | start-cosmic (upstream) | brush `exec -l` unverified; purpose is to source login env we don't have | **DROP it.** Launcher sets env explicitly instead. |
| `gnome-keyring-daemon` + `/run/user/UID/keyring` | start-cosmic | no keyring | guarded by `[ -d …/keyring ]` (false) → skipped; launcher omits. `SSH_AUTH_SOCK` simply unset. |
| `mapfile`, `${!var}`, `[[ ]]` (bashisms) | start-cosmic systemd block | brush is POSIX+partial bash | unreached (systemctl absent); launcher is pure POSIX sh, `sh -n` clean, no bashisms. |
| `sleep` binary | — | **no `sleep`** | **not needed anywhere**: cosmic-session readiness = socketpair; restart backoff = tokio async timer; dbus-ready = busy-poll-regular-file (m5). Confirmed zero `/bin/sleep` dependence. |
| FIFO / `mkfifo` rendezvous | — | FIFOs are fake (no blocking) | **not used**: comp-ready via socketpair, dbus-ready via regular-file poll. No FIFO anywhere in the path. |
| `/run/user/$UID` auto-create | comp needs it for wayland sock; dbus for its sock | only `/run/user/0` exists (0700 tmpfs) | launcher `mkdir -p "$XDG_RUNTIME_DIR"; chmod 700`. For root (uid 0) `/run/user/0` already exists (m5 mkfs snippet). Non-root uids need the mkdir. |
| PATH lookup of children | launch-pad `Command::new(bare)` lib.rs:172 | — | launcher forces `/usr/bin` onto PATH; all 8 session bins install to `/usr/bin`. |
| writable cosmic-config dir | cosmic-config `create_dir_all` lib.rs:244 | HOME may be unset | launcher sets `HOME=/root`, `XDG_CONFIG_HOME=$HOME/.config`, mkdir -p. |
| journald / logind / seatd / udevd | systemd.rs; logind feature | none present | all compiled OUT (`--no-default-features`); comp uses libseat's builtin/logind-less seat (M4 libseat shim) + fake `/sys`. Not re-litigated here. |
| dconf / gsettings (`DCONF_PROFILE=cosmic`) | start-cosmic exports it | no dconf | nothing **reads** it at runtime (COSMIC uses cosmic-config, not gsettings); env var is inert. Launcher omits it. |

---

## 7. Bring-up sequence the launcher realizes

1. login (root) → `start-cosmic-leandros`.
2. Ensure `/run/user/0` (0700), `HOME`/`XDG_*` set, PATH has `/usr/bin`, DISPLAY
   & WAYLAND_DISPLAY unset, `COSMIC_BACKEND=kms`, `ICED_BACKEND=tiny-skia`.
3. `exec dbus-run-session -- cosmic-session` (busd comes up, bus addr exported;
   busy-poll-regular-file readiness — no sleep/FIFO).
4. cosmic-session owns `com.system76.CosmicSession`, spawns `cosmic-comp` with
   `COSMIC_SESSION_SOCK`.
5. cosmic-comp inits KMS/GBM/EGL, binds `$XDG_RUNTIME_DIR/wayland-1`, sends
   `SetEnv{WAYLAND_DISPLAY=wayland-1}`; session unblocks.
6. session spawns settings-daemon (pipewire inert), notifications+panel (socket
   pair), then app-library/launcher/osd/bg (+ the 4 unbuilt tolerant ones log &
   continue).
7. Desktop is "up" = comp drawing + panel visible. Missing data (icons, real
   wallpaper) degrades to blank/solid, never a crash (see m6-data-manifest.md).

---

## 8. Verification checklist (for the on-target M6 wave — this lane can't run QEMU)

1. All 8 built bins + `cosmic-settings-daemon` (pipewire-gap/out) in `/usr/bin`,
   executable, correct arch; `libpipewire-0.3.so.0` stub on the loader path
   (else settings-daemon spawn fails → **session panic**).
2. Launcher: `start-cosmic-leandros` runs under brush; env asserted with `env`.
3. Confirm cosmic-comp reaches SetEnv (grep session log for "got environmental
   variables from cosmic-comp"). If session dies at
   "failed to receive environmental variables" → comp died pre-SetEnv (KMS/EGL).
4. busd grants `com.system76.CosmicSession` (no "Failed to request name").
5. panel + notifications stay up (no restart storm in the log).
6. No `/bin/sleep`-not-found or FIFO-hang symptoms (there should be none).
