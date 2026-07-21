# Wayland + COSMIC Desktop on LeandrOS — Implementation Plan

Goal: run the COSMIC Desktop Environment **unmodified** (source: `../cosmic-epoch`) on
LeandrOS, both x86_64 and aarch64, in QEMU. "Unmodified" means: no COSMIC source
patches. Build-configuration choices (cargo feature flags such as
`--no-default-features`) are allowed. Everything *beneath* COSMIC — kernel, libc,
system libraries, daemons — is ours to implement, port, or shim.

Status: PLAN (2026-07-21). Based on a full code survey of both trees; file references
below were verified at planning time.

---

## 1. The shape of the problem

COSMIC's Linux coupling lives almost entirely in **system libraries and build flags**,
not in COSMIC's own code:

- cosmic-comp's Wayland *server* side is pure-Rust (`wayland-backend`) — **no
  libwayland-server needed**. All protocol implementations (xdg-shell,
  wlr-layer-shell, the cosmic-protocols suite, viewporter, fractional-scale, …) ship
  inside cosmic-comp/Smithay. The OS provides only **transport + graphics + input**.
- The only bare-metal backend is `kms`: **DRM/KMS + GBM + EGL + GLES2 is mandatory**.
  The GlowRenderer compiles GLSL at startup and aborts without a live GLES2 context.
  "Software rendering" in cosmic-comp still means Mesa (llvmpipe via
  `EGL_MESA_device_software`) — there is **no CPU-only scanout path**.
- Native libs cosmic-comp links: libseat, libinput, libudev, libgbm, libdrm,
  libxkbcommon (+ XKB data files), pixman, libdisplay-info, libEGL/libGLESv2.
- systemd/logind are cleanly removable: build cosmic-comp, cosmic-session,
  cosmic-greeter with `--no-default-features`. Absence is runtime-detected.
- A **D-Bus session bus is a hard requirement** (cosmic-session aborts without one).
- Minimal fatal-at-spawn process set: `cosmic-comp`, `cosmic-settings-daemon`,
  `cosmic-panel`, `cosmic-notifications`, supervised by `cosmic-session`, launched by
  `start-cosmic` (bash — brush covers this). Launcher/app-library/bg are non-fatal
  but wanted for usable UX. XWayland, PipeWire, UPower, NetworkManager,
  accountsservice all degrade gracefully — **out of scope**.
- Most clients (settings, applets, notifications, osd, bg, launcher-by-default)
  render with **tiny-skia on the CPU and post wl_shm buffers** — no GPU needed
  client-side. Exceptions: cosmic-workspaces (wgpu by default — disable via feature
  flags or accept its non-fatal absence) and **cosmic-panel**, which uses smithay
  client `use_system_lib` + `wayland-egl` and therefore **dlopens
  libwayland-client/libwayland-egl and needs client-side EGL/GLES**.

### Hard blockers in today's kernel (from survey)

| # | Gap | Where | Why it blocks |
|---|-----|-------|---------------|
| 1 | `sendmsg`/`recvmsg` ignore `msg_control` — **no SCM_RIGHTS** | `kernel/src/syscall.rs:5552`, `servers/net/src/lib.rs:1232` | Wayland's transport passes every shm-pool/keymap/dmabuf fd over the socket; D-Bus needs it too |
| 2 | File-backed `MAP_SHARED` silently degrades to a private copy (no page cache/VMO); memfd is a tmpfs file that maps privately; no F_SEAL; no /dev/shm | `kernel/src/syscall.rs:1436` | wl_shm — every COSMIC client draws through shared memory |
| 3 | DRM live node is legacy-only: no SET/DROP_MASTER, GETCAP gaps, no DESTROY_DUMB, no ADDFB2/planes/atomic/PRIME/GEM ioctls wired (richer `drivers/src/drm_driver.rs` exists but is not connected to `/dev/dri/card0`) | `drivers/src/drm_device_interface.rs` | Mesa GBM + Smithay DrmDevice probe these at init |
| 4 | No sysfs at all; static /dev; no udev | `servers/vfs` | Mesa's libdrm reads `/sys/dev/char/…`; libinput wants udev enumeration |
| 5 | No VT_*/KD* ioctls, no seatd/logind | `servers/tty` | libseat needs *a* backend (solved by shim, see §3) |
| 6 | evdev exposes a keyboard only — no pointer, no EV_REL/EV_ABS | `servers/evdev/src/lib.rs` | libinput would find no pointer; virtio-tablet exists but isn't surfaced |
| 7 | AF_UNIX caps 16 sockets/16 paths, paths are byte-matched not VFS nodes; epoll busy-polls with caps 16 instances/32 interests | `servers/net/src/lib.rs:881` | A compositor + bus + dozen clients blow past all of these |
| 8 | No Rust std port, static-only no_std userland, no dynamic linker in use | `targets/*.json` | COSMIC is std+tokio; comp/panel/Mesa dlopen at runtime |

Not needed (verified): SEQPACKET (Wayland/D-Bus use STREAM), shared futexes (musl and
wl_shm use private ones), pidfd, rseq, splice, vDSO/NSS (musl doesn't need them),
signalfd for tokio (it uses a self-pipe; calloop may want it — small add), real
VT switching, devtmpfs/uevent-netlink (no hotplug in QEMU), full udevd/seatd ports.
eventfd/timerfd already work.

---

## 2. Strategic decisions

### D1. Toolchain: build COSMIC for `*-unknown-linux-musl`, **dynamically linked**, with a real ld.so. No Rust std port.

The kernel already speaks Linux syscall numbers (that's why relibc's linux Pal works
unmodified), so a musl binary runs as-is once the missing syscalls exist. Rust std
for musl is free and mature — porting std to a leandros target would be a large,
permanent maintenance burden for zero gain.

Dynamic (not static) because **dlopen is on the critical path in at least three
places** and static musl's dlopen is a failing stub:
- cosmic-comp's EGL loading (khronos-egl/libloading dlopens `libEGL.so.1`),
- Mesa's GBM/DRI loader dlopens the gallium megadriver,
- cosmic-panel dlopens `libwayland-client.so`/`libwayland-egl.so` (`use_system_lib`).

Kernel work: `PT_INTERP` support in the ELF loader (map ld.so, correct auxv:
AT_PHDR/AT_ENTRY/AT_BASE/AT_RANDOM/…), ship `ld-musl-<arch>.so.1` + shared libs in
the image. Note: this is for the *COSMIC world* only — the existing relibc/no_std
userland stays as-is.

**Fallback** (if ld.so bring-up stalls): static musl everywhere it can work +
`dlopen`-self shim (`--export-dynamic`, dlopen(NULL)/dlsym) with Mesa's megadriver
and libwayland statically linked. Fragile; only if forced. Decide via spike S4.

### D2. Graphics: Mesa **softpipe** via gallium `kms_swrast` over **dumb buffers + legacy KMS**. llvmpipe and dmabuf/PRIME deferred.

- `kms_swrast` renders on the CPU into buffers allocated via
  `CREATE_DUMB`/`MAP_DUMB`/mmap on any KMS node and scans out via legacy
  `ADDFB`+`SETCRTC`/`PAGE_FLIP` — exactly what the virtio-gpu node already mostly
  does. No GEM-object GPU path, no PRIME, no atomic needed for bring-up (Smithay has
  a legacy KMS fallback).
- **softpipe first, not llvmpipe**: llvmpipe means static-linking LLVM and doing
  per-arch codegen bring-up *twice* (both-arches rule). softpipe is portable C.
  Accept that it will be slow under TCG — correctness first, llvmpipe is the perf
  wave. (Consistent with the MAME-on-TCG experience.)
- virgl/Venus is a dead end on the macOS dev host (no host EGL — already confirmed
  during the Venus effort) — only revisit on a Linux host, later, as perf work.
- linux-dmabuf: cosmic-comp advertises it per-backend, but **shm clients don't need
  it**. Defer PRIME/dmabuf until the wgpu/panel-EGL wave; panel's client EGL should
  use Mesa's swrast-over-wl_shm Wayland platform path.
- Kernel adds (small, bounded): wire `SET_MASTER`/`DROP_MASTER`, `DESTROY_DUMB`,
  `GEM_CLOSE`, missing `GET_CAP`s, `ADDFB2` (LINEAR only) into the live node —
  much of this exists unwired in `drivers/src/drm_driver.rs`; plus a **minimal
  read-only synthetic sysfs** (`/sys/dev/char/226:0`, `/sys/class/drm`,
  `/sys/class/input`) because Mesa's C libdrm resolves devices through sysfs
  (Smithay's own drm-rs does raw ioctls and doesn't care).

### D3. Seat/udev/input: **shim libseat and libudev; port real libinput and libxkbcommon**. No sysfs-udevd, no seatd, no VT work.

- **libseat ABI shim** (~10 functions): running as root, `open_device()` just opens
  `/dev/dri/card0` and `/dev/input/event*` directly; `switch_session` is a no-op.
  Single seat, single session, no VT switching → the entire VT_*/KD* ioctl family
  drops out of scope.
- **libudev ABI shim**: static enumeration of card0 + input nodes from the device
  model (readable off the synthetic sysfs for consistency); the hotplug monitor
  returns an fd that never fires (QEMU device set is fixed at boot).
- **Port real libinput** (C): Smithay drives `Libinput::new_with_udev` with real
  stateful logic (tap, gestures, accel) — reimplementing its ABI is more fragile
  than porting it. It sits happily on the shims.
- **Port libxkbcommon** + install XKB data files (`/usr/share/X11/xkb`); also
  libdisplay-info and pixman (both small, portable C).
- Kernel: surface **virtio-tablet as an EV_ABS/BTN_LEFT evdev node** (absolute
  pointer = no pointer-grab pain in QEMU; the driver exists, it's just not exposed),
  extend evdev beyond EV_KEY (EV_ABS/EV_REL/EV_SYN, EVIOCGBIT for abs/rel, EVIOCGABS).

### D4. D-Bus: **busd first** (the zbus project's own Rust bus), reference `dbus-daemon` port as fallback.

zbus needs: EXTERNAL auth (uid via SO_PEERCRED), Hello, RequestName/ReleaseName,
AddMatch, unique-name routing, signal broadcast, NameOwnerChanged. busd is written
by the zbus authors against exactly this surface, is pure Rust (builds trivially in
our musl world, reuses our SCM_RIGHTS/epoll work), and we control it. If busd
proves immature in practice, the reference C dbus-daemon is highly portable and is
the least-behavioral-risk fallback. dbus-broker is rejected (Linux-coupled:
SO_PEERSEC, cgroups).

### D5. Process/session glue

- `start-cosmic` runs under **brush** (already ported); verify its bashisms early.
- Boot path: login → root → `start-cosmic` (greeter/greetd out of scope).
- Filesystem conventions: tmpfs at `/run/user/$UID` (0700 — Wayland + bus sockets),
  `/dev/shm`, `/run/dbus`; fonts + cosmic-icons + XKB data + cosmic config schemas
  installed into the f2fs image (initrd stays lean).

---

## 3. Kernel work list (grouped, roughly ordered)

**Wave K1 — transport (blockers 1, 2, 7):**
- SCM_RIGHTS: parse/build cmsgs in sendmsg/recvmsg, translate fds across processes,
  MSG_CMSG_CLOEXEC, MSG_CTRUNC semantics, queued-fd lifetime on socket close.
- SO_PEERCRED (D-Bus EXTERNAL auth).
- Shared file-backed mmap: per-file VMO/page-cache so `MAP_SHARED` mappings of the
  same file (incl. via an fd received over SCM_RIGHTS) alias the same physical
  pages, with refcounting across fork/munmap. Scope to tmpfs/memfd first; f2fs
  MAP_SHARED can stay degraded. memfd becomes genuinely shareable;
  `F_ADD_SEALS`/`F_GET_SEALS` with SEAL_SHRINK honored (wl_shm's actual need).
- AF_UNIX: raise caps 16→512, bind creates real VFS socket nodes (S_IFSOCK) in
  tmpfs, connect resolves through VFS; keep abstract-namespace byte matching.
- Mounts: /dev/shm, /run/user/$UID.

**Wave K2 — event loop:**
- epoll: replace the yield busy-loop with real blocking on waitqueues; raise caps
  (instances 16→64, interests 32→512); verify EPOLLET against tokio/mio + calloop
  patterns; eventfd/timerfd wakeup integration.
- signalfd4 (calloop signal sources); minimal inotify (init1/add_watch return a
  valid fd that never fires — keeps settings-daemon's config-watch alive without
  live reload).
- /proc additions as flushed out by std/tokio (`/proc/self/exe` at minimum).

**Wave K3 — ELF loader:**
- PT_INTERP + auxv for dynamic binaries; both arches.

**Wave K4 — graphics + input (blockers 3, 4, 6):**
- Wire the dormant `drm_driver.rs` ioctls into the live node: SET/DROP_MASTER,
  GET_MAGIC/AUTH_MAGIC, DESTROY_DUMB, GEM_CLOSE, ADDFB2(LINEAR), full GET_CAP set;
  properties/planes/atomic deferred to the perf wave.
- Minimal synthetic sysfs (read-only): /sys/dev/char, /sys/class/drm,
  /sys/class/input.
- evdev: virtio-tablet EV_ABS node, EV_REL, extended EVIOC* ioctls.

---

## 4. Userland porting list

| Component | Approach | Size |
|---|---|---|
| musl toolchains (both arches) + sysroot | build infra | M |
| ld.so + shared-lib packaging | musl's own | M (kernel K3 is the real work) |
| Mesa (EGL, GLESv2, GBM, gallium kms_swrast + softpipe) + libdrm | port (meson cross) | **XL — the long pole** |
| libinput | port | M |
| libxkbcommon + XKB data | port + data files | S |
| pixman, libdisplay-info | port | S |
| libudev shim | write (~1k LoC C) | S |
| libseat shim | write (~300 LoC C) | S |
| busd (D-Bus session bus) | build; dbus-daemon port as fallback | S–M |
| libwayland-client/-egl (panel only) | port (small C lib) | S |
| cosmic-epoch components, `--no-default-features` where flagged | build | M (build wrangling) |
| fonts, cosmic-icons, config schemas, XDG env | packaging | S |

---

## 5. Milestones (each: QEMU-verified on **both** x86_64 and aarch64)

**M0 — Spikes (parallel, ≤1 day each; kill the unknowns first)**
- S1: static-musl hello + tokio echo server on LeandrOS (proves std/tokio surface;
  eventfd/timerfd expected green — confirm under load).
  **ON-TARGET RUN DONE 2026-07-21 — aarch64 fully green, x86_64 has a
  tokio-bootstrap hang.** aarch64: hello-std all OK (PROC_SELF_EXE quirk: returns
  "/bin/init"); tokio-echo-selftest SUMMARY pass=3 fail=0 skip=1 — multi-thread
  runtime (2 workers), 400 UDS echoes across 4 concurrent clients, timers
  accurate, mpsc fan-in; TCP SKIP: loopback bind() → EINVAL (real gap, low
  priority — Wayland/D-Bus need only UDS). x86_64: hello-std identical clean
  pass, but tokio-echo-selftest prints START then hangs BEFORE "RUNTIME: OK" —
  QEMU pegged ~307% CPU, no panic/fault on serial, Ctrl-C dead (PID 1 I/O stuck
  behind it).
  **RESOLVED (commit 82d0cc3): re-entrant RUN_QUEUE spinlock deadlock** —
  `sys_sigprocmask`/`sys_sigaction` (`sched/src/signal.rs:340/:308`) dereferenced
  user pointers while holding RUN_QUEUE; a demand-paging fault on the mask page
  re-entered the scheduler lock (page_fault → handle_page_fault →
  lock_leader_address_space), freezing all 4 vCPUs IF=0. Latent SMP bug, not arch
  logic — aarch64 just happened to have the page resident; tokio's signal-driver
  setup was merely the first workload to hit the window. Fix: user reads before
  lock, user writes after release. Verified: tokio selftest pass=3 skip=1 on BOTH
  arches + sigtest/polltest/vfstest/waittest baselines green.
  **K1 guardrail derived from this:** never touch user memory under RUN_QUEUE or
  any IRQ-off spinlock — use the fault-safe validate_user_buf/read_user_buf/
  write_user_buf paths; grep-gate new syscalls for raw `core::ptr::read/write` on
  user pointers under locks. Known same-shape hazard to keep clean: epoll_wait
  holds EPOLL_INSTANCES.lock() across probe_fd_events_seq → vfs/net handlers
  (`kernel/src/syscall.rs:5887`) — safe today, but K1 leans on it hard.
  **HOST-BUILD DONE 2026-07-21.** All 4 binaries (hello-std,
  tokio-echo-selftest × both arches) built static ET_EXEC, zero DT_NEEDED/PT_INTERP,
  with rust-lld + rustc self-contained musl CRT alone (no zig/docker/cross-gcc);
  needs `cargo +nightly` (stable lacks musl std here) and **`-C
  relocation-model=static`**. Artifacts + recipe:
  `~/.claude-forain/jobs/afde2e74/tmp/s1-musl-spike/`.
  **Landmine → D1/K3:** default x86_64-musl output is static-PIE (ET_DYN, first
  PT_LOAD at vaddr 0, self-relocating) while aarch64 defaults to ET_EXEC — and our
  loader (`elf/src/lib.rs`) accepts ET_DYN (:116) but maps literal p_vaddr with no
  load bias (:235) and applies no relocations (:306), so a static-PIE would map
  onto the null page. K3 (PT_INTERP) must therefore also add: (a) non-zero ET_DYN
  load bias applied to PT_LOADs + e_entry, (b) R_*_RELATIVE relocation processing
  or correct AT_BASE/AT_PHDR for self-relocating rcrt1. Until then every musl
  binary we build must force relocation-model=static; third-party PIE prebuilts
  won't run.
- S2: SCM_RIGHTS + shared-memfd two-process pixel test (spec for K1; will fail
  today — it's the acceptance test).
  **DONE 2026-07-21 — test committed (`userland/scmtest`, commit 558310b), all 4
  subtests FAIL identically on both arches, as designed.** Symptoms: recvmsg
  returns data but `msg_controllen` untouched / control buffer zeroed (cmsgs
  dropped both directions — `servers/net/src/lib.rs:1232` reads only
  iov fields at msghdr offsets 16/24, never 32/40/48); MSG_CTRUNC never set
  (`msg_flags` never written); memfd pixel test blocked behind fd-pass, parent
  mapping confirms MAP_SHARED never aliased; F_ADD_SEALS/F_GET_SEALS fake-succeed
  via VFS fcntl catch-all (`servers/vfs/src/lib.rs:3161`) and post-"seal"
  ftruncate succeeds. Plain sendmsg/recvmsg data transfer + socketpair +
  memfd_create + ftruncate plumbing all work — gaps are cleanly scoped. K1 is
  done when scmtest goes 4/4 PASS both arches with no test changes. (Test nit for
  later: parent/child printf interleaves byte-wise on shared console — rendezvous
  before printing if it gets annoying.)
- S3: Mesa meson cross-compile probe for musl (does kms_swrast+softpipe configure?
  where does dlopen bite?).
  **DONE 2026-07-21 — verdict: YES, fully builds.** Mesa 25.3.6 + libdrm 2.4.134
  cross-compile configure→compile→link→install to x86_64-musl on macOS via
  `zig cc -target x86_64-linux-musl` (zig 0.16.0), surfaceless+drm platforms,
  softpipe (no `swrast`/`kms_swrast` gallium option anymore — kms_swrast is winsys
  inside the megadriver). ZERO musl-vs-glibc source issues; 3 host-side fixes only
  (pip mako/packaging/pyyaml + PYTHONPATH for ninja; brew bison ≥3; zig-cc wrapper
  merging `-Wl,--version-script <file>` into `=` form — this one will recur).
  Workdir + reproducible scripts + NOTES.md:
  `~/.claude-forain/jobs/afde2e74/tmp/s3-mesa-probe/`.
  Remaining delta: `-Dplatforms=wayland` needs host wayland-scanner + cross
  libwayland(+libffi) — moderate, scoped, not attempted.
  **dlopen reality check (≥24.1 architecture):** no per-driver `*_dri.so` —
  softpipe/kms_swrast live in `libgallium-25.3.6.so` linked as hard DT_NEEDED;
  the ONLY runtime dlopen is libgbm → `/usr/lib/gbm/dri_gbm.so`
  (GBM_BACKENDS_PATH). Runtime ship set: libEGL.so.1, libGLESv2.so.2, libgbm.so.1,
  dri_gbm.so, libgallium, libdrm.so.2, libexpat.so.1, libz.so.1 — and every one
  NEEDs musl `libc.so`, i.e. the GL stack rides on ld-musl (D1), not relibc.
  Build strategy: keep building C deps on macOS/zig; stand up an Alpine container
  (colima/Docker) later as integration/runtime-test box when assembling the full
  library stack.
- S4: dynamic musl binary + trivial dlopen under a hand-loaded ld.so → confirms D1
  primary vs fallback.
- S5: busd + zbus client roundtrip on Linux-musl (host), then on LeandrOS after K1.
  **BUILD PROBE DONE 2026-07-21 — builds clean + static on both arches, no
  blockers.** busd 0.5.0 (commit c6f2e91, zbus 5.14.0 git-pinned fork), 100% pure
  Rust (rustix linux_raw backend — no C toolchain at all), S1 toolchain recipe
  verbatim, ET_EXEC/0 DT_NEEDED verified. Minimal broker slice already the
  default (zbus features tokio+bus-impl). CLI: `-a unix:path=...`,
  `--print-address`, `--ready-fd`; default address `unix:dir=$XDG_RUNTIME_DIR`.
  Two integration scope-adds: (1) busd unconditionally reads
  `/usr/share/dbus-1/session.conf` before honoring `-a` — ship a minimal
  `<busconfig>` on the image (or `--config`); (2) confirmed no dbus-run-session
  equivalent in-repo → write our own launcher (spawn busd, await
  --ready-fd/--print-address, export DBUS_SESSION_BUS_ADDRESS, exec child).
  Runtime zbus roundtrip deferred until a Linux container or post-K1 LeandrOS.
- S6: brush runs start-cosmic's bashisms.
  **DONE 2026-07-21 — verdict: YES, no gaps.** brush (0.4.0, local-patched tree,
  built as-is) parses (`-n`) and executes `cosmic-session/data/start-cosmic` (115
  lines) end-to-end, including the self-re-exec login-shell path (`exec -l`, :27),
  `mapfile -t < <(process-sub)` (:87), arrays, indirect `${!name:-}` (:90),
  `printf '%q'` (:99), brace expansion with bash's literal-on-no-match quirk (:45).
  Execution proceeds to :111 and fails only on missing `dbus-run-session` — which
  reveals a D4 scope addition: **we must ship a `dbus-run-session` equivalent**
  (busd does not provide one; small wrapper or the reference dbus tool). The
  systemctl block (:79-107) degrades gracefully when systemctl is absent.
- S7: cosmic-panel feature audit — can `use_system_lib`/wayland-egl be dropped by
  flags alone? (Informs whether panel needs client EGL at M6 or can go tiny-skia.)
  **DONE 2026-07-21 — verdict: NO.** cosmic-panel is a nested Wayland compositor:
  tiny-skia renders only its iced chrome, which is uploaded as a GL texture and
  composited by a smithay `GlesRenderer`, presented solely via
  `WlEglSurface`+EGL `swap_buffers` (cosmic-panel `space/render.rs:140-161`,
  `space/panel_space.rs:1385-1438`). `cosmic-panel-bin/Cargo.toml` has NO
  `[features]` section; `use_system_lib`, `wayland-egl`, `wayland-backend/client_system`
  are unconditional (lines 14-20, 33, 71). → M6 firmly requires: libwayland-client +
  libwayland-egl ports, Mesa EGL with `EGL_EXT_platform_wayland` + GLES2 (swrast
  Wayland platform). Watch item: panel calls `bind_wl_display`
  (`EGL_WL_bind_wayland_display`, `xdg_shell_wrapper/shared_state.rs:141`) on its
  nested server — verify Mesa swrast advertises it or that the panel tolerates
  its absence (applets that post wl_shm import via ImportMem regardless).
- Exit: D1/D4 decided; K1 acceptance tests written.
  **M0 STATUS 2026-07-21: exit criteria met.** D1 confirmed (static musl proven
  end-to-end on both arches incl. multi-thread tokio; dynamic-path prerequisites
  precisely scoped — ET_DYN bias + relocs land in K3, S4's runtime portion is
  blocked on K3 by design). D4 confirmed (busd builds static both arches; scope
  adds: session.conf + dbus-run-session launcher). K1 acceptance test committed
  (scmtest, 558310b). Bonus outcomes: Mesa cross-build fully working (ports/mesa/),
  SMP user-mem-under-spinlock kernel deadlock found + fixed (82d0cc3).
  Remaining M0 tails: S4 runtime (after K3), S5 zbus roundtrip (Linux container
  or post-K1). Next: kernel wave K1 (SCM_RIGHTS, shared VMO, AF_UNIX/epoll
  scale), Mesa track continues with wayland-platform delta + aarch64 build.

**M1 — Wayland transport lights up** (K1 + K2)
- Exit: S2 passes; a pure-Rust Smithay headless server + wl_shm client exchange a
  registry, attach a buffer, and the server reads correct client pixels over our
  socket. Tokio multi-task stress idles at ~0% CPU (epoll actually blocks).

**M2 — Dynamic linking** (K3)
- Exit: dynamically-linked musl binary with dlopen runs on both arches.

**M3 — GL on screen, no Wayland** (K4 DRM + Mesa port)
- Exit: kmscube-class GLES2-over-GBM binary renders animated frames to
  /dev/dri/card0 via kms_swrast; screenshot-verified via run-leandros driver.

**M4 — Input + seat** (K4 evdev + libinput/libseat/libudev/libxkbcommon)
- Exit: Smithay's reference compositor (anvil, kms backend) starts via the
  shims, composites a wl_shm client, cursor follows the virtio-tablet, keyboard
  types. This is the full "beneath COSMIC" stack proven end-to-end.

**M5 — cosmic-comp**
- Build `--no-default-features`; XKB data + fonts installed.
- Exit: cosmic-comp runs on kms, renders its UI, accepts a wl_shm client;
  busd running; zbus client owns a name.

**M6 — COSMIC session**
- start-cosmic → cosmic-session → settings-daemon + panel + notifications (+ bg,
  launcher, app-library). Panel per S7 outcome (tiny-skia or client-EGL-swrast).
- Exit: **COSMIC desktop screenshot on both arches** — panel + wallpaper render,
  launcher opens, pointer/keyboard interact, a wl_shm app (cosmic-settings) opens.

**M7 — Hardening + perf (post-goal)**
- llvmpipe; atomic KMS/planes/properties; PRIME + linux-dmabuf (enables wgpu
  clients + panel EGL fast path); XWayland; cosmic-greeter/greetd; virgl on a
  Linux host.

Parallel tracks: the **Mesa track (S3→M3)** starts at M0 alongside the kernel
transport track — it gates nothing until M4 but is the schedule's long pole.

---

## 6. Risk register

| Risk | Impact | Mitigation / fallback |
|---|---|---|
| **Mesa port** (musl cross, dlopen coupling, DRM assumptions) | Gates M3+; no alternative exists (comp aborts without GLES2) | S3 RESOLVED far better than feared: full cross-build works on macOS/zig, zero musl source issues, dlopen surface shrank to libgbm→dri_gbm.so only (megadriver is DT_NEEDED in ≥24.1). Residual risk moves to runtime: ld-musl loader (K3) + our DRM node honoring Mesa's ioctls (K4) |
| **Shared-VMO mm change** (blocker 2) | Deepest kernel change; everything above wl_shm depends on it | Scope to tmpfs/memfd only; S2 acceptance test gates M1; reuse shared-anon machinery keyed on file object |
| **Dynamic linker bring-up** | Gates comp/panel/Mesa at runtime | Fallback: static + dlopen-self shim; decide at S4, don't drift |
| **cosmic-panel client EGL** (fatal-at-spawn component) | Could force early dmabuf work | S7 RESOLVED negative: panel is a nested compositor, client EGL unavoidable → Mesa swrast-over-wl_shm Wayland platform is the committed path; dmabuf only if that fails; check `EGL_WL_bind_wayland_display` on swrast |
| **busd/zbus behavioral gaps** | Session aborts without bus | Reference dbus-daemon port stands by |
| **epoll semantics under real tokio+calloop load** | Stalls/spins that look like hangs | M1 exit includes idle-CPU criterion; persistent serial reader during long tests (known QEMU gotcha) |
| **Both-arches divergence** (TLS, DRM, loader) | Doubles bring-up on every milestone | Both-arch exit criteria on every milestone, not just at the end; softpipe (portable C) chosen partly for this |
| **TCG softpipe speed** | Desktop may be seconds-per-frame slow | Accepted for correctness milestones; llvmpipe/virgl are M7 |

---

## 7. Out of scope (explicitly)

XWayland, PipeWire/audio, NetworkManager, UPower, accountsservice, greetd +
cosmic-greeter (boot to root session instead), cosmic-workspaces' wgpu path,
GPU-accelerated rendering (virgl/Venus), hotplug (udev monitor stubs), VT
switching, multi-seat. All degrade gracefully or are non-fatal per the survey.
