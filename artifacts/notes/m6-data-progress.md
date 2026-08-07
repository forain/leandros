# M6 session-choreography + data-surface prep — checkpoint

Lane: HOST-ONLY, repo-read-only. Write ONLY under
~/code/leandros-artifacts/m6-session-data/ and .../notes/.
Do NOT touch repo, QEMU, or other lanes' dirs.

## STATUS

### Task 1 — session choreography (IN PROGRESS, evidence gathered)
Evidence collected (all cited):
- start-cosmic: cosmic-epoch/cosmic-session/data/start-cosmic. bash; degrades on
  LeandrOS (no systemctl -> systemd block skipped; keyring dir absent -> skipped).
  RISKY line: `exec bash -c "exec -l '${SHELL}' -c '${0} --in-login-shell'"`
  login-shell re-exec (bash `exec -l`). Draft launcher will DROP this.
- cosmic-session/src/main.rs: process manager (launch-pad). FATAL-AT-SPAWN set uses
  `.expect()`:
  * compositor (default `cosmic-comp`) — main blocks on `env_rx.await.expect(...)`
    (main.rs:148-152) + spawn task `.expect("failed to launch compositor")` (comp.rs:149).
  * cosmic-settings-daemon — `.expect("failed to start settings daemon")` main.rs:255.
  * cosmic-notifications — `.expect("failed to start notifications daemon")` main.rs:306.
  * cosmic-panel — `.expect("failed to start panel")` main.rs:325.
  TOLERANT (start_component, logs error only, main.rs:505-560): cosmic-app-library,
  cosmic-launcher, cosmic-workspaces, cosmic-osd, cosmic-bg, cosmic-greeter,
  cosmic-files-applet, cosmic-idle.
- launch-pad lib.rs:198 `command.spawn().map_err(Error::Process)?` -> missing/non-exec
  binary => start() returns Err => the 4 `.expect()`s PANIC the whole session.
  Once spawned, a later crash => infinite restart (max_restarts=usize::MAX, main.rs:130;
  ExponentialBackoff 10ms) via tokio timer — NOT a panic, NOT `/bin/sleep`.
- Readiness gate = socketpair, NOT sleep/FIFO. comp.rs:109 UnixStream::pair;
  cosmic-comp writes length-prefixed JSON SetEnv{WAYLAND_DISPLAY(+DISPLAY)} over
  COSMIC_SESSION_SOCK once its wayland socket binds (cosmic-comp/src/session.rs:89-104,
  get_env at :59); session blocks on env_rx.await. Pure blocking socket read => works
  on LeandrOS. cosmic-session needs NO sleep binary and NO FIFO.
- systemd.rs: with --no-default-features (per m6-bins-manifest) systemd/logind cfg'd
  OUT. set/start/stop_systemd_target still compiled (main.rs calls them unconditionally)
  but they `run_optional_command("systemctl", ...)` which just warns if systemctl absent
  (systemd.rs:95-108). is_systemd_used() = /run/systemd/system exists = false. autostart
  feature also OFF (not in default) => no .desktop autostart scan.
- service.rs: session owns bus NAME `com.system76.CosmicSession`, serves iface at
  /com/system76/CosmicSession (2 methods: exit, restart). SESSION bus only (zbus
  default-features=false, features=["tokio"]). No system bus (logind cfg'd out).
- dbus-run-session (m5): busy-poll-on-regular-file, NO FIFO/sleep (m5 manifest §2).
  busd = session bus, /usr/libexec/busd, session.conf.
- settings-daemon binary is in pipewire-gap/out/ (NOT m6-session-bins/out/); needs
  stub libpipewire-0.3.so.0 staged (pipewire-gap/lib/<arch>/). FATAL if absent.

### Task 1 — DONE
notes/m6-session-choreography.md written; m6-session-data/start-cosmic-leandros
written (sh -n clean, no bashisms/sleep/FIFO, chmod +x).

### Task 2 — DONE (data/config surface)
Key results:
- cosmic-config: absent config = soft (unwrap_or_default/Entry::fallback);
  ONLY precondition = writable HOME/XDG_CONFIG_HOME (create_dir_all,
  cosmic-config lib.rs:244). Files are RON at $XDG_DATA/cosmic/<name>/vN/<key>.
- cursor: UNNECESSARY — embedded FALLBACK_CURSOR_DATA (cosmic-comp
  cursor.rs:52,60-75). (This is where anvil's cursor came from.)
- wallpaper: missing = None=>continue = BLACK, not crash (wallpaper.rs:146).
  Staged 135-byte 64x64 solid PNG at the fallback path
  /usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg (content-sniffed,
  ext irrelevant; ScalingMode default Zoom fills screen).
- icons: default theme "Cosmic" (Inherits Pop,hicolor); freedesktop-icons returns
  Option => blank icon soft-fail. Full set 2.8MB/676 inodes => DEFERRED.
- .desktop/dconf/gsettings/mime: confirmed UNNECESSARY for boot.

### Task 3 — DONE
notes/m6-data-manifest.md written (staged list, per-component missing-file table,
size/inodes ~4.7KB / <=5 inodes, mkfs snippet incl. pipewire stub, R1-R3 risks).

## LANE COMPLETE — all deliverables written. No live background children.
Deliverables:
- notes/m6-session-choreography.md
- notes/m6-data-manifest.md
- m6-session-data/start-cosmic-leandros (chmod 755)
- m6-session-data/shared/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg
