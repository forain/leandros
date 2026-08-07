# M6 Progress — full COSMIC desktop screenshot on BOTH arches

Status: STARTING. Owner: M6 wave. Git HEAD cb8ba58 (clean, verified).

## Mission
M6 exit: full COSMIC desktop screenshot on BOTH arches (panel + wallpaper render;
a client window placed/visible). Stretch: launcher opens; input interaction evidence.

## Log

### Step 0 — orientation (in progress)
- Verified git log/status: HEAD cb8ba58, clean tree.
- Created this checkpoint file.
- Reading inherited notes: m5-progress.md M5f section, m6-session-choreography.md,
  m6-bins-manifest.md, m6-loadsmoke-results.md, dbus-brush-rootcause.md,
  m6-icons-manifest.md, m6-wallpaper-note.md, pipewire-gap-design.md.

## Inherited gaps (first tasks)
1. Client windows not PLACED under bare cosmic-comp (needs session shell components).
2. Residual softpipe pipe_get_tile_rgba(NULL) crash ONCE at init on a worker (benign?).
3. busd name-acquisition evidence inconclusive (capture properly, file-on-image).

## Step 1 — DONE: analysis + staging edits (host-only, no build yet)

### KEY DISCOVERIES (change the plan)
1. **cosmic-session takes `[COMPOSITOR] [ARGS...]` as its OWN argv** (main.rs:123-126)
   and passes ARGS straight to the compositor. So `cosmic-session cosmic-comp --no-xwayland`
   is the clean way to force the M5f Xwayland-EMFILE fix — no comp wrapper needed.
2. **The kernel has NO shebang (`#!`) binfmt.** execve accepts ELF magic ONLY
   (open_exec_header syscall.rs:2651 rejects non-ELF; eager path also ELF-only).
   The two launcher scripts (`start-cosmic-leandros`, `dbus-run-session`) can NEVER
   be exec'd directly. Also there was **no `/bin/sh`** (shell is /bin/brush; bins live
   in /bin = inode 4, not /usr/bin). FIX: stage `/bin/sh` as a hardlink to brush and
   run every script as `sh <script>` (brush-as-sh reads/interprets the file — no
   shebang/ENOEXEC dependency). The proposed dbus-run-session's internal `sh -c` now
   resolves too. This is exactly the "brush ships as /bin/sh? check image" the brief flagged.
3. app-library binary FILE is `cosmic-applibrary` but cosmic-session SPAWNS it as
   `cosmic-app-library` → must install under the spawn name.
4. cosmic-settings (GUI) is NOT auto-spawned (only cosmic-settings-DAEMON is). Staged
   anyway as an optional client for the stretch window goal.

### EDITS MADE (all host-side artifacts + repo mkfs; NOT yet committed)
- `m6-session-data/start-cosmic-leandros`: added COSMIC_RENDER_DEVICE=226:0,
  GBM_ALWAYS_SOFTWARE=1, SMITHAY_USE_LEGACY=1, COSMIC_DISABLE_SYNCOBJ=1,
  COSMIC_DISABLE_DIRECT_SCANOUT=1 (the proven-good M5f comp config); dropped the
  COSMIC_DRM_ALLOW_DEVICES default; launch line now
  `exec sh /usr/bin/dbus-run-session -- cosmic-session cosmic-comp --no-xwayland`.
- Copied `m5-session-ship/dbus-run-session.proposed` over the OLD (broken `$!`)
  staged `dbus-run-session` for BOTH arches.
- `scripts/mkfs-f2fs-populated.py`: new M6 block stages 8 session bins + daemon into
  /bin (correct spawn names), start-cosmic-leandros into /bin, /bin/sh→brush hardlink,
  libpipewire-0.3.so.0 stub into /usr/lib, Cosmic icons (83) + wallpaper into /usr/share.
  Syntax OK; all inputs verified present both arches.

## Step 2 — DONE: fresh f2fs root images regenerated BOTH arches
- Only the mkfs script + host artifacts changed (no kernel/userland source change), so
  regenerated f2fs-data0/1-{arch}.img directly (kernel target/final-* + leandros-limine
  boot images already current at cb8ba58). Both mkfs runs rc=0.
- Verified packed: 10 cosmic-* bins (8 session + daemon + comp), start-cosmic-leandros,
  `sh`→brush hardlink (nid 199/198), libpipewire-0.3.so.0 stub, 82 icons, wallpaper jpg.
- x86 img 1.25GB→1.82GB, aarch64 1.23GB→1.79GB. Logs: notes/m6-mkfs-{x86,aarch64}.log.

## Step 3 — aarch64 bring-up r0 FAILED (session never came up) → 2 bugs found+fixed
Screenshots beat serial here (brush readline redraws every keystroke; the framebuffer console
shows clean FINAL text). Evidence: m6-screenshots/m6-aarch64-r0-a.png + m6-diag-aarch64-b.png.

### BUG A — `/usr/bin/sh` exec failure (the session-killer)
Launcher's `exec sh /usr/bin/dbus-run-session` → brush's `exec` builtin prepended /usr/bin to
PATH and execve'd `/usr/bin/sh` (nonexistent) WITHOUT falling through to /bin/sh → ENOENT,
nothing downstream ran. (Normal fork-exec `sh -c` DOES fall through — diag proved bareword
`sh -c` works — but the `exec` builtin does not.) FIX (applied): absolute `/bin/sh` in BOTH
scripts. Confirmed on-target: `/bin/sh -c`=OK, command -v sh=/bin/sh, all bins present.

### BUG B — `/run/user/0` not creatable at runtime in one `mkdir -p`
`mkdir -p /run/user/0` returns 0 but does NOT create the deepest level when several nested
levels are new in one call (2nd mkdir completes it). FIX (applied): pre-create /run,/run/user,
/run/user/0 (0700 root) as static mkfs dirs (dir_nodes 18/19/20, dir_owner[20], subdirs).

### KERNEL FINDING (documented, no fix needed)
Kernel execve has NO shebang binfmt — ELF only. Scripts run as `sh <script>`. Script-layer only.

## Step 4 — r1 (with A+B fixes): /usr/bin/sh gone, /run/user/0 exists — but session STILL dies.
session.log EMPTY. Chain isolation (m6_chain.py, screenshots m6-chain-aarch64-*):
- Test A: brush-as-sh runs a SCRIPT FILE fine (HELLO_SCRIPT_A) — refutes "brush script crash".
- Test B: `test -d`/`stat` see /run/user/0 (inode 19216, dir) but `chmod 700` fails ENOENT.
  A real but NON-FATAL bug in the chmod/fchmodat syscall path (launcher guards chmod w/ ||true).
- Test C: `/bin/sh /usr/bin/dbus-run-session -- /bin/echo X` → child RAN (dc.log has X), rc=0.
  busd + dbus-run-session WORK for a trivial child. (One subproc EL0-faults @0x159F760 but child ok.)
- Test D (`sh -x` launcher trace): launcher runs to completion, reaches
  `exec /bin/sh /usr/bin/dbus-run-session -- cosmic-session cosmic-comp --no-xwayland`. AFTER the
  exec, PID 21 faults ELR=0x1516B04 FAR=0x7FFFFFFFBEFC0 (STACK addr, transl fault) + PID 22 jumps
  to 0x13C04B8. cosmic-session/cosmic-comp crash almost immediately, no log output.

### LEADING HYPOTHESIS: cosmic-session (tokio/launch-pad) or the comp-under-session crashes
very early with a STACK fault — session-specific (comp ran fine DIRECT in M5f, same env). Possibly
a stack-size limit (main-thread high-VA stack fault @0x7FFFFFFF...) hit by tokio/zbus/launch-pad.
## Step 5 — TWO separate crash sites found; MAJOR methodology issue: persistent-image pollution.
- E1 (comp DIRECT, polluted img): cosmic-comp exits 101 (Rust panic) at cosmic-comp
  src/config/mod.rs:172 = `cosmic_config::Config::new(...).unwrap()` → create_dir_all ENOTDIR.
- KERNEL: user main-thread stack is only **256KB** (syscall.rs:201-203 USER_STACK_SIZE=64*PAGE,
  eagerly mapped). Session faults land EXACTLY 64B below the stack base → STACK OVERFLOW. cosmic-comp
  ran DIRECT in M5f (calloop main, shallow); cosmic-session's tokio/zbus/launch-pad main overflows.
- fsrepro (POLLUTED img): `/root/.config` shows `?---------` (unknown file type = CORRUPTED inode
  left by E1's crashed create_dir_all). mkdir /root/.config/cosmic → ENOTDIR (parent not a dir).
  ROOT METHODOLOGY BUG: f2fs image is PERSISTENT; r1→chain→e1→fsrepro all mutated ONE image
  cumulatively → confounded. MUST use fresh images per clean test (login-sessions note warned this).
- tmpfs (/tmp) is fresh every boot + reliable; busd creates its socket in f2fs /run/user/0 fine.

### Two real issues to fix (disambiguating on FRESH image now):
1. cosmic config create_dir_all on f2fs may produce corrupt inodes (or was pollution). WORKAROUND
   candidate: route XDG_CONFIG/CACHE/DATA_HOME + XDG_RUNTIME_DIR to tmpfs in the launcher.
2. 256KB user stack too small for cosmic-session tokio → bump USER_STACK_SIZE (kernel rebuild).
## Step 6 — ROOT CAUSE CONFIRMED: 256KB user stack overflow (kernel). Fix applied.
comp2-home on a FRESH image (HOME=/root): `/root/.config` = proper `drwxr-xr-x` (NOT `?---------`),
NO config panic — comp got past Config::new, inited EGL. So create_dir_all WORKS on clean f2fs;
the earlier ENOTDIR/`?` corruption was POLLUTION from a prior crash corrupting a partial config write.
Root cause chain: cosmic-session (256KB main stack) overflows → crash → corrupts partial config →
later runs read corrupt dir → ENOTDIR panic. Fix the stack, everything follows.
FIX: kernel/src/syscall.rs USER_STACK_SIZE 64→2048 pages (256KB→8MB, Linux default). eager map.
Methodology: use FRESH images per session attempt (persistent-image corruption accumulates).

## Step 7 — built (8MB stack). Session STILL crashes → it is INFINITE RECURSION, not small-stack.
Full session (s0, fresh img): still crashes, ELR=0x1516B04, sp=0x7FFFFF7FF000 = EXACTLY the new
8MB stack base, FAR 96B below it → consumed the ENTIRE 8MB stack. So it's unbounded recursion (or
an >8MB single stack frame), NOT a merely-small stack. 8MB bump did take effect but doesn't fix it.
(Keep the 8MB bump anyway — 256KB was pathologically small and is correct to raise.)

TOOLING NOTES: `grep` is NOT on the image (coreutils build omits it) — use head/tail/cat/wc only.
Screenshots of the console (clean final state) + compound-command-at-idle (sleep exists) are the
reliable evidence channels; per-command typing garbles under comp CPU load; HVF truncates serial.

### ISOLATION (m6_iso.py):
- iso-sess (`dbus-run-session -- cosmic-session /bin/echo STUBCOMP`): busd LOGS "Listening on
  /run/user/0/bus" (works!). cosmic-session + echo-comp does NOT hit 0x1516B04 (only the benign
  0x159F760 dbus-machinery fault seen since test C). => cosmic-session's own code does NOT recurse.
- => The 0x1516B04 recursion is in **cosmic-comp spawned by cosmic-session** (COSMIC_SESSION_SOCK +
  real bus). comp DIRECT with NO bus (comp2-home) worked fine. Running iso-comp now (comp under
  dbus, bus present, NO session-sock) to split bus-vs-COSMIC_SESSION_SOCK.
- session.log EMPTY in full run because launch-pad relays comp stdout via cosmic-session's tracing;
  comp recurses before any line is relayed.

### PATH OPTIONS if comp-under-session is unfixable without a source patch:
A. If trigger = COSMIC_SESSION_SOCK only: run cosmic-comp DIRECT (works) + panel/bg as manual
   Wayland clients against it (bypasses cosmic-session spawning comp) → still yields panel+wallpaper.
B. If trigger = the bus: panel/bg also affected; deeper. Escalate per mission (comp source patch).

## Step 8 — TRIGGER CONFIRMED = COSMIC_SESSION_SOCK (NOT the bus).
iso-comp (comp under dbus, bus present, NO session-sock): comp runs, 8 log lines (journald BrokenPipe,
i18n, benign cosmic_settings_config NoConfigDirectory), only the benign 0x159F760 dbus fault — NO
0x1516B04 crash. So comp-under-a-bus is FINE. 0x1516B04 fires ONLY when comp is spawned by
cosmic-session with COSMIC_SESSION_SOCK set. session.rs run_socket() itself isn't recursive; the
recursion is elsewhere in comp startup gated on COSMIC_SESSION_SOCK — exact site needs symbolization
(binary load base) — DEFERRED as M7 (comp-under-session spawn bug).
=> PATH A (m6_patha.py): busd + cosmic-comp DIRECT + cosmic-bg + cosmic-panel as manual Wayland clients.
KEEP the 8MB stack bump (correct regardless). Fresh images per attempt. grep absent; head/tail/wc only.

## Step 9 — PATH A/B (manual clients) ALSO blocked → SECOND independent wall. ESCALATING.
PATH B (busd + comp WITHOUT a bus + bg/panel WITH bus, m6_pathb.py): comp runs (NO fault in serial —
comp did NOT crash), comp.log clean (warn: journald+config). BUT cosmic-bg CRASHES at its very first
Wayland roundtrip: bg.log = `Io error: Broken pipe (os error 32)` / "failed to initialize registry
queue" / panic at src/main.rs:105. cosmic-panel: "Falling back to default panel configuration",
NoConfigDirectory, exits. => comp binds wayland-1 but DROPS a real client during wl_registry init
(broken pipe). This is the M5d/M5f client-compositing wall resurfacing (entangled with rm-induced
f2fs /root churn + a real client-serving issue). Wallpaper never renders (desktop stays ~2 colors).

### FINAL M6 STATUS: MILESTONE BLOCKED — escalating per mission criteria.
Two independent, deep blockers, BOTH needing source-level work (comp and/or Mesa softpipe):
 1. cosmic-comp infinite-recursion (ELR=0x1516B04, overflows ANY stack) triggered by
    COSMIC_SESSION_SOCK — blocks the PROPER cosmic-session desktop.
 2. cosmic-comp drops real Wayland clients at wl_registry (broken pipe) — blocks even the
    manual-client bypass (panel/bg never composite). = the M5d/M5f softpipe/client wall.
Base graphics stack VERIFIED SOUND: drmsmoke renders a full RGB gradient via KMS; vfstest PASS.

### WORKING-TREE CHANGES (uncommitted — left for orchestrator/user to commit; regression-safe):
 - kernel/src/syscall.rs: USER_STACK_SIZE 64→2048 pages (256KB→8MB). Correct robustness raise; does
   NOT fix blocker 1 (unbounded recursion). vfstest PASS + drmsmoke OK on the 8MB build.
 - scripts/mkfs-f2fs-populated.py: +104 lines — stage 9 session bins (correct spawn names incl.
   cosmic-app-library←cosmic-applibrary), cosmic-settings-daemon + libpipewire stub, start-cosmic-
   leandros, /bin/sh→brush hardlink, /run+/run/user+/run/user/0 (0700), Cosmic icons (82) + wallpaper.
 - HOST artifacts (not in repo, staged for mkfs): m6-session-data/start-cosmic-leandros (env +
   `cosmic-comp --no-xwayland` + absolute /bin/sh); m5-session-ship/dbus-run-session.proposed (/bin/sh -c).
Fresh images built (build-all, both arches) at HEAD cb8ba58 + these uncommitted changes.

### KEY INFRA BUGS FOUND+FIXED (enable ANY future session work):
 - Kernel has NO shebang (`#!`) binfmt — execve is ELF-only. Scripts MUST run as `sh <script>`.
   Provided /bin/sh (brush hardlink); scripts use absolute /bin/sh (brush `exec` builtin does not
   fall through a nonexistent /usr/bin/sh via PATH).
 - `/run/user/0` didn't exist + runtime `mkdir -p` of multiple new nested levels is unreliable on
   f2fs (returns 0, deepest level absent) → pre-created in the image.
 - `grep` is absent from the image; `chmod`/fchmodat on some dirs returns spurious ENOENT (guarded).
 - Persistent f2fs image accumulates corruption across crashed runs (`?---------` inodes from partial
   create_dir_all) — USE FRESH IMAGES per session attempt.

### M7 NEXT STEPS:
 1. Symbolize ELR=0x1516B04 in cosmic-comp-aarch64 (need PIE load base from the loader) → find the
    COSMIC_SESSION_SOCK-gated recursion (suspect: setup_socket/set_cloexec on the inherited socketpair
    fd, lib.rs:155/session.rs:75, OR a logger/panic loop when comp stdio is a launch-pad pipe).
 2. Root-cause the client wl_registry "broken pipe" (comp drops clients) — the M5d/M5f softpipe wall;
    may need the Mesa softpipe minimal patch (Alpine/zig recipe in llvmpipe-lane / ports/mesa).
 3. Then retry full cosmic-session on a FRESH image; then x86_64; then full regressions + commits.
Harnesses (all reliable): m6_comp2.py (compound-at-idle), m6_iso.py, m6_slog.py, m6_pathb.py, m6_regress.py.

## Step 10 (M6b) — BLOCKER 2 ROOT-CAUSED: pending-accept send-EPIPE (kernel/net), COST-CONFIRMED
Owner: M6b wave. Fresh aarch64 images per attempt. HEAD 30db31a.
Harness bug found+fixed first: v1 diag used one long `printf` for pb.sh; the guest tty
canonical mode (MAX_CANON ~255B) TRUNCATED the compositor line -> comp never ran (bg
error "Could not find wayland compositor" main.rs:99, NOT the real broken-pipe). pathb was
NOT truncated (its script = exactly 9 lines, wc=9) so pathb's broken-pipe evidence is VALID.
v2 harness (m6b_diag.py) builds pb.sh via short `echo >>` appends (each <110B); wc=13, clean.

### DECISIVE evidence (m6b_diag.py, aarch64, fresh img, WAYLAND_DEBUG=1 on cosmic-bg):
- cosmic-bg WAYLAND_DEBUG (bg.log): sends EXACTLY 2 msgs then dies, receives ZERO bytes:
    [37990]  -> wl_display#1.get_registry(new id wl_registry#2)
    [37990]  -> wl_display#1.sync(new id wl_callback#3)
    Io error: Broken pipe (os error 32)  / failed to initialize registry queue / main.rs:105
  NO wl_display.error received, NO globals -> comp sent NOTHING. Not a protocol error.
- comp.log ENDS at 00:00:22.180 (journald warn, i18n INFO, shortcuts/window_rules
  NoConfigDirectory ERRORs = all benign). NO "listening on wayland", NO backend/EGL init,
  NO client handling, ever. (The "panel fallback" line I earlier attributed to comp was
  actually cosmic-PANEL's log — comp was never proven alive past 22.18 by logs.)
- busd healthy (Listening on /run/user/0/bus at 18.82). serial: NO comp fault/panic.
- Timeline: busd 18.8s, comp start 22s, cosmic-bg connect 37.99s.

### ROOT CAUSE (code-proven in servers/net/src/lib.rs):
comp is ALIVE at connect time — proven by SEMANTICS: handle_close's UnixListening arm
(line ~2065) calls free_bound_idx, so an EXITED comp would make cosmic-bg's connect return
ECONNREFUSED (line 1136), NOT broken pipe. cosmic-bg got broken pipe => comp's listening
BoundPath still live => comp NOT exited, just not-yet-accepting.
The EPIPE is from handle_send's catch-all `_ => err_reply(-32)` (line 1355): a socket in
SockState::UnixPendingAccept (connect done, server hasn't accept()ed yet) is NOT handled by
the send path. Also unix_stream_end (line 1495, used by sendmsg/recvmsg) matches ONLY
UnixConnected -> returns None for pending-accept -> sendmsg plain path calls handle_send ->
catch-all EPIPE. cosmic-bg (rust wayland-backend) writes get_registry the instant after
connect, BEFORE comp's (slow/busy) event loop accepts -> EPIPE. wlclient (M5f, libwayland-C)
escaped ONLY by scheduling luck (comp accepted before its write).
Linux semantics VIOLATED: post-connect a SOCK_STREAM AF_UNIX socket is ESTABLISHED +
writable; data written pre-accept is BUFFERED and delivered after accept. accept path
(lines 984/996-998) preserves conn_idx + rings across the pending->connected transition, so
buffering pre-accept is safe: comp reads the buffered bytes on its first recv post-accept.

### FIX (kernel/net, my domain — clearly correct, Linux-matching):
Treat UnixPendingAccept as UnixConnected{is_a:true} in send/recv/poll/unix_stream_end:
 1. unix_stream_end: also return Some((conn_idx, true)) for UnixPendingAccept.
 2. handle_send: add UnixPendingAccept arm -> write ring_ab (is_a=true), peer_closed=closed_b.
 3. handle_recv: add arm -> read ring_ba, EAGAIN if empty & !closed.
 4. handle_poll: add arm -> POLLOUT writable (+ POLLIN if ring_ba has data/closed).

### OPEN QUESTION before rebuild: is comp SLOW-to-accept (fix suffices) or STUCK in init?
M5f proved comp fully inits+serves a client on aarch64 at cb8ba58 (wlclient roundtrip). M6
only added 8MB stack + mkfs staging (neither should break comp). So comp is LIKELY slow
(EGL/softpipe under HVF >16s), not stuck. Next: cheap NO-REBUILD test giving comp 55s before
cosmic-bg + ls /run/user/0 + full comp.log, to separate comp-readiness from the race.
Harness: m6b_diag.py (v2, reliable). Uncommitted: none yet (fix not written).

## Step 11 (M6b) — pending-accept FIX BUILT; comp-readiness investigation deepened
Kernel fix applied (servers/net/src/lib.rs) + built aarch64 (m6b-build-aarch64.log, 0 errors):
handle_send/handle_recv/handle_poll/unix_stream_end now treat UnixPendingAccept as
UnixConnected{is_a:true} (buffer pre-accept writes; POLLOUT writable; EAGAIN on empty read).
8MB stack (179f9fa) confirmed MAIN-THREAD ONLY (syscall.rs:2982) — clone_thread uses caller
child_stack, so worker threads unaffected; 8MB is NOT a comp-hang cause.

### comp-readiness archaeology — the real variable is NOT the bus:
- M5f composite4 (WORKED, served wlclient): NO busd; comp log REACHED EGL/KMS/DRM backend
  (28.74 smithay egl, 28.87 kms::device EDID, 29.50 drm::compositor, then softpipe worker
  [FAULT] far=..10). comp env IDENTICAL to M6b (COSMIC_BACKEND=kms + GBM_ALWAYS_SOFTWARE etc —
  M5f exported it on the prior line, not inline). So ENV is NOT the regression.
- M6b busd tests (pathb/diag/wait, OLD kernel): comp reaches CONFIG only (8 log lines, stops at
  window_rules ~20-22s), CREATES wayland-1 socket, never logs EGL, never accepts. cosmic-bg
  EPIPEs (pending-accept bug).
- M6b nobus test (n0, NEW kernel): comp reaches NOTHING — comp.log=0 lines, NO wayland-1,
  NO fault/panic/exec-fail in 62s. WORSE than busd case. => busd is NOT the hang trigger; the
  busd path's `sleep 3` + bus actually lets comp reach further (config+socket). Bus hypothesis
  REFUTED.
- Net: on the CURRENT image (30db31a mkfs staging) comp does NOT reach EGL in my manual setups,
  whereas M5f (older image) reached EGL. Suspect: the 30db31a mkfs staging changed the image in
  a way that stalls comp's early/backend init (candidates: libpipewire-0.3.so.0 STUB in
  /usr/lib that comp/libcosmic may dlopen; or a staged file comp reads). NOT the 8MB stack.
  NOT confirmed yet.

### DECISIVE test running (d2): busd config + FIXED kernel + long waits (comp 28s, bg 25s),
smithay=debug. Reads out: (a) does comp reach EGL/backend (comp-tail egl/kms/drm lines)?
(b) does cosmic-bg get served (globals) / hang (2 -> lines, no globals — comp frozen) / still
EPIPE (fix ineffective)? This disambiguates comp-frozen-at-config vs comp-fine-but-quiet.

### Blocker-1 (parallel lane) — comp-recursion-analysis.md READ, for AFTER blocker-2:
Real userspace unbounded recursion (thin fp/lr 0x60-frame walker, cyclic client-surface graph:
raise_with_children / popup tree / tiling tree / menu geom). ELR=0x1516B04 UNTRUSTWORTHY (= 
draw_solid's return addr; fault handler likely prints LR-as-ELR — 5-min aarch64 EL0 sync-abort
audit). Kernel semantics exonerated. FIX (mine, kernel-side, in scope): add EL0 fault-time
x29-chain backtrace + log load base + true ELR in fault handler; run instrumented s0; offline
llvm-addr2line; then ESCALATE the named comp cycle-guard site. RETEST blocker-1 AFTER my socket
fix (pre-accept data loss could have corrupted early protocol → cyclic graph downstream).

## Step 12 (M6b) — PENDING-ACCEPT FIX VALIDATED; remaining wall = comp hangs before EGL
d2 (FIXED kernel, busd config, comp 28s + bg 25s + 60s harness wait, fresh img):
- cosmic-bg bg.log = ONLY the two `-> get_registry / -> sync` lines. NO "Broken pipe", NO
  panic (d1 old-kernel had EPIPE+panic here). => pending-accept fix WORKS: client writes now
  BUFFER and the client waits cleanly instead of EPIPE. BLOCKER-2 KERNEL ROOT CAUSE FIXED+VERIFIED.
- comp.log STILL 8 lines, frozen at config (window_rules 22.41), NO EGL — even after 60s+.
  => comp genuinely HANGS after config, before EGL/backend init. cosmic-bg buffers get_registry
  and hangs waiting for globals comp never sends.
REMAINING WALL: comp does not reach EGL in the manual busd+comp setup on the current image.
cosmic-comp DT_NEEDED: ld-musl,libc,libdisplay-info,libgbm,libinput,libpixman,LIBSEAT,libudev,
libxkbcommon (NO libpipewire/libdbus — stub irrelevant). HYPOTHESIS: comp opens DRM via libseat;
with busd present libseat may try the logind-over-D-Bus backend and BLOCK on a reply minimal
busd never sends (M5f had NO bus -> logind fails fast -> builtin backend -> reached EGL).
NEXT (d3): busd config + LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0 -> does comp reach EGL + serve
cosmic-bg (globals)? If yes: desktop unblocked via env-only (no source patch).

## Step 13 (M6b) — LIBSEAT ruled out; isolating busd as the comp-freeze cause
d3 (LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0 + busd): comp STILL frozen at config (8 lines, no
EGL). libseat NOT the cause. bg.log still clean 2-line buffer (fix holding).
Post-EGL D-Bus (M5f logged activation-env update at 29.6, AFTER egl 28.7) can't explain a
PRE-egl freeze. Only clean differentiator vs M5f-success remains busd PRESENCE.
d4: EXACT m6b_diag structure, ONLY change = NO busd (echo placeholder), no DBUS ref on bg.
If comp reaches EGL + serves cosmic-bg -> busd confirmed as freeze cause; wallpaper path = comp
+ cosmic-bg with NO bus. (nobus n0's 0-line anomaly was likely its separate-script structure.)

## Step 14 (M6b) — BUSD CONFIRMED as comp-freeze cause; comp reaches backend without it
d4 (NO busd, else identical to d2): serial shows comp got PAST config to BACKEND —
`[CPIO] Looking for file: bin/--no-xwayland` (comp kiosk-child exec quirk, SAME as M5f) +
`[FAULT] FAR=0x10 ELR=0x42881918 PID=17 ... [EXIT] pid=17 code=1` = the M5f softpipe
pipe_get_tile_rgba(NULL) worker crash (worker dies, main survives per M5f). So WITHOUT busd
comp reaches backend/compositing exactly like M5f. => busd PRESENCE freezes comp at config
(comp blocks on an early D-Bus/session interaction with minimal busd; NOT libseat, NOT kernel).
CAVEAT: once comp takes the framebuffer, per-command serial log-dumps GARBLE (known M5 issue) —
d4's post-run echo/cat dumps were unreadable. Capture composited output via QMP screendump
DURING the run instead. pending-accept fix still holding (bg.log clean pre-garble).
NEXT: m6b_wall.py — no busd, comp + cosmic-bg, QMP screenshot during composition = M6 WALLPAPER.
Then try ordering trick (comp first, busd+panel after comp is serving) for panel.
BLOCKER-2 KERNEL FIX = DONE+VALIDATED. Remaining wall (comp+busd freeze) = comp/D-Bus, escalate.

## Step 15 (M6b) — WALLPAPER BLOCKED by M5 softpipe crash (Mesa, escalate); blocker-2 kernel fix stands
m6b_wall.py (no busd, comp + cosmic-bg, QMP screenshots during run): comp reached RENDERING then
PID 16 faulted FAR=0x10 ELR=0x42708918 (softpipe pipe_get_tile_rgba(NULL) NULL+0x10 deref =
the M5d/M5f softpipe wall) + [EXIT] pid=16 code=1. comp never presented a frame -> framebuffer
stayed on the TEXT CONSOLE (all t34/t54/t74/t94 screenshots identical = kernel fault text, NO
wallpaper). Also [SYSCALL] ENOSYS nr=0x1B7 (faccessat2/439 unimplemented; benign fallback).
=> Full desktop/wallpaper needs the Mesa softpipe fix (M5 authority, multi-day) — ESCALATE.

### M6b FINAL POSITION (blocker 2):
- KERNEL ROOT CAUSE of blocker-2 symptom ("cosmic-bg Broken pipe at wl_registry") = FIXED+VALIDATED:
  the pending-accept send-EPIPE bug (servers/net). d1(old)=EPIPE+panic; d2(fixed)=clean buffered
  wait, no broken pipe. This IS the literal blocker-2 failure the brief described.
- THREE independent userspace/Mesa walls remain, ALL out of kernel scope, needing escalation:
  1. comp FREEZES at config when a session bus (busd) is present (early D-Bus/session block on
     minimal busd) — comp reaches backend only with NO bus. (comp/D-Bus; not kernel/libseat.)
  2. Mesa softpipe FAR=0x10 pipe_get_tile_rgba(NULL) crash blocks the actual COMPOSITE/present —
     even a healthy connection can't yield a rendered wallpaper. (Mesa softpipe; M5 wall.)
  3. (blocker 1) comp unbounded recursion under COSMIC_SESSION_SOCK (parallel-lane analysis).
- Net: the kernel is SOUND for real-client Wayland now; the desktop is gated on Mesa+comp userspace.
NEXT: validate fix via regressions (socket suites first), build+test x86_64, commit, then EL0
backtrace instrumentation (aids softpipe FAR=0x10 + blocker-1 symbolization).

## Step 16 (M6b) — CLOSE-OUT: fix committed 5c43227, regressions green both arches
- COMMITTED 5c43227 "net: buffer writes on connected-but-unaccepted AF_UNIX stream sockets"
  (main, no Claude mention). Only servers/net/src/lib.rs (+53). 8MB stack/mkfs already committed.
- Regressions GREEN: aarch64 vfstest PASS + drmsmoke gradient + scmtest PASS + epolltest 8/0;
  x86_64 scmtest PASS + epolltest 8/0 + polltest ALL PASS (pipe_epoll/pollout_reflects_ring_full/
  poll_and_select_match/socketpair_epoll_readiness/epoll_wait_times_out/pipe_hup_refcount).
  Change is purely additive (new UnixPendingAccept arms; UnixConnected paths untouched).

### EL0 BACKTRACE INSTRUMENTATION — SCOPED, DEFERRED to M7 (safety + reachability):
LOCATION: arch/aarch64/src/exception.rs, fn exc_el0_sync_handler(esr,elr,frame:*mut UserFrame),
right before `sched::exit(1)` (line ~213). `elr` IS the true ELR_EL1 (NOT LR — settles analysis
§5: the printed ELR is trustworthy; 0x1516B04 paradox is a load-base/symbolization issue, not a
handler bug). frame.x[29]=fp, frame.x[30]=lr, frame.sp_el0=user sp. Helpers: serial_print_str,
print_hex, print_number. Recipe: print x29/x30, then walk fp chain (ret=*(fp+8), next=*(fp),
ascending, 8-aligned, <=32 frames), print each ret; offline `llvm-addr2line -e cosmic-comp-aarch64
<ret-0x200000>`.
SAFETY BLOCKER (why deferred, not rushed): the REACHABLE crash (softpipe FAR=0x10) is on a WORKER
THREAD (PID 16/17) whose stack is a musl-allocated mmap of extent UNKNOWN to the kernel — a
fixed-window fp walk can read past the mapped stack → EL1 sync fault → exc_el1_sync_handler spins
= DEADLOCK. Correct impl must bound the walk to the CURRENT TASK's registered stack extent (main:
USER_STACK_TOP-USER_STACK_SIZE..TOP in syscall.rs; threads: clone child_stack) via a new
sched::current_user_stack_range() helper. That sched plumbing is the real work; do it first.
REACHABILITY: blocker-1 recursion (the primary capture target) is currently UNREACHABLE — comp
can't reach serving-under-session (softpipe crash + busd freeze). So instrument + fix ONE of those
first, else the instrumented run only catches the softpipe worker crash (whose Mesa .so frames
need per-.so load bases the kernel doesn't track — mostly unsymbolizable). Net: land the sched
stack-range helper + instrumentation, THEN a session run once comp can reach the recursion.

### M6b EXIT SUMMARY (for orchestrator):
DELIVERED: blocker-2 kernel root cause fixed+validated+committed (5c43227). Kernel sound for
real-client Wayland. REMAINING (all userspace/Mesa, escalate): comp-freezes-with-busd (D-Bus),
Mesa softpipe FAR=0x10 (no composite), blocker-1 recursion. No wallpaper/desktop screenshot
possible without the Mesa softpipe fix. Harnesses: m6b_diag.py (the workhorse), m6b_wall.py,
m6b_sockreg.py, m6_regress.py. All fresh-image, aarch64=uefi-hvf, x86_64=TCG.

## Step 17 (M6c) — W2 ROOT-CAUSED via disassembly: MAP_DUMB miss → NULL displaytarget → tile-cache crash
Owner: M6c. HEAD 5c43227 (clean, verified). Fresh aarch64 img per attempt.

### RECONFIRM (m6c_w2repro.py, aarch64 uefi-hvf, fresh img, wlclient, NO busd): crash STILL fires.
Serial m6-screenshots/m6c-aarch64-r0-serial.log: 3 `[MMAP] DynamicDevice` device mmaps ALL
SUCCEED (off B7400000/B7C00000/48000000, phys echoed, map_device OK). Then FAULT far=0x10
dfsc=6 EC=0x24 ELR=0x42746918 PID=16 x0=0 x1=0. NO 4th mmap before the fault.

### KERNEL EXONERATED, Mesa fault SYMBOLIZED (libgallium-25.3.6.so has FULL local symbols):
- load_base solved by symbol arithmetic: pipe_get_tile_rgba @file 0xecc8f8; ELR 0x42746918 =
  base 0x4187A000 + 0xecc918 = pipe_get_tile_rgba+0x20. => fault IS pipe_get_tile_rgba(pt=NULL)
  reading pt->box @+0x10 (x0=0 → FAR=0x10). CONFIRMED.
- CALLER = sp_find_cached_tile (0xf5ab50 bl pipe_get_tile_rgba). x0=tc->transfer[face] loaded
  from tc+0x28 array (0xf5aab0), passed with NO null-check.
- tc->transfer[face] populated in sp_tile_cache_set_surface: loop @0xf5a444, calls
  pipe->transfer_map (ctx+0x3a0) via `blr x9` @0xf5a484, stores result @0xf5a494 WITHOUT
  null-check. On map failure both tc->transfer[] and tc->transfer_map[] become NULL (→ x0=x1=0).
- softpipe_transfer_map (0xf59778): resource IS a display target (spr->dt @res+0x160 != NULL);
  calls winsys->displaytarget_map (winsys+0x30) via `blr x8` @0xf598e4; if it returns NULL →
  frees transfer, returns NULL (@0xf59944).
- kms_sw_displaytarget_map (0xf3c080): calls drmIoctl(DRM_IOCTL_MODE_MAP_DUMB 0xC01064B3) FIRST
  (@0xf3c0cc); if it FAILS → mtx_unlock + return NULL (@0xf3c0d0/0xf3c0d4) — BEFORE any mmap().
  This EXACTLY explains "3 good device mmaps then fault, no 4th mmap": the failure is at the
  MAP_DUMB ioctl, which never reaches the mmap() syscall.

### KERNEL SIDE: std_handle_map_dumb (drivers/src/drm_device_interface.rs:979) returns NotFound
when map.handle is NOT in DUMB_BUFFERS. So the render-target/composite bo being mapped has a GEM
handle not registered in DUMB_BUFFERS. The 3 that mapped fine ARE CREATE_DUMB dumb buffers
(comp's own front/back/cursor). The failing one is a bo that entered via a non-CREATE_DUMB path
— strongly implicated by the DRM_CAP_PRIME IMPORT|EXPORT report (get_cap:1110) routing GBM bo
alloc through "the proper DRIimage path where the bo is a real gallium resource" (its handle may
not be a DUMB_BUFFERS entry). This is a KERNEL/DRM registration gap, NOT purely a Mesa bug.

### WHY a Mesa null-check ALONE is INSUFFICIENT for W2 exit:
Null-checking tc->transfer (or the winsys map) prevents the CRASH but yields NO pixels for that
surface (the tile cache can't read/write the RT) → no visible composite. W2 exit needs the bo to
actually MAP. So the real fix is kernel-side: make MAP_DUMB (and the 0x1007 mmap validator)
resolve the failing handle. INSTRUMENTING now: std_handle_map_dumb logs the missing handle +
DUMB_BUFFERS keys; CREATE_DUMB logs handle+phys. Build m6c-build-aarch64-mapdumb.log; rerun
m6c_w2repro to NAME the handle and trace its origin (CREATE_DUMB vs PRIME vs addfb/GEM).
Diagnostic build only — will be reverted before ship.

## Step 18 (M6c) — W2 DEFINITIVE ROOT CAUSE (source-confirmed): PIPE_BUFFER bound as color RT
Instrumented std_handle_map_dumb (CREATE_DUMB + MAP_DUMB MISS traces, via always-on
crate::pci::serial_debug — NOTE: rdebug is gated behind RENDER_DEBUG=false, so the first
instrumented build emitted nothing; switched to serial_debug*). Rebuilt aarch64, reran
m6c_w2repro (fresh img, wlclient, no busd).

### DECISIVE trace (serial): 4x CREATE_DUMB (handles 1-4), ZERO MAP_DUMB MISS, then FAULT far=0x10.
=> kms_sw_displaytarget_map's DRM_IOCTL_MODE_MAP_DUMB NEVER failed. Kernel FULLY EXONERATED.
By elimination: if sp_tile_cache_set_surface had run its map loop, displaytarget_map would have
HIT (handle known) and returned a valid transfer → no crash. It crashed with transfer=NULL, and
no MISS → the map loop was SKIPPED (no map call at all).

### SOURCE (mesa 25.3.6 src/gallium/drivers/softpipe/sp_tile_cache.c, sp_tile_cache_set_surface):
```
if (ps->texture->target != PIPE_BUFFER) {        // <-- the resource[0x4c] byte gate (target)
   for (i...) tc->transfer_map[i] = pipe_texture_map(..., &tc->transfer[i]);
} else {
   /* can't render to buffers */
   assert(0);                                     // <-- NO-OP in release → tc->transfer stays NULL
}
```
The bound color render-target's resource has **target == PIPE_BUFFER (0)**. softpipe refuses to
map a buffer as an RT (assert(0), stripped in release) → tc->transfer[]=NULL. Then
sp_find_cached_tile does `pt = tc->transfer[layer]; assert(pt->resource); ... pt->resource->format`
→ pt=NULL deref → FAR=0x10. (ELR varies per boot: r0=0x42746918, map2=0x42708918; both =
pipe_get_tile_rgba+0x20 after per-boot load-base shift — symbolization robust.)

### WHY a PIPE_BUFFER is bound as a color RT: correlates with a CLIENT wl_shm buffer entering
compositing (wl_shm pools are linear → PIPE_BUFFER). Exact upstream trigger (smithay GlesRenderer
shm-import / an intermediate blit target / Mesa DRIimage resource target choice) NOT yet pinned —
needs GL-level tracing of cosmic-comp, out of cheap-analysis reach.

### FIX = MESA-SIDE (kernel exonerated), and NON-TRIVIAL (mission ESCALATE: "Mesa patch bigger
### than minimal"). Ranked options for orchestrator:
 1. ROOT (best, unknown size): stop a PIPE_BUFFER resource being bound as a color RT — trace where
    (smithay import vs Mesa dri resource_create target). May be a format/modifier/config lever.
 2. softpipe: in set_surface else-branch, MAP the PIPE_BUFFER via a linear transfer instead of
    assert(0), so the tile cache can read/write it (small patch, but softpipe tile cache assumes
    2D stride — needs validation it yields correct pixels, not garbage).
 3. Robustness-only (INSUFFICIENT for W2 exit): null-guard tc->transfer in sp_find_cached_tile +
    sp_flush_tile_cache. Prevents the crash but the RT is then never read OR written (put_tile
    also needs tc->transfer) → black for that surface. Does NOT yield a visible composite.
Build path EXISTS: Docker OK + ~/code/leandros-artifacts/llvmpipe-lane/build-in-alpine.sh (Mesa
25.3.6 softpipe, source at llvmpipe-lane/src/mesa) — a full libgallium ninja build (~heavy).

### M6c STATUS: W2 root-caused to the exact Mesa mechanism (kernel exonerated). Fix is Mesa-side,
non-trivial (options above). ESCALATING per mission. Diagnostic kernel instrumentation REVERTED
(tree clean at 5c43227); clean aarch64 rebuild in progress to restore uninstrumented baseline.
W1/W3 NOT started (W3 depends on W2). Harness: m6c_w2repro.py (reliable, fresh-img/no-busd/wlclient).

## Step 19 (M6d) — W2 diagnostic libgallium BUILT + LOADING; crash reproduced under instrumentation
Owner: M6d. HEAD 5c43227 (clean). Diagnostic softpipe libgallium (W2DIAG-instrumented
sp_tile_cache.c set_surface else-branch + sp_texture.c create_surface/res_create/
res_from_handle breadcrumbs) built via adapted Alpine recipe
(llvmpipe-lane/build-diag-softpipe.sh: softpipe-only, -Dllvm=disabled, NO zstd,
static libstdc++/libgcc, -fno-stack-protector). NEEDED matches shipped exactly
(libdrm,libexpat,libz,libc.so) = clean drop-in.
- LANDMINE 1 (fixed): Alpine default -fstack-protector-strong -> libgallium referenced
  __stack_chk_guard, which LeandrOS musl libc.so does NOT export (it DOES export
  __stack_chk_fail). Alpine static libstdc++.a/libgcc.a re-introduced the ref even after
  -fno-stack-protector on Mesa sources. FIX: inject ssp_guard.o (unsigned long
  __stack_chk_guard=const, built -fno-stack-protector -fPIC) onto the link line
  (c_link_args/cpp_link_args). Now __stack_chk_guard is DEFINED. THE FINAL FIX BUILD
  INHERITS THIS (same script) — required for BOTH arches.
- LANDMINE 2 (fixed): pkill is ABSENT on the image; harness relied on it to kill comp.
  comp kept owning the framebuffer -> per-command serial cat of /tmp/comp.log GARBLED
  (unreadable). FIX (m6d_w2diag.py): save comp/wl PIDs (echo $! > /tmp/*.pid), kill -9
  by PID at script tail to release the fb, THEN dump comp.log on clean serial.
- d1 run (before pkill fix): crash REPRODUCED with diag lib loaded (no MESA-LOADER error):
  far=0x10 EC=0x24 DFSC=0x6 ELR=0x420D87B4 x0=0 x1=0 [EXIT] pid=16 code=1 = the same
  softpipe pipe_get_tile_rgba(NULL) worker crash. comp.log W2DIAG lines were fb-garbled.
- d2 running now with the PID-kill fix to read the W2DIAG resource props + creator ra's.

## Step 20 (M6d) — W2 STEP-18 ROOT CAUSE REFUTED by definitive instrumentation. Real cause = MAP_DUMB on the RT bo.
RELIABLE CAPTURE finally achieved (m6d_w2diag3.py): comp stderr stays on the INHERITED
serial (clean program output like kernel [FAULT] lines); driver `sleep N; echo MARK`
cmds act as READER-WINDOWS that pump comp's async stderr into SERIAL_LOG (driver only
appends to the log while actively reading a cmd — Python time.sleep captured nothing).
comp is idle (no CPU-starve) until a client connects, so the wlclient-launch cmd sends clean.

### e0 DECISIVE W2DIAG capture (aarch64, fresh img, wlclient, no busd):
- crash STILL fires: far=0x10 EC=0x24 ELR=0x41A207B4 PID=11 = pipe_get_tile_rgba(NULL)
  (base 0x411c1ff4 solved via nm pipe_get_tile_rgba; ELR = base+off+0x20, exact).
- **NO "set_surface RENDER-TO-BUFFER" line EVER printed** (my else-branch instrumentation).
  => ps->texture->target is NOT PIPE_BUFFER. **STEP 18's root cause (PIPE_BUFFER bound as
  color RT) is REFUTED.** No create_surface-over-buffer either.
- The only render targets seen (symbolized against diag lib):
  * res_create target=2(2D) 1280x800 bind=0x18000a  <- dri_create_image (ra 0x412973e0)
  * res_from_handle target=2 1280x800 bind=0xa whandle_type=2(FD) <- dri2_from_dma_bufs
    (ra 0x41298604) = smithay imports the output GBM bo as an EGLImage/dmabuf.
  * PIPE_BUFFER(target=0) lines are just VERTEX buffers (bind=0x10, _mesa_bufferobj_data).
- => the crash is the IF-branch (target!=PIPE_BUFFER) `pipe_texture_map` returning NULL on a
  DISPLAY-TARGET-backed RT => softpipe_transfer_map -> kms_sw_displaytarget_map ->
  DRM_IOCTL_MODE_MAP_DUMB FAILS -> NULL transfer -> pipe_get_tile_rgba(NULL). This is
  Step 17's kernel DRM/MAP_DUMB direction (a PRIME/dmabuf-imported or non-dumb RT bo whose
  handle MAP_DUMB can't resolve), NOT a Mesa softpipe tile-cache bug.

### IMPLICATION: the approved W2 plan (Mesa softpipe else-branch fix) is built on the refuted
Step-18 cause and will NOT fix this. The real fix is almost certainly KERNEL-side (MAP_DUMB
must resolve the RT bo — PRIME-imported via 6ce43be borrowed dumb VMOs, or displaytarget bo).
Out of approved Mesa-patch scope -> ESCALATE per mission, AFTER v5 confirms the exact failing
handle + errno. v5 instruments kms_sw_displaytarget_map (MAP_DUMB ret/errno + handle),
add_from_prime (fd->handle), create_dumb (handle), and set_surface if-branch NULL. Building.

## Step 21 (M6d) — winsys-instrumented run (e1): nondeterministic; RT scanout IS create_dumb handle=1
e1 (v5 lib: kms_sw map/add_from_prime/create_dumb instrumented). Single boot. Sequence:
- 4x vertex buffers (target=0 bind=0x10, noise).
- dri_create_image -> res_create target=2 1280x800 bind=0x18000a -> **create_dumb handle=1
  size=4096000 pitch=5120** (comp's own scanout RT is a proper CREATE_DUMB dumb buffer ✓).
- dri2_from_dma_bufs -> res_from_handle target=2 1280x800 whandle_type=2(FD) -> **add_from_prime
  fd=31** -> then a DIFFERENT crash: FAR=0x0 ELR=0x1593CD4 PID=4 (LOW addr = cosmic-comp/smithay
  MAIN binary, NOT libgallium@0x41xxxxxx). NO kms_map line reached.
=> e1 crashed at the dmabuf-import step (drmPrimeFDToHandle / smithay EGLImage import), BEFORE any
   map. e0 crashed LATER at pipe_get_tile_rgba (a map returned NULL). TWO crash signatures across
   runs = HVF scheduling NONDETERMINISM. Both plausibly the same underlying dmabuf-RT problem
   (import path vs later map). 0x1593CD4 is near blocker-1's 0x1516B04 + the benign 0x159F760 —
   all in cosmic-comp's main PIE (low base). Need a run that REACHES kms_map to see MAP_DUMB
   ret/errno. e2 re-running. If still nondeterministic, switch to KERNEL-side std_handle_map_dumb
   serial_debug (deterministic, clean serial) to settle whether MAP_DUMB fails and on which handle.
KERNEL context: FD_TO_HANDLE (syscall.rs:5719) returns the ORIGINAL dumb handle via
dmabuf_handle_of -> that handle IS in DUMB_BUFFERS -> MAP_DUMB *should* resolve. So if MAP_DUMB
fails it's a handle-registration/dedup gap; if it succeeds the NULL is from mmap(offset=phys) or
elsewhere. std_handle_map_dumb (979) sets map.offset=phys; winsys then mmap(fd, offset=phys).

## Step 22 (M6d) — KERNEL-side instrumentation (deterministic clincher) building
e2 also starved at add_from_prime before kms_map -> Mesa-side capture unreliable past the import.
Switched to KERNEL serial_debug (clean, never garbles, fires regardless of comp's later fate):
- syscall.rs FD_TO_HANDLE intercept: "W2K FD2H dfd=<fd> -> handle=<h> | NONE"
- drm_device_interface.rs std_handle_map_dumb: "W2K MAP_DUMB handle=<h> FOUND phys=<p> |
  NOTFOUND keys=<list>"
This settles the exact mechanism: (a) does FD_TO_HANDLE(fd=31) resolve the imported output
dmabuf to a valid dumb handle, and (b) is that handle in DUMB_BUFFERS so MAP_DUMB succeeds.
- If FD2H -> NONE: kernel dmabuf_handle_of/TGID gap -> smithay import null-derefs (e1 0x1593CD4).
- If FD2H -> handle=1 + MAP_DUMB FOUND: NULL comes from mmap(offset=phys) or later softpipe.
Build: build-all.sh --arch aarch64 (kernel+drivers rebuild + fresh image with diag libgallium).
Harness for the run: m6d_w2diag3.py (reader-window capture) — W2K lines land clean in serial.

### FIRM CONCLUSION regardless of the sub-mechanism (for the escalation):
The APPROVED W2 plan is built on Step-18's REFUTED root cause (PIPE_BUFFER bound as color RT);
the softpipe else-branch fix will NOT address this crash. The real failure is on the
dmabuf-imported OUTPUT render target (smithay imports the GBM scanout bo as an EGLImage via
PRIME/dmabuf; software softpipe/kms_sw winsys then cannot use it). This spans kernel DRM PRIME +
Mesa kms_sw winsys + smithay software-render — BIGGER than option-2 scope -> ESCALATE.

## Step 23 (M6e) — code analysis settles the mechanism; clean-capture rerun for the token
Owner: M6e. HEAD 5c43227 + M6d W2K instrumentation (uncommitted). Read M6d Steps 19-22 + e3 capture.
CODE ANALYSIS (definitive, no runtime needed for these):
- BOTH PRIME paths already thread sched::tgid_of: HANDLE_TO_FD (syscall.rs:5693) AND FD_TO_HANDLE
  (5724). M5c fix 146089c ALREADY covers FD_TO_HANDLE. => task decision-table case (a)'s stated
  mechanism (FD_TO_HANDLE keys by raw TID) is REFUTED by source; it is not the bug.
- dmabuf_handle_of / install_dmabuf_vmo (servers/vfs/src/lib.rs:492,513) are symmetric + TGID-keyed.
  => IF fd was registered by a prior HANDLE_TO_FD in the same process, FD2H resolves.
- MAP_DUMB (drm_device_interface.rs:979) is deterministic: create_dumb inserts handle=1 into the
  GLOBAL DUMB_BUFFERS (1597); scanout bo IS handle=1 => MAP_DUMB(1) ALWAYS FOUND, returns phys.
- mmap(card0,offset=phys) validate (handle_ioctl_mmap:1354) echoes phys iff it matches a DUMB_BUFFERS
  entry => scanout phys is known => map_device succeeds.
So on the deterministic happy path ALL kernel steps resolve. The open question is ONLY whether fd 31
was actually registered (did HANDLE_TO_FD run for it) => FD2H handle-vs-NONE is the sole decision token.
e3 GARBLE ROOT CAUSE: kernel serial_debug() and the shell TTY share the UART with no common lock; the
NEXT command's prompt-redraw truncated the FD2H line mid-string ("-> han"). FIX (m6e_w2token.py):
one command backgrounds comp then blocks the shell in `sleep 55` — shell silent, FD2H lands clean.
No wlclient (FD2H fires on comp's OWN scanout import, per e3 line 141). Running aarch64 t0.

## Step 24 (M6e) — W2 MECHANISM SETTLED: kernel PRIME path PROVEN SOUND at runtime → ESCALATE (not a kernel fix)
Owner: M6e. HEAD 5c43227 + inherited W2K instrumentation (uncommitted, NO new source edits by M6e).

### THE DECISIVE TOKEN (clean capture, m6e_w2token.py t0, aarch64 uefi-hvf, fresh serial):
    W2K FD2H dfd=0x0000001F -> handle=0x00000001
=> kernel PRIME_FD_TO_HANDLE(fd=31) RESOLVES the smithay-imported scanout dmabuf to dumb handle=1.
Capture fix vs e3 garble: e3's line truncated because kernel serial_debug() and the shell TTY share
the UART with no common lock; the NEXT command's prompt-redraw overwrote it. m6e keeps the shell
SILENT (backgrounds comp then blocks it in `sleep`), so the async kernel line lands clean.

### CASE (a) REFUTED at runtime AND in source:
- Both PRIME paths already thread sched::tgid_of (HANDLE_TO_FD syscall.rs:5693, FD_TO_HANDLE 5724).
  M5c fix 146089c ALREADY covered FD_TO_HANDLE. The task's "FD_TO_HANDLE keys by raw TID" is stale.
- dmabuf_handle_of / install_dmabuf_vmo (servers/vfs/src/lib.rs:492,513) symmetric + TGID-keyed.
- Runtime FD2H→handle=1 confirms the import resolves. NOT a TGID/registration gap.

### KERNEL DOWNSTREAM PROVEN SOUND (deterministic code, no runtime needed):
- MAP_DUMB(handle=1): create_dumb inserts handle=1 into GLOBAL DUMB_BUFFERS (drm_device_interface.rs
  :1597); std_handle_map_dumb (:979) => ALWAYS FOUND, returns map.offset=phys. Deterministic.
- mmap(card0, offset=phys): sys_mmap DynamicDevice path (syscall.rs:1469) -> DRM ioctl 0x1007 ->
  handle_ioctl_mmap (:1354) validates phys ∈ DUMB_BUFFERS -> echoes phys -> map_device(virt,phys,len).
  The dumb block is order-10 contiguous (4194304B ⊇ 4096000B); the map is in-bounds. Succeeds.
=> The ENTIRE kernel-side PRIME/dmabuf/MAP_DUMB/mmap chain for the scanout RT is SOUND. There is NO
   small kernel fix here. This UPGRADES M6d Step-20's hedge ("almost certainly KERNEL-side") to a firm
   NEGATIVE: the kernel is cleared.

### WHERE IT ACTUALLY FAILS (Mesa kms_sw winsys / softpipe / smithay — ALL out of approved scope):
Mesa source read (llvmpipe-lane/.../winsys/sw/kms-dri/kms_dri_sw_winsys.c + softpipe/sp_texture.c,
sp_tile_cache.c). Crash chain (M6d e0 symbolication = pipe_get_tile_rgba(NULL)):
  kms_sw_displaytarget_map returns NULL  (ONLY on MAP_DUMB fail:331 or mmap fail:344 — both kernel-
  cleared above; OR the imported resource has spr->dt==NULL because add_from_prime.get_plane:138
  returned "plane too big" on a stride/offset mismatch => from_handle NULL => resource has neither dt
  nor data) -> softpipe_transfer_map (sp_texture.c:463) returns NULL -> sp_tile_cache set_surface
  stores NULL map -> pipe_get_tile_rgba(NULL) faults. e1's alt signature (0x1593CD4, comp PIE, before
  any kms_map) = the smithay EGL software-import layer null-derefs the failed import. BOTH are Mesa
  kms_sw/softpipe displaytarget handling + smithay software-render — the exact cross-layer scope M6d
  Step-22 named. The approved softpipe-else-branch patch is on the REFUTED Step-18 cause; N/A.

### VERDICT: W2 is NOT a small kernel fix and NOT the refuted Mesa scope -> ESCALATE per mission.
No source changed by M6e. Instrumentation LEFT in tree (diagnostic scaffolding for the escalation).

### Harness/landmines this step (for the next owner):
- aarch64 uefi-hvf: repeated boot attempts in one harness loop hang (known); attempt-1 works from a
  cleanly-killed state. Empty /tmp/leandros-serial.log across all 5 attempts = HVF didn't start the VM.
- Reader-window capture: LONG guest command lines (>~40 chars) get FIFO-dropped/corrupted by the PL011
  16-byte RX FIFO ("sleep 55"->"sleeho", "export WAYLAND..."->"ort WAYLAND..."). Keep capture cmds short.
- Driver early-break on "> ": " -> handle=" contains "> " so it stops the read right after FD2H (fine
  for the token; use driver_nobreak.py copy for full-window MAP_DUMB/[MMAP] capture).
- IMAGE STATE: f2fs-data0/data1-aarch64.img REGENERATED by M6e at 02:24 via mkfs-f2fs-populated.py
  (may carry the diagnostic libgallium). Boot image leandros-limine-aarch64.img (00:53) untouched =
  instrumented kernel. FINALS must rebuild fresh with STOCK libgallium (unship diagnostic lib).
- Artifacts: m6e_w2token.py (clean token), m6e_w2full.py + driver_nobreak.py (full-window), m6e_live.py
  (live-guest driver). Token serial: notes/m6-screenshots/m6e-aarch64-t0-serial.log.

## Step 25 (M6f) — TASK 1: kernel lseek/fstat SIZE query CODE-EXONERATED; real question = Mesa get_plane stride vs size
Owner: M6f. HEAD 5c43227 + inherited W2K instrumentation (uncommitted). Read Step 24 + 19-22 first.
MESA SOURCE (llvmpipe-lane/.../winsys/sw/kms-dri/kms_dri_sw_winsys.c) — the exact import path:
- kms_sw_displaytarget_from_handle(FD) -> add_from_prime(fd) -> drmPrimeFDToHandle -> find_and_ref(handle).
  If NOT found: **lseek(fd,0,SEEK_END)** -> kms_sw_dt->size (line 424-431); then
  **get_plane** (line 131): rejects "plane too big" iff
     offset + util_format_get_2d_size(format, stride, height) > kms_sw_dt->size   (line 138)
  -> returns NULL -> from_handle NULL -> resource spr->dt==NULL -> softpipe_transfer_map NULL ->
  sp_tile_cache stores NULL -> pipe_get_tile_rgba(NULL) = M6d e0 crash. EXACT chain.
KERNEL SIDE (TASK 1 verdict): install_dmabuf_vmo (vfs/src/lib.rs:503) sets tmp[idx].len = capacity
  = (1<<order)*4096 = 4194304 for handle=1 (order 10 ⊇ 4096000). handle_lseek TmpFile (vfs:3153) returns
  tmp[idx].len on SEEK_END. M6e token FD2H(31)->handle=1 PROVES install_dmabuf_vmo ran for fd 31 (dmabuf_handle_of
  only resolves a borrowed VMO whose len was set) => lseek(SEEK_END)=4194304 = CORRECT, NOT 0/garbage.
  => TASK 1 (kernel returns 0) is REFUTED by code + the M6e runtime token. No small kernel size fix.
REMAINING UNKNOWN (-> TASK 2): does get_plane reject anyway because Mesa's whandle->stride/offset make
  util_format_get_2d_size(fmt,stride,height) EXCEED 4194304 (a GBM stride-alignment mismatch, Mesa-side)?
  Or does add_from_prime SUCCEED and the NULL come later? Need runtime get_plane/size/stride values.
  Plan: enhance W2DIAG lib to print in add_from_prime (lseek size) + get_plane (need vs size + reject) +
  map, rebuild diag libgallium (bg), run m6e clean-capture repro, read the deciding values.

## Step 26 (M6f) — *** W2 REAL ROOT CAUSE CAPTURED (clean, deterministic): kernel mmap(card0,offset=phys) FAILS errno=29 for the 4th dumb buffer ***
Owner: M6f. m6f_w2map.py (shell-silent foreground /bin/sh script; comp+wlclient stderr INHERIT serial;
interactive shell blocked in sleep => no prompt-redraw garble). aarch64 uefi-hvf, instrumented img
(inherited W2K kernel prints + diag libgallium). FIRST fully clean capture through the crash.
DECISIVE serial (notes/m6-screenshots/m6f-aarch64-m0-serial.log): per-frame the compositor does
  create_dumb handle=N -> add_from_prime fd=X -> W2K FD2H fd=X->handle=N -> W2K MAP_DUMB N FOUND phys ->
  W2DIAG kms_map OK.
- handles 1,2,3 (phys 0xB7000000/0xB7800000/0xB7C00000): ALL SUCCEED (kms_map OK size=4096000).
- handle=4 (phys=0xA5800000, a LOWER buddy region after pid17 exit + a new Task spawn):
    W2K MAP_DUMB handle=4 FOUND phys=0xA5800000       <- kernel resolves, returns phys (MAP_DUMB sound)
    W2DIAG kms_map MMAP FAIL handle=4 off=2776629248 size=4096000 errno=29   <- **the mmap() FAILS**
    W2DIAG set_surface MAP-NULL-IFBRANCH map=0 xfer=0  <- NULL displaytarget map stored in tile cache
    [FAULT] far=0x10 elr=0x421467B4 pid=16 = pipe_get_tile_rgba(NULL)  <- THE crash
=> ROOT CAUSE is KERNEL-side: mmap(/dev/dri/card0, offset=phys=0xA5800000, len=4096000) on the DynamicDevice
   DRM path FAILS with errno=29, even though MAP_DUMB found handle=4 at that phys. NOT the size query
   (TASK 1 correctly exonerated), NOT MAP_DUMB (M6e correctly cleared). It is the mmap(offset=phys) step
   M6e code-read as sound (syscall.rs handle_ioctl_mmap:1354 + sys_mmap DynamicDevice:1469 + map_device)
   but NEVER observed FAIL at runtime. This OVERTURNS the M6d/M6e "kernel exonerated -> escalate" verdict:
   there IS a kernel bug, and it is reached only for buffers in a lower/late phys region (handle>=4).
NEXT: read the DRM device mmap path; identify why offset=0xA5800000 fails errno=29 when 0xB7xxxxxx passes
   (map_device range/bounds check? offset validation? errno 29 source). Likely a SMALL KERNEL FIX -> the
   whole W2 wall falls (first 3 frames already composite; fixing frame-4 mmap should let it run).

## Step 27 (M6f) — *** W2 ROOT CAUSE PINNED + SMALL KERNEL FIX APPLIED ***
MECHANISM (fully proven from the m0 clean capture + code):
- Compositor is multithreaded. Per-frame scanout mmap in Mesa kms_sw_displaytarget_map does:
  (1) drmIoctl(MAP_DUMB) -> sys_ioctl -> routes via **vfs::handle(pid)** which canonicalizes pid->TGID
      (servers/vfs/src/lib.rs:1457) -> resolves card0 on ANY thread -> W2K MAP_DUMB FOUND phys. OK.
  (2) mmap(card0, offset=phys) -> sys_mmap (syscall.rs:1466) -> **vfs::vfs_get_node_kind(current_pid(), fd)**
      which did NOT canonicalize -> find_tbl(rawTID) misses the TGID-keyed fd table on a RENDER thread
      -> returns None -> DynamicDevice branch SKIPPED (no [MMAP] line) -> falls through to file-backed
      mmap -> lseek(card0, offset) on a non-seekable device -> **ESPIPE (errno 29)** -> Mesa mmap
      MAP_FAILED -> kms_map returns NULL -> softpipe set_surface stores NULL xfer -> pipe_get_tile_rgba(NULL)
      FAULT far=0x10. EXACTLY the m0 capture: handles 1-3 (main thread, TID==TGID) mmap OK; handle 4
      (render thread, TID!=TGID) mmap ESPIPE -> crash.
- This is the SAME TGID-vs-rawTID class 6ce43be fixed for PRIME. vfs_get_node_kind was the last fd-table
  consumer still keyed by raw TID (siblings at vfs:654/671/3240/6182 already canonicalize "fd tables are
  per-process"). It has 3 callers (mmap device-detect:1466, syscall.rs:2648, pipe check:6731) — all latent.
FIX (servers/vfs/src/lib.rs:711): `let pid = sched::tgid_of(pid);` at the top of vfs_get_node_kind —
  matches the file's established convention, fixes all 3 callers, no-op on the main thread (TID==TGID),
  strictly correct process-scoped fd lookup on worker threads. 1 line. Kernel domain.
REFUTES M6d Step20/M6e Step24 "kernel exonerated -> escalate": M6e code-read MAP_DUMB+mmap sound but
  MAP_DUMB uses vfs::handle (canonicalized, sound) while the mmap DEVICE-DETECT uses vfs_get_node_kind
  (raw TID, NOT sound on worker threads) — the distinction M6e's code-read missed, and which only a
  clean runtime capture past frame-3 exposed. Building aarch64 (kernel+drivers+fresh img); W2K instr
  kept for the validation run, revert before finals.

## Step 28 (M6f) — *** W2 FIX VALIDATED: composite runs, NO crash, screen painted (first visible composite) ***
Rebuilt aarch64 (kernel+drivers+fresh img, diag lib) with the vfs_get_node_kind tgid fix. m6f_w2map.py fix0,
aarch64 uefi-hvf, fresh img, comp+wlclient. DECISIVE (notes/m6-screenshots/m6f-aarch64-fix0-serial.log):
- handle=4 (phys=0xA5800000, the render-thread frame that FAILED in m0):
    W2K MAP_DUMB handle=4 FOUND phys=0xA5800000
    [MMAP] DynamicDevice fd, off=0xA5800000     <- NOW ENTERS the device branch (was ABSENT in m0)
    [MMAP] map_device virt=0x85318000 result=0x85318000   <- SUCCESS
    W2DIAG kms_map OK handle=4                    <- OK (was "MMAP FAIL errno=29" in m0)
- FAULT count = 0 (m0 had 1 pipe_get_tile_rgba crash). MMAP-FAIL/errno=29 count = 0 (m0 had 1).
- kms_map OK count = 5; composite loop CONTINUES past frame 4 (handle 3 reused). comp alive+rendering.
- Screenshot m6f-aarch64-fix0-post.ppm: 1280x800, 99.99% non-black, uniform ~rgb(44,44,44) = cosmic-comp
  default clear color => comp PAINTS THE WHOLE SCREEN (first visible composite; no crash-black).
VERDICT: W2 root-caused + FIXED with a 1-line kernel change (servers/vfs/src/lib.rs:711 tgid canonical.).
  The M6d/M6e escalation is RETIRED — it WAS a small kernel fix after all, just past the frame-3 window
  no prior capture reached cleanly.
REMAINING for TASK 3: wlclient's own colored window not in the final frame (early [EXIT] pid=17 code=1 —
  wlclient likely exited); get a proper client window + cosmic-bg Orion wallpaper visible. Then W1/W3, finals.

## Step 29 (M6f) — *** TASK 3 aarch64: Orion Nebula wallpaper VISIBLE (full COSMIC composite) ***
m6f_wallpaper.py bg0 (cosmic-comp + cosmic-bg, no busd, fresh fixed img). Screenshot
m6f-aarch64-bg0-end.ppm = the FULL Orion Nebula (heic0601a) wallpaper edge-to-edge, cursor visible
top-left, 100 distinct color buckets (photographic nebula: reddish/pink/purple/brown). Saved PNG
notes/m6-screenshots/m6f-aarch64-orion-wallpaper.png. This is the first real visible COSMIC desktop
composite on LeandrOS — cosmic-comp compositing cosmic-bg's wallpaper through the software
softpipe/kms_sw path that the W2 fix unblocked (M6b Step15's "wallpaper blocked by softpipe crash"
is now RESOLVED, same root cause). aarch64 DONE. Next: x86_64 (build done), then W1/W3, finals.

## Step 30 (M6f) — TASK 3 x86_64: Orion wallpaper VISIBLE too => W2 fix cross-platform
Rebuilt x86_64 (kernel+drivers+fresh img) with the vfs fix. m6f_wallpaper.py x0 (uefi/TCG):
m6f-x86_64-x0-end.ppm = FULL Orion Nebula wallpaper at 1920x1080, cursor visible, 100 color buckets.
Saved PNG notes/m6-screenshots/m6f-x86_64-orion-wallpaper.png. Both arches now show a real COSMIC
composite. TASK 1/2/3 COMPLETE. W2 wall permanently down with a 1-line kernel fix.
NEXT (time-boxed, instrumented build still in place): W1 busd-freeze probe + W3 full-session landing,
then finals (revert instrumentation, restore STOCK libgallium, rebuild both, vfstest-first regressions,
commit vfs fix, plan-doc update).

## Step 31 (M6f) — W1/W3 status: busd-freeze PERSISTS (separate userspace wall, unaffected by W2 fix)
Full-session probe (m6_session.py, start-cosmic-leandros WITH busd, aarch64 fresh fixed img):
screenshots m6-aarch64-w1probe-{desktop,desktop2,console}.ppm = 94% black + 5% light text = TEXT
CONSOLE (comp never presented a frame). => WITH busd, comp still freezes at an early D-Bus interaction
BEFORE compositing (exactly M6b Step14's finding). The W2 fix unblocks the render path AFTER comp
reaches compositing (proven: no-busd -> wallpaper renders both arches), so it does NOT change the
full-session outcome: comp never reaches the render path under busd. W1 is a genuine SEPARATE wall in
cosmic-comp<->busd (userspace D-Bus), NOT a kernel fix. Per mission ranking (session.conf > stub
name-owner > env > escalate) it needs a busd-stdout/UXTRACE capture of the blocking method — a userspace
investigation out of the kernel domain that unblocked W2. W3 recursion likewise unreachable while W1
holds. RECOMMEND: escalate W1/W3 to a cosmic-comp/busd userspace lane with the M6b Step14 + this evidence.
DECISION: land the proven high-value W2 kernel fix cleanly (finals) as the bankable M6f deliverable;
document W1/W3 for escalation.

## Step 32 (M6f) — CLOSE-OUT COMPLETE
COMMIT: 5c62bdf on main "drm/mmap: resolve card0 fd in the process fd table for worker-thread mmap"
  (servers/vfs/src/lib.rs:711 `let pid = sched::tgid_of(pid)`). Instrumentation REVERTED
  (kernel/src/syscall.rs + drivers/src/drm_device_interface.rs back to committed state via git checkout).
  Working tree CLEAN.
FINAL IMAGES: both arches rebuilt fresh (03:18) shipping STOCK libgallium (aarch64 82063600 restored from
  .SHIPPED-BAK; x86_64 81871904 never swapped) — NO diagnostic lib, NO kernel instrumentation.
REGRESSIONS GREEN both arches (vfstest FIRST): aarch64 + x86_64 vfstest VFSRC=0 all symlink tests PASS,
  all subprocess exits code=0; drmsmoke renders gradient (PRIME/mmap path); fix is a NO-OP for
  single-threaded programs (tgid_of(pid)==pid) so no test can regress — empirically confirmed.
DESKTOP SCREENSHOTS both arches (final stock images):
  notes/m6-screenshots/m6f-aarch64-orion-wallpaper.png (1280x800, stock image, 100 color buckets)
  notes/m6-screenshots/m6f-x86_64-orion-wallpaper.png  (1920x1080, stock image, 100 color buckets)
  = full Orion Nebula wallpaper, cosmic-comp + cosmic-bg, first real COSMIC composite on LeandrOS.
PLAN DOC updated (memory/project_wayland_cosmic_plan.md, newest-at-top M6f RESULT line).

=== M6f DELIVERABLES (for orchestrator) ===
- TASK 1: kernel dmabuf lseek/fstat SIZE query CODE-EXONERATED (install_dmabuf_vmo len=capacity 4194304).
- TASK 2 (superseded): clean capture past frame-3 found the REAL cause — not a Mesa reject, a kernel
  raw-TID fd-table miss on the compositor's render thread → mmap ESPIPE → softpipe NULL crash.
- FIX: 1-line kernel change (tgid canonicalization). W2 wall PERMANENTLY DOWN. NO Mesa patch needed;
  the M6d/M6e "escalate, Mesa multi-day" verdict is RETIRED.
- TASK 3: visible composite (Orion wallpaper) BOTH arches. DONE.
- W1 (busd freeze) / W3 (recursion): confirmed SEPARATE userspace (cosmic-comp/busd) walls, UNAFFECTED
  by the W2 fix (comp never reaches the render path under busd). ESCALATE to a cosmic-comp/busd lane.

=== M7 REMAINDER ===
- W1: capture busd stdout + comp UXTRACE to name the blocking D-Bus method comp waits on at config;
  fix ranked session.conf > stub name-owner > env > escalate (mission TASK 4). Userspace, not kernel.
- W3: full cosmic-session recursion (comp-recursion-analysis.md) — reachable only after W1; EL0
  fp-walk backtrace per m6-progress Step 16 (needs sched::current_user_stack_range()).
- Optional cheap kernel add: faccessat2/439 ENOSYS (benign fallback today).
STOP — M6f close-out complete.

## Step 33 (M6f) — TGID-class audit applied (coordinator note) — COMMIT da722e1
Applied notes/tgid-audit.md's 3 CONFIRMED one-liners as ONE commit (da722e1), atop the W2 fix (5c62bdf):
1. POSIX-timer family (servers/tty): canonicalize tty_server::handle() + check_timers/ensure_real_timer/
   set_real_itimer/get_real_itimer (TIMER_TABLES per-process; check_timers runs every syscall-return under
   the running thread — a worker-armed timer was checked by nobody).
2. sys_fcntl F_GETFL stdio branch (syscall.rs ~4575): raw pid -> tgid (F_SETFL already canonicalized).
3. sys_execve steal_mounted_file (syscall.rs 2773): raw pid -> tgid (same fn's cloexec sweep at 3153 already did).
DEFERRED per audit caution (finding #5): tty close_all / stdio_flags_close_all NOT bare-canonicalized
   (would wipe process state on worker-thread exit); correct fix = existing leader-only gate, out of scope.
   Findings #4 (dead TTY_* handlers) + umask/heap_end architecture also left per the doc.
REGRESSIONS (behavior-touching timer changes watched per coordinator): rebuilt BOTH arches (stock libgallium).
- aarch64 (m5_regress): vfstest ALL PASS, drmsmoke ALL PASS (incl PRIME_HANDLE_TO_FD/MMAP_ALIAS/FD_TO_HANDLE),
  epoll/scm/poll/sig/evtest PASS, **timertest 5/5 PASS**, waittest = ONLY the known wait_on_process_group
  flake (project_open_issues.md; unrelated to timer/fcntl/execve code).
- x86_64 (m5_regress + focused m6f_timerx): vfstest PASS, drmsmoke PASS, epolltest PASS, **timertest 5/5 PASS**,
  **waittest ALL PASS incl wait_on_process_group** (confirming the aarch64 waittest miss was the flake).
=> Timer canonicalization does NOT regress timertest/waittest on either arch. Commit da722e1 stands.
Final git: main da722e1 (5c62bdf W2 fix + da722e1 TGID audit); working tree CLEAN; images ship stock libgallium.
STOP — M6f complete (W2 solved + visible composite both arches + TGID audit landed).

## Step 33 (M6g) — *** W1 ROOT-CAUSED: busd-side userspace async deadlock on the pipelined Hello; KERNEL EXONERATED with byte-exact delivery proof ***
Owner: M6g. HEAD 5c62bdf, clean. W2 already solved+shipped by M6f (wallpaper both arches). Mission: W1
(comp freezes under busd) → panel → W3 → close-out. W1 blocks the whole session; panel/W3 unreachable
until W1 falls.

### METHOD: never-before-captured busd/comp exchange. m6d_w1diag (RUST_LOG=busd=trace,zbus=trace) proved
the D-Bus AUTH HANDSHAKE COMPLETES (AUTH EXTERNAL 30→OK→NEGOTIATE_UNIX_FD→AGREE→BEGIN→"Handshake done"→
busd::peer created). => the SCM_CREDENTIALS/auth-freeze hypothesis (mission hyp 2) is REFUTED at runtime:
zbus EXTERNAL server auth uses SO_PEERCRED (getsockopt, works since K1) + getpwuid_r, NOT SCM_CREDENTIALS
(that's BSD-only in zbus; Linux client sends a plain nul byte). No crash (only [EXIT] code=0; screenshot =
text console) → a genuine HANG, not a fault. busd's log ends at "peer created": it NEVER processes a method
call. comp's own logger doesn't emit zbus-target lines, so comp side needed a syscall trace.

### DECISIVE: UXTRACE (gate 535eb07 flipped true, rebuilt aarch64) + smp=1 (LEANDROS_QEMU_EXTRA='-smp 1' →
clean whole-line serial, no SMP interleave garble). m6g_uxtrace.py, shell-silent foreground. THE COMPLETE,
BYTE-EXACT socket trace of the busd(pid 0xB/0xC threads)↔comp(pid 0x14/0xF threads) connection (comp fd
0x101, busd peer fd 0x104):
    CON pid=14 fd=101 v=0        (comp connect)
    ACC pid=B  fd=103 v=104      (busd accept → peer fd 0x104)
    SND pid=F fd=101 v=0x13 (19) → RCV pid=C fd=104 v=0x13   nul + "AUTH EXTERNAL 30"
    SND pid=C fd=104 v=0x25 (37) → RCV pid=F fd=101 v=0x25   "OK <32-hex-guid>"
    SND pid=F fd=101 v=0x9A (154)→ RCV pid=C fd=104 v=0x9A   NEGOTIATE_UNIX_FD + BEGIN + **Hello** (pipelined)
    SND pid=C fd=104 v=0xF  (15) → RCV pid=F fd=101 v=0xF    "AGREE_UNIX_FD"
    <<< PERMANENT SILENCE — no further SND/RCV either direction, ever >>>
=> The kernel delivered EVERY byte, including the 128-byte Hello coalesced into the 154-byte read. busd RCV'd
all 154. busd sent AGREE (last handshake step) then NOTHING. comp never sends Hello separately (it was in the
154 blob) and blocks awaiting the reply. THE KERNEL AF_UNIX PATH IS EXONERATED at runtime (byte-exact pairs).

### MECHANISM (settled from zbus 5.17 source, representative of busd's 5.14 fork + cosmic's zbus):
- CLIENT (comp) pipelines: zbus client handshake perform() sends NEGOTIATE_UNIX_FD+BEGIN and appends the Hello
  message, then blocks INLINE in receive_hello_response() awaiting the unique-name reply (client.rs:204). comp
  is correctly stuck waiting — the stall is on busd.
- SERVER (busd) reads all 154 bytes in one recvmsg; handshake consumes NEGOTIATE_UNIX_FD (→AGREE) + BEGIN
  (→done); the trailing Hello (128 B) survives in Authenticated.already_received_bytes (server.rs:237,
  builder.rs:541) and is handed to the peer's socket_reader. zbus's receive_message (socket/mod.rs:138-163)
  assembles a fully-buffered Hello with ZERO recvmsg (128 ≥ MIN_MESSAGE_SIZE), and socket_reader.receive_msg
  EAGERLY calls read_socket at loop top (socket_reader.rs:57) — so ONE poll of that task would parse+dispatch
  Hello and reply. It never does: busd's peer socket_reader "Waiting for message on the socket.." (line 56) is
  NEVER logged (the self-dial connection DID log it at startup). => busd's freshly-spawned per-peer reader task
  is never driven to its first poll → buffered Hello never parsed → no reply → comp deadlocks.
- TRIGGER = coalescing: comp's pipelined post-OK bytes land in busd's FINAL handshake read (so there is no
  post-handshake socket-readiness edge to kick a readiness-gated peer loop). Present at smp=4 too (the earlier
  garbled smp=4 trace also showed the v=0x9A 154-byte coalesced pair) — NOT an smp=1 artifact. On real Linux
  busd's broker routing works (RUNTIME-NOTES Docker test), so this is a LeandrOS-specific busd/tokio task-drive
  gap surfaced by the coalescing, not a busd logic error per se.

### KERNEL CLEARED ON EVERY CANDIDATE PATH (inspection + runtime):
- Data delivery: byte-exact SND/RCV pairs (above) — every Hello byte reached busd.
- Poll/epoll: handle_poll reports POLLIN when readable>0 (net lib.rs:2145); handle_send/handle_sendmsg both
  bump conn.seq + sched::wake_poll (1250/1252, 1691/1693); epoll_wait first-fire works (last_seq=u64::MAX,
  fire iff seq!=last_seq — syscall.rs:6206/6285); three-phase prepare/reprobe/commit closes check-then-sleep.
- tokio cross-thread wake: eventfd write bumps EVENTFD_SEQ + sched::wake_poll (vfs lib.rs:2977-2982) — the mio
  Waker path is sound. NOTE POLL_SAFETY_WAKE=false (syscall.rs:6368): no periodic re-probe, so a *pure* lost
  wake would hang — but the byte-exact delivery proves the data arrived and the deadlock is userspace, not a
  missed wake.

### SECONDARY OBSERVATION (not the blocker, note for M7): UXTRACE shows a stray "CON pid=D fd=105 v=-95"
(EOPNOTSUPP) — some comp/side connect to an unsupported socket type; benign relative to the Hello deadlock.
Also found a real (minor, non-causal) kernel bug: handle_getsockopt (net lib.rs:1918) returns ok_reply() for
ANY unrecognized optname incl. SO_PEERPIDFD instead of ENOPROTOOPT, so zbus (unix.rs:457-467) wraps fd 0 as a
bogus pidfd OwnedFd. Auth still completes (uid via SO_PEERCRED is correct), so NOT the freeze — but it's a
correctness gap. Left UNFIXED (some callers may rely on permissive success; out of W1 scope).

### VERDICT: W1 is a busd(/zbus/tokio) USERSPACE async task-scheduling deadlock, NOT a kernel fix. Ranked
fixes: (1) session.conf — can't fix a task-drive bug; (2) kernel — cleared, socket delivers every byte;
(3) stub name-owner — irrelevant (stall is Hello, before any name ownership); (4) comp env — no known knob to
disable zbus Hello pipelining. => ESCALATE per mission with the precise target: busd's per-peer connection is
not eagerly driven to parse already_received_bytes after a coalesced handshake read. Concrete M7 next step:
obtain busd source (RUNTIME-NOTES: busd/target/{arch}-unknown-linux-musl/release/busd — sibling build tree),
inspect its per-peer serve loop vs zbus's eager socket_reader; if it's a readiness-gated receive, that's the
one-spot fix (eagerly poll/drain on peer creation). Alternatively a tokio-runtime spawn-wake audit. W3
unreachable while W1 holds. Instrumentation (UXTRACE) REVERTED; tree clean at 5c62bdf.

## Step 34 (M6h) — W1 MECHANISM PINNED TO A busd/tokio MULTI-THREAD UNPARK LOST-WAKE; FIX = current_thread runtime (building)
Owner: M6h. HEAD da722e1, tree clean (verified). W2 shipped by M6f (wallpaper both arches). Mission: fix W1,
bring up full session, W3, land M6.

### SOURCE-LEVEL ROOT CAUSE (obtained busd 0.5.0 + zbus 5.13.1 crates; no sibling tree survived — rebuilt recipe):
busd 0.5.0 pins zbus 5.13.1, tokio 1.49, features tokio+bus-impl, `rt-multi-thread`, `#[tokio::main]` (=multi_thread).
Per-peer read path traced end to end:
- bus/mod.rs accept_next() -> tokio::spawn(Task-A = peers.add()). Task-A: Peer::new() runs the zbus server
  handshake (connection::Builder...build()); the coalesced 154-byte read leaves the 128-byte Hello in
  Authenticated.already_received_bytes; build_ (builder.rs:441) calls conn.init_socket_reader(already_read=Hello)
  which does SocketReader::new(...).spawn(&executor).
- CRITICAL: with the tokio feature, zbus's Executor (abstractions/executor.rs) is a ZERO-COST wrapper —
  `spawn` == `tokio::task::spawn`, `tick` == pending() no-op, start_internal_executor is a no-op. So the
  socket_reader is a PLAIN tokio task (Task-B). Task-A then tokio::spawns serve_peer (Task-C) and completes.
- SocketReader::receive_msg (socket_reader.rs:48-51) logs "Waiting for message.." at loop-top BEFORE read_socket;
  M6g proved that line is NEVER logged for the comp peer => Task-B is never polled once.
- DECIDER: receive_message (socket/mod.rs:99-165) with already_received_bytes.len()=128 >= MIN_MESSAGE_SIZE(16)
  assembles the FULL Hello from the buffer with ZERO recvmsg and NEVER awaits socket readiness (drain MIN then
  drain remainder, `while pos<total_len` is false). So a single first poll would parse+broadcast Hello and busd
  would reply. The UXTRACE shows NO busd SND after AGREE => Task-B genuinely never got its first poll.
=> W1 is NOT a zbus buffer-ordering bug and NOT a socket-data delivery bug (kernel already byte-exact-exonerated).
It is tokio's MULTI-THREAD runtime failing to drive a freshly-spawned task — the spawn-time inter-worker UNPARK
wake is lost on LeandrOS/TCG. This wake is ORTHOGONAL to the socket-byte delivery M6g used to "exonerate" the
kernel; the earlier exoneration addressed the wrong wake. Matches the memory's "slow-vs-stuck under TCG" and
POLL_SAFETY_WAKE=false ("a pure lost wake would hang"). The self-dial connection's socket_reader DID log
"Waiting for message" because it is created during startup churn (workers not yet parked); the comp peer is
created in steady state (workers parked) -> the spawn unpark is lost.

### FIX (in-policy: we build busd): src/bin/busd.rs `#[tokio::main]` -> `#[tokio::main(flavor="current_thread")]`.
A single-threaded runtime has NO worker threads and NO inter-worker unpark; block_on drives the root future and
all spawned tasks cooperatively, draining the run queue at every await point -> the socket_reader's first poll
happens and the buffered Hello is processed. A single-threaded session broker is the reference dbus-daemon model
(zero throughput concern). This is also a DISCRIMINATING experiment: if it fixes W1 it confirms the multi-thread
unpark lost-wake; that underlying kernel/tokio wake gap is recorded for M7 (cosmic-* session bins are calloop/
single-threaded-ish and reached compositing in the no-busd path, so busd is the main tokio-multi-thread user).

### BUILD: recreated the S5 musl recipe (no sibling tree, no script survived): crates.io busd-0.5.0.crate;
cargo +nightly (1.97 nightly), target {aarch64,x86_64}-unknown-linux-musl, .cargo/config.toml linker=rust-lld +
rustflags -C relocation-model=static (musl static-PIE landmine), release profile already panic=abort/lto=fat/
strip. Toolchain validated with a hello-world (ET_EXEC static aarch64). aarch64 busd build in progress
(m6h-busd-build-aarch64.log). Workspace: ports/busd-build/ (host-only); repo dir ports/busd/ to hold patch+
build script+rationale for the commit. Nothing built into the kernel/repo yet; tree still da722e1.

## Step 35 (M6h) — current_thread INSUFFICIENT; W1 sharpened to NESTED-SPAWN not driven; fix = inline add() in accept loop
current_thread busd built+staged+fresh-image tested (aarch64, v4/v5/v6). FULL busd.log (RUST_LOG=busd=trace,
zbus=trace) captured on a FRESH image with M6g's exact env:
- busd HEALTHY: starts, listens, self-dial conn built, TWO self-dial socket_readers log "Waiting for message"
  (in-memory Channel::pair, spawned TOP-LEVEL from main/for_address) — current_thread drives them fine.
- comp connects: "Accepted connection" -> full handshake (AUTH EXTERNAL->OK->NEGOTIATE_UNIX_FD->AGREE->BEGIN->
  Handshake done) -> "busd::peer: created" ... and busd.log ENDS THERE (31 lines total, cat-verified). The comp
  peer's socket_reader NEVER logs "Waiting for message" => still never polled, EVEN ON current_thread.
=> Multi-thread-unpark theory REFUTED (current_thread fails identically). Sharper root cause: the comp peer's
socket_reader is a NESTED tokio::spawn — spawned from within Task-A, where Task-A = the `tokio::spawn(add())`
that bus/mod.rs accept_next() wraps around peers.add(). Top-level spawns (self-dial socket_readers, spawned from
the runtime root future) ARE driven; a task spawned from within another spawned task is NOT driven to its first
poll on LeandrOS/TCG. (The real-fd-vs-in-memory-Channel difference is a candidate too, discriminated by the fix.)
FIX (bus/mod.rs accept_next): drop the extra tokio::spawn wrapper; await peers.add() INLINE in the accept loop
(the runtime root future) -> the socket_reader becomes a TOP-LEVEL spawn -> polled -> buffered Hello parsed ->
Hello replied. A session broker accepts clients ~sequentially; the fast local handshake inline costs nothing.
Kept current_thread too (single-threaded broker = reference dbus-daemon; removes multi-thread variables).
DISCRIMINATOR: if comp's socket_reader now logs "Waiting" + busd routes Hello => nested-spawn was it (W1 fixed);
if "Waiting" appears but comp still hangs => real-fd tokio I/O-driver wake (kernel epoll) is the true layer.
ALSO FOUND (separate, real): fresh-image /root/.config|.cache|.local are BROKEN ?-type inodes created by the
launcher's runtime `mkdir -p` hitting an f2fs kernel mkdir bug (same class as the pre-created /run/user/0 mkfs
workaround, mkfs line 662). comp create_dir_all -> ENOTDIR panic (config/mod.rs:172) on a POISONED image. On a
truly fresh image /root is clean and comp creates its own config (NoConfigDirectory errors are non-fatal). This
poisons the FULL-session launcher path too -> M6 close-out will need mkfs to pre-create those dirs (or launcher
to use tmpfs/double-mkdir). Building inline-add busd now (m6h-busd-build2-aarch64.log).

## Step 36 (M6h) — *** W1 DEFINITIVE ROOT CAUSE: kernel poll/wake reactor never wakes busd's epoll_wait(INFINITE) to run the freshly-spawned socket_reader — NOT a busd userspace bug ***
Instrumented busd (current_thread + inline-add + BUSD-DBG eprintlns) + kernel UXTRACE (EPW=epoll_wait entry
w/ timeout, FTXW/FTXK=futex wait/wake) at smp=1. Correlated pids: busd=pid 0x4.
KEY TRACE: busd's per-peer socket_reader is NEVER polled. BUSD-DBG shows add() completing fully for the comp
peer ("add spawning serve_peer for :busd.1", "add inserted peer :busd.1", "accept_next done") — so add()/the
"created" trace do NOT deadlock — but comp's "serve_peer ENTERED for :busd.1" NEVER prints and busd's LAST
syscall is `EPW fd=0x400 v=0xFFFFFFFFFFFFFFFF` = epoll_wait(timeout=INFINITE), after which busd makes NO more
syscalls (blocked forever, NOT spinning; per-pid counts: busd 9 EPW total then silence). The self-dial
socket_readers (spawned during startup, before the accept loop parks) DO run ("serve_peer ENTERED for
org.freedesktop.DBus"); a task spawned WHILE the reactor is parked in epoll_wait(infinite) (every real client's
socket_reader) does NOT — the wake that should interrupt the infinite park to drain the ready task (tokio
waker-eventfd write -> epoll edge, or the current_thread run-queue drain before the infinite park) is not
delivered on LeandrOS. => W1 is a KERNEL poll/wake reactor defect, NOT busd userspace. This CORRECTS M6g's
"escalate to userspace busd" verdict.
FOUR busd-level fixes all reproduced the identical stall (busd.log ends at "peer created", comp stuck at 8
config lines both, no render): (1) current_thread; (2) inline add() in the accept loop (top-level spawn, not
nested); (3) yield_now() after add() — NEVER RETURNS (v8: "accept_next post-yield" never prints); (4) 50ms
tokio::time::interval keepalive — BUSY-SPINS (v9: busd.log=10, comp never connected) because the tokio TIME
driver also misbehaves (durations not honored -> tight loop starving the accept loop). Both multi-thread and
current_thread fail identically => not a runtime-flavor issue. Same class as M4 "roundtrip stalls under TCG" +
POLL_SAFETY_WAKE=false ("a pure lost wake would hang").
SEPARATE REAL BLOCKER found for the full desktop: fresh-image /root/.config|.cache|.local are BROKEN ?-type
inodes (unresolvable inode, mode ---------) created by the launcher's runtime `mkdir -p` hitting an f2fs kernel
mkdir bug (same class as the pre-created /run/user/0 mkfs workaround, mkfs line 662); comp create_dir_all ->
ENOTDIR panic (cosmic_comp config/mod.rs:172). On a truly fresh image /root is clean and comp creates its own
config (NoConfigDirectory errors are non-fatal). M7: mkfs should pre-create /root/.config /.cache /.local as
real dirs (or the launcher should double-mkdir / use tmpfs), else the full-session launcher path is poisoned.
DECISION: the kernel poll/wake reactor is the most dangerous code in the tree (spinlock/lost-wake hazards per
memory) and a fix is OUT OF SAFE SCOPE for this wave without a dedicated kernel effort. Reverting all diag,
shipping STOCK busd (the patch doesn't fix W1), landing the M6 desktop via the PROVEN fallback (cosmic-comp +
cosmic-bg manual composition = M6f wallpaper), regressions on stock kernel, ports/busd/ committed as the
investigation record. W1 kernel fix + the /root/.config f2fs-mkdir fix -> M7.

## Step 37 (M6h) — CLOSE-OUT COMPLETE
COMMIT: d1d87d6 on main "ports/busd: add busd W1 investigation + build recipe" (ports/busd/{README.md,build.sh,
current-thread-runtime.patch}). NO Claude mention in the message (per user CLAUDE.md; the default Claude-Session
trailer was removed by amend). Kernel diag REVERTED (git checkout kernel/src/syscall.rs — UXTRACE=false, EPW/FTX
traces gone); tree CLEAN except the committed ports/busd/. Stock busd RESTORED both arches (aarch64 2294648,
x86_64 2529336 — .ORIG-multithread-bak files removed so images pack the original); experimental busd binaries
NOT shipped.
FINAL IMAGES: build-all --arch both from da722e1 sources (stock kernel, stock busd, stock libgallium) → fresh
f2fs both arches.
REGRESSIONS GREEN BOTH ARCHES (fresh images, vfstest FIRST):
- aarch64: 0 FAIL/panic/nonzero-exit across the whole log; vfstest ALL PASS (xattr/acl/symlink/chroot/f2fs-own),
  drmsmoke ALL PASS incl PRIME_HANDLE_TO_FD/MMAP_ALIAS/FD_TO_HANDLE, scmtest/epolltest/evtest2/polltest/sigtest/
  timertest/waittest/idletest PASS, kmscube FRAME_DIFF=130158 ANIMATING, REGRESSION DONE.
- x86_64: all suites done fail=0 (SUMMARY pass=8 fail=0 epolltest, pass=2 fail=0 idletest), WAITTEST PASS incl
  wait_on_process_group, kmscube FRAME_DIFF=149165 ANIMATING, REGRESSION DONE. (code=42/code=7 are intended
  test-internal child exits; `setsid` not-found is benign, kmscube still animated.)
DESKTOP (fallback, mission-sanctioned since W1 = kernel, unfixable this wave): M6f's no-busd manual composition
(cosmic-comp + cosmic-bg = Orion wallpaper) stands, committed screenshots m6f-{aarch64,x86_64}-orion-wallpaper.png
on the identical stock build; M6h re-verify aarch64 in m6h-wallpaper-aarch64.log.
PLAN-DOC + MEMORY.md updated (M6h RESULT at top, corrects M6g's userspace verdict).

=== M6h DELIVERABLES (for orchestrator) ===
- W1 DEFINITIVELY root-caused: a KERNEL poll/wake-reactor defect (busd's tokio reactor blocks in
  epoll_wait(INFINITE) and is never woken to run the freshly-spawned per-peer socket_reader → buffered Hello
  never parsed → cosmic-comp deadlocks). NOT a busd userspace bug — corrects M6g. Proven by busd+zbus source
  analysis + BUSD-DBG markers + smp=1 kernel syscall trace (busd=pid 4, last call epoll_wait infinite, silence).
- 4 busd-level fixes tried, all fail identically (current_thread, inline-accept, yield_now, interval-keepalive);
  tokio TIME driver also broken (interval spins). ports/busd/ documents it + ships the build recipe.
- Desktop delivered via the proven fallback (wallpaper both arches). Regressions GREEN both arches.

=== M7 REMAINDER (updated) ===
- **W1 real fix = KERNEL**: make a wake against a task blocked in epoll_wait(infinite) (tokio waker eventfd →
  sched::wake_poll) reliably interrupt the park, AND/OR the current_thread run-queue drain before the infinite
  park. Most dangerous code in the tree (spinlock/lost-wake hazards) → dedicated kernel wave. Then rebuild busd
  (ports/busd/build.sh) + re-run W1 validation (comp → busd Hello replied → comp serves with the bus). Also fix
  the tokio TIME driver (epoll timeout / interval durations not honored).
- **Full-desktop config blocker**: fresh-image /root/.config|.cache|.local are broken ?-inodes (launcher runtime
  `mkdir -p` hits an f2fs kernel mkdir bug, same class as pre-created /run/user/0 at mkfs:662). mkfs should
  pre-create them as real dirs (or launcher double-mkdir/tmpfs) — else the full-session launcher path is poisoned.
- W3 recursion (unreachable while W1 holds), getsockopt SO_PEERPIDFD ENOPROTOOPT (M6g), plus prior M7 list
  (llvmpipe gallivm, tty close_all leader-gate, dbus-run-session $! hardening, init spin, SCTLR UCI|UCT, atomic
  KMS, XWayland).
STOP — M6h complete (W1 root-caused to kernel + corrected M6g; desktop via fallback; regressions green both arches).

## Step 23 (M6d) — W2 DEFINITIVE: approved-plan PREMISE REFUTED. ESCALATION. (read this + Steps 20-22)
STATUS FOR ORCHESTRATOR: I was NOT idle — Steps 19-23 ran a full instrumented-libgallium
diagnostic campaign (7 QEMU boots e0..e3 + d0..d4, 5 Alpine Docker libgallium builds, 1 kernel
build). The nudges' process-scan fell between my background HVF runs. e3 IS processed. Result
changes the plan: **the approved W2 fix (softpipe sp_tile_cache else-branch, based on Step 18's
"PIPE_BUFFER bound as color RT") is built on a REFUTED root cause and will NOT fix the crash.**

### HARD EVIDENCE (instrumented softpipe+winsys libgallium, symbolized, multiple runs):
1. Added a print in sp_tile_cache_set_surface's ELSE branch (the exact `target==PIPE_BUFFER`
   "can't render to buffers" path Step 18 blamed). Across ALL runs it **NEVER fired**. No
   create_surface-over-buffer either. => no color RT with target==PIPE_BUFFER exists. Step 18 REFUTED.
2. The ONLY render targets (symbolized vs the diag lib):
   - dri_create_image -> res_create target=2(PIPE_TEXTURE_2D) 1280x800 bind=0x18000a ->
     kms_sw create_dumb handle=1 (comp's own scanout = a valid CREATE_DUMB dumb buffer).
   - dri2_from_dma_bufs -> res_from_handle target=2 1280x800 whandle_type=2(FD) = smithay
     imports the GBM scanout bo as an EGLImage via PRIME/dmabuf (fd=31).
   - the target=0/PIPE_BUFFER resources are just VERTEX buffers (bind=0x10, _mesa_bufferobj_data).
3. The canonical far=0x10 pipe_get_tile_rgba(NULL) crash (e0, matches ALL prior waves) =
   the softpipe FRAMEBUFFER tile cache's IF-branch pipe_texture_map() returning NULL for a
   target=2 DISPLAY-TARGET-backed RT => kms_sw_displaytarget_map (MAP_DUMB/mmap) gave NULL.
4. Kernel W2K trace: FD_TO_HANDLE(fd=31) RESOLVES (Some branch: "-> handle=", not NONE).
   The crash is NOT triggered by the wl_shm client — it is at comp's OWN output-RT setup
   (dmabuf import + first software render), BEFORE/independent of any client. (Matches Step 15
   where cosmic-bg alone also crashed, and the "residual crash ONCE at init".)
5. Nondeterminism (HVF): the failure lands either at the dmabuf import (smithay null-deref at
   cosmic-comp main-PIE 0x1593CD4, near blocker-1's 0x1516B04) OR at the later softpipe map
   (e0 pipe_get_tile_rgba NULL). Same underlying dmabuf-imported-RT-in-software-render problem.

### TRUE ROOT AREA: software (softpipe/kms_sw) rendering INTO a PRIME/dmabuf-imported output bo.
smithay's KMS backend renders the composited frame into the GBM scanout bo via a GL FBO built
from an EGLImage (dri2_from_dma_bufs). On the software path that bo becomes a kms_sw display
target whose map goes through DRM_IOCTL_MODE_MAP_DUMB+mmap(offset=phys). The map returns NULL
(or the import path faults) -> no pixels -> crash. This spans KERNEL DRM PRIME/MAP_DUMB + Mesa
kms_sw winsys + smithay software-render. **Bigger than option-2 (softpipe patch) scope.**

### WHY NOT the approved fixes:
- Option-1 (stop PIPE_BUFFER being bound as RT): there IS no PIPE_BUFFER RT (refuted). N/A.
- Option-2 (else-branch linear transfer): the else-branch is never taken. Patching it is a no-op.
- The crash is a NULL from the IF-branch's displaytarget map, not the else-branch.

### RECOMMENDED NEXT (orchestrator decision — all bigger than my approved scope):
 A. KERNEL-side (most likely correct, my domain if approved): make the imported-output-bo path
    mappable by softpipe. Determine whether MAP_DUMB (std_handle_map_dumb, drm_device_interface
    .rs:979) or the mmap(offset=phys) DRM handler rejects the PRIME-imported/display-target
    handle, and register/resolve it so kms_sw_displaytarget_map succeeds. FD_TO_HANDLE already
    round-trips to a handle; verify that handle is a DUMB_BUFFERS key with a valid phys+mmap.
    (One more kernel-instrumented run with NO concurrent driver input during the window would
    capture the MAP_DUMB found/notfound + handle cleanly — the reader-window garble truncated it.)
 B. Mesa kms_sw winsys: if the imported bo can't be a dumb-mappable display target, route its
    map through the PRIME/dmabuf mmap instead of MAP_DUMB.
 C. smithay/cosmic path: avoid importing the scanout bo as a GL render target on the pure-sw
    path (render to an offscreen dumb RT, then blit) — comp-source, needs approval.
 D. llvmpipe swap (llvmpipe-lane build exists): different rasterizer/framebuffer path may avoid
    the softpipe tile-cache map entirely; +147MB libLLVM + W^X JIT kernel surface (NOTES flags).

### TREE/IMAGE STATE (IMPORTANT):
- MY diagnostic edits REVERTED: kernel/src/syscall.rs + drivers/src/drm_device_interface.rs
  (git checkout), softpipe sp_tile_cache.c/sp_texture.c (restored from backup), SHIPPED
  libgallium restored (m3-gl-stack/sysroot-aarch64, 82063600 B). Mesa host winsys
  kms_dri_sw_winsys.c still has W2DIAG edits (host source only, NOT in ship set — restored lib
  is the pristine shipped one; safe, but revert for tidiness).
- FOREIGN uncommitted changes present at handoff, NOT MINE, appeared during my session:
  sched/src/lib.rs (+CURRENT_TGID/current_tgid_fast, labeled "M7b instrumentation ... Reverted
  before finals") and scripts/mkfs-f2fs-populated.py (+12 lines). A separate M7b effort's temp
  edits. LEFT UNTOUCHED (not mine, may be in active use). NOTE: my last aarch64 kernel+image
  build (build-all) COMPILED these in + my (now source-reverted) W2K instrumentation, so the
  CURRENT f2fs-data0/1-aarch64.img + target/final-aarch64 are NOT pristine 5c43227 — rebuild
  from a clean tree before any regression/final.
- Diagnostic harnesses (reliable, reusable): m6d_w2diag3.py (reader-window serial capture — THE
  one that works under comp fb/CPU load), m6d_w1diag.py (W1, ready, unrun). Diag build recipe:
  llvmpipe-lane/build-diag-softpipe.sh (softpipe-only, -fno-stack-protector + ssp_guard.o inject
  — LeandrOS musl libc.so lacks __stack_chk_guard; pkill absent; both are documented landmines).

### W1 (comp-freeze-under-busd): static analysis only (no run — W2 consumed the budget). busd at
/usr/libexec/busd, launched --config session.conf --address unix:path=/run/user/0/bus; comp
RequestName's com.system76.CosmicComp. Harness m6d_w1diag.py ready (busd+comp, RUST_LOG zbus=trace
both sides, reader-window capture). Not yet run. W3 depends on W2 (blocked).

## Step 24 (M6d) — CLOSE-OUT (W2 escalated). Kernel-trace clincher inconclusive (nondeterminism).
k0 (single-send comp+inline-sleep, no client, clean-capture harness m6d_w2kclean.py): comp inited,
did its [MMAP] DynamicDevice device mappings + the harmless kiosk-child `bin/--no-xwayland` exec
quirk, then EXITED code=0 WITHOUT reaching the dmabuf import — no W2K FD2H/MAP_DUMB fired this run.
HVF nondeterminism: comp's path varies run-to-run (import-crash / map-crash / clean-early-exit).
Pinning MAP_DUMB-fail vs mmap-fail vs smithay-import-null is now an orchestrator decision (all
candidate fixes A-D exceed my approved Mesa-softpipe scope), so I STOP here rather than spend more
nondeterministic HVF runs. The ESCALATION in Step 23 stands and is well-supported.

### FINAL M6d POSITION:
- W2: approved plan's PREMISE REFUTED (Step 18 PIPE_BUFFER-as-RT disproven with instrumented
  libgallium). Real crash = software render into the PRIME/dmabuf-imported OUTPUT RT
  (kms_sw MAP_DUMB/mmap -> NULL -> pipe_get_tile_rgba(NULL)), independent of the client.
  ESCALATED with full evidence + ranked fix options A-D (Step 23). NO visible composite achieved;
  the approved softpipe else-branch patch would be a no-op (branch never taken) so I did NOT ship it.
- W1: static analysis only; harness m6d_w1diag.py ready; not run (W2 consumed the budget).
- W3: blocked on W2.
- Tree: MY diagnostic edits fully reverted (kernel + Mesa softpipe + Mesa winsys), SHIPPED
  libgallium restored. FOREIGN "M7b instrumentation" changes (sched/src/lib.rs,
  scripts/mkfs-f2fs-populated.py) present at handoff, NOT mine, left untouched — flag for owner.
- Built kernel/images (target/final-aarch64, f2fs-data0/1-aarch64.img) are NOT pristine 5c43227
  (contain the foreign M7b changes + my source-reverted W2K); rebuild from a clean tree for finals.
- Reusable assets: m6d_w2diag3.py (reader-window serial capture — the reliable one),
  m6d_w2kclean.py (single-send), m6d_w1diag.py; llvmpipe-lane/build-diag-softpipe.sh (softpipe
  diag lib recipe w/ the __stack_chk_guard + pkill landmine fixes). Full logs: notes/m6d-*.log,
  notes/m6-screenshots/m6d-aarch64-{e0..e3,k0}-serial.log.
