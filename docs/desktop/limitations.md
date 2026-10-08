# What the desktop still cannot do

The COSMIC desktop runs, logs in, renders on the GPU and shuts down cleanly on both
architectures, but it is not yet everything a Linux COSMIC install is. This page lists the
gaps that remain, why each exists, and which earlier gaps have closed — so that a stale
limitation is not rediscovered as a new bug. [TODO.md](../../TODO.md) is the live tracker;
this page is the explanation.

## Open gaps

### Session services

- **No document portal.** `xdg-document-portal` needs FUSE, which LeandrOS does not have.
  The FileChooser, Screenshot, Settings and Access portals work; applications receive plain
  `file://` URIs, and the portal frontend logs one `unknown destination
  org.freedesktop.portal.Documents` warning per login.
- **Some system services are absent**: `org.freedesktop.timedate1` (Settings' Date & time
  page), `net.hadess.SwitcherooControl`, and the accessibility bus `org.a11y.Bus`. Calls to
  them fail fast with `ServiceUnknown`. `locale -a` does not exist, so Settings' "Add
  language" list is empty.
- **Services run as the session user.** There is no system bus and no setuid helper, so the
  login1/locale1/UPower implementations cannot do anything that needs root. `SetLocale` as a
  normal user answers `AccessDenied` (`/etc` is root's); power-off is the one root operation
  that is supported, by delegation to init ([session.md](session.md#shut-down-and-restart)).
- **Power management is shutdown and reboot only.** Suspend, hibernate and hybrid sleep are
  `NotSupported`; logind "block" inhibitors for shutdown are accepted but not enforced
  (COSMIC only takes handle-power-key inhibitors); Ctrl-Alt-Del does not reach init yet; the
  greeter's own power buttons do nothing, because the greeter phase has no system bus.

### Login and privilege

- **No PAM stack and no utmpx.** greetd authenticates through `pam-leandros`, a Rust
  implementation of the libpam application ABI backed by `/etc/shadow`
  ([login.md](login.md#no-pam-stack-pam-leandros)); the greeter's lock-screen role links an
  older C shim from outside the repository. Whether to grow the shim or port Linux-PAM (which
  `dlopen`s modules) is undecided. Nothing shipped reads utmpx — upstream greetd has no utmpx
  code at all — so implementing it waits on a consumer that would make it testable.
- **The greeter's compositor runs as root.** In the user session every COSMIC process,
  cosmic-comp included, runs as the user. In the *greeter* session cosmic-comp stays root,
  because `libseat` is a shim with no seatd behind it to broker revocable device fds; only the
  greeter client drops to uid 990. That is a recorded gap, not a design.
- **The privilege model is Linux's minus capabilities.** Since the `perms` and `privcheck`
  work (2026-09-16, `e1ee68a`) path resolution, `AF_UNIX` connect, signals, mounts,
  credentials, scheduling, rlimits and device nodes follow Linux's rules (checked by
  `permtest` and a 124-case comparison against a Linux host). What remains: no real capability
  sets (a root `capset` drop is accepted with no effect); rlimits are stored but only
  `RLIMIT_NICE`/`RLIMIT_RTPRIO` are enforced; the private IPC syscalls, `SYS_SPAWN` and the
  serial debug write carry no credential check; `setsid()` still succeeds for a process-group
  leader; `protected_regular`/`protected_fifos` are not implemented. Details are in
  [../servers/security.md](../servers/security.md).

### Applications and toolkits

- **`cosmic-term` and `cosmic-settings` can crash on exit.** iced's SCTK event-loop thread
  holds a foreign `wl_display` that winit disconnects and frees while the thread is still
  awake; the thread then writes through the freed map (`wl_map_insert_at`). The kernel half —
  epoll waking that thread with `POLLNVAL` for a closed fd — is fixed; the residual
  use-after-free is in the application, and musl's mallocng unmapping the freed chunk means
  Linux with musl crashes the same way (glibc usually hides it). The fix belongs in iced or
  COSMIC and has deliberately not been patched. It costs nothing visible: the window is
  already closing.
- **Panel applets render with iced's CPU renderer.** Every other COSMIC client renders through
  wgpu on the GPU, but applets are clients of cosmic-panel's embedded server, which exposes no
  `zwp_linux_dmabuf_v1` unless its own GLES renderer exists at bind time.
- **On a host GL below GLES 3.1** (a virgl host without storage buffers), wgpu cannot create
  a device and the clients fall back to tiny-skia with a banner; the compositor still renders
  on the GPU. The Mac's default QEMU now provides GLES 3.1, so this is a fallback for unusual
  hosts rather than the Mac's normal path.
- **No Xwayland.** It is not shipped, and cosmic-comp is always started with
  `--no-xwayland`.
- **A virgl client that hits `EMFILE` while submitting loses its GPU context**, as on Linux.
  The defence is fd headroom (512 descriptors per process), not a different failure mode.

### Display, input and hardware

- **QEMU is the only place the desktop is verified.** The Raspberry Pi 5 and real-PC targets
  boot and have their own display paths, but COSMIC on real hardware is future work
  ([../hardware/README.md](../hardware/README.md)).
- **cosmic-comp is never fully idle.** It does an allocate/free cycle per wakeup; the kernel
  side of the idle cost was cut (greeter dispatches roughly halved, host QEMU CPU at idle
  ~2.5–3 % in a session) but the compositor-side churn is upstream behaviour.

## Closed since earlier records

These were listed as limitations in older documentation and are no longer true. They are
kept here because each was a real, recorded gap and the reason it closed is worth knowing.

| Earlier limitation | Now |
|---|---|
| **No D-Bus activation** — `busd` omitted `<servicedir>`, so no portal, no screenshot, no file chooser | busd activates `.service` files (`84ec91a`), spawns each service once for concurrent callers (`e1b6dcf`) and waits 25 s like dbus-daemon (`6593739`); login1, locale1, UPower, power-profiles, PolicyKit1, RealtimeKit1 and the portal all start on demand ([session.md](session.md)) |
| **VT switching lands, device arbitration does not** — a background VT could read live keyboard/pointer events | DRM master is per VT and follows the switch (`959710d`, `869014d`); `EVIOCGRAB` is enforced and `EVIOCREVOKE` implemented; VT 2..6 are real terminals with their own logins; a measured round trip shows cosmic-comp dropping and retaking master itself through the shim ([../servers/tty-and-vt.md](../servers/tty-and-vt.md)) |
| **The privilege boundary is thinner than it looks** — the greeter reached a `0700 root` `/run/user/0` and a root-owned socket because path resolution applied no search-permission test and `AF_UNIX` connect discarded the caller's identity | Search permission is enforced on every component and write permission on socket inodes; the greeter now uses its own `/run/user/990` ([login.md](login.md#greetds-control-socket-cannot-live-in-run)) |
| **A compositor death leaks kernel memory** — roughly 140 MB per `cosmic-comp` respawn; a fast respawn storm could exhaust the buddy allocator | It was user pages, not kernel heap: eager VMAs are backed by a power-of-two buddy block, and the unmapped tail was lost whenever the VMA was split or forked. `free_eager_tail` returns it (14.1k pages per death → −26) ([lane-greeterleak](../../artifacts/notes/lane-greeterleak-2026-09-24.md)) |
| **Closed windows leak a GPU texture each** | the approved `weak_window` cosmic-comp patch ([README.md](README.md#zero-cosmic-source-patches-with-one-approved-exception)) |
| **cosmic-comp runs as root** | true only for the greeter session; the user session's compositor runs as the user |
| **Software rendering (softpipe) by default** | GPU by default (Zink over Venus, else virgl); softpipe only by explicit opt-in |
| **No `#!` scripts** — every launcher had to be run as `sh <script>` | `execve` follows Linux `binfmt_script` semantics (`3762fb6`, `userland/exectest`); the launchers are still invoked through `sh` |
| **No power-off** — the shutdown dialog's Confirm failed | login1 → init → `reboot(2)` → ACPI S5 / PSCI, with a clean f2fs unmount |

## Refuted limitations

Recorded so they are not reopened without new evidence:

- **"libcosmic/iced cannot render."** The applications blocked in a D-Bus probe before they
  ever opened a Wayland connection ([session.md](session.md#busd-answers-serviceunknown)).
- **"More than ~4 applets kill the panel."** One listening socket could not be `dup`ed or
  passed over SCM_RIGHTS ([session.md](session.md#the-applet-ceiling-that-was-one-listening-socket)).
- **"The first launch of an application is slow."** On fresh images first and second launches
  measured the same to within noise (terminal ~1.1–1.4 s, file-chooser dialog ~0.9 s on both
  arches); a 60 s first dialog was host load from parallel builds.
- **"busd's tokio runtime wedges."** A capture confound; see
  [session.md](session.md#the-historical-busd-wedge).

## See also

- [README.md](README.md), [login.md](login.md), [session.md](session.md)
- [../../TODO.md](../../TODO.md) — the live open-work list
- [../servers/security.md](../servers/security.md) — the full privilege-gap list
- [../graphics/README.md](../graphics/README.md) — GPU paths and their host requirements
