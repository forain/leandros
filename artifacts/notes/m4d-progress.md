# M4d wave — progress + resume instructions

Owner: deep-reasoner M4-EXIT wave. Exclusive git/QEMU/images. main @ 06defe1 at start.
Resume: read this file top-to-bottom, then continue from the last "NEXT" block.

## Inherited state (verified by prior waves, do NOT re-derive)
- main 06defe1. M0-M3 + K5 done both arches. Kernel PRIME/dmabuf done (6ce43be), scanout
  unblock = ioctl sign-ext mask 8a2a271. NO Mesa patch exists. Kernel socket stack AUDITED
  SOUND. MSG_DONTWAIT net "gap" = verified NO-OP.
- UNCOMMITTED: kernel/src/syscall.rs has UXTRACE instrumentation (const UXTRACE=true; uxtrace()
  on ACC/CON/SND/RCV -> serial "UXTR <tag> pid= fd= v="). Keep gated-false before final commit.
- m4c decisive finding: clean aarch64 uefi-tcg run, env correct ([/run/user/0][wayland-1]).
  wlclient: "connected to display" -> "roundtrip: requesting globals..." -> HANG.
  FULL UXTR = ONE line `CON pid=6 fd=256 v=0`. NO ACC/SND/RCV. anvil NEVER accepts.
  anvil.log frozen at line 40 "Creating new Output" (guest t=43.63s). QEMU 296% CPU, 3 vCPU
  threads pegged -> anvil COMPUTE-BOUND at first-frame softpipe render under TCG, BEFORE calloop.
  slow-vs-stuck poll NEVER CONCLUDED -> that is my first job.

## Key facts established this wave (m4d)
- Scanout 1280x800 "Native" mode (clock 72576) comes from virtio-gpu GET_DISPLAY_INFO at boot
  (virtio_gpu.rs get_display_info; fallback 1280x800 at drm_device_interface.rs:1324). smithay
  builds the "Native" mode from info.width/height at drm_device_interface.rs:903-921. DrmModeInfo
  list in drm/device.rs:106-108 (1920/1024/800) is NOT what anvil picks.
- Prior good log m4-anvilrun-aarch64-serial.log: "Creating new Output" reached at guest t~2s;
  it is consistently the LAST anvil.log line across all captures -> stall is AFTER it (first render).
- run-qemu.sh / driver.py accel: aarch64 uefi defaults HVF on Apple Silicon; "uefi-tcg" forces TCG,
  "uefi-hvf" forces HVF. x86_64 = TCG only (no HVF). Serial sock server=on,wait=off (single client,
  reconnect OK). Monitor sock separate. driver.py start does boot-detect to login: marker (120s).
- HVF risk: prior waves saw HVF boot hang at virtio-input init (2/2 recent). Retryable (boot-time).

## Decisive experiment (RUNNING / see result below)
Script: ~/code/leandros-artifacts/m4d_resolve.py (background). Logic:
  1. Try HVF boot up to 4x (stop+pkill, start uefi-hvf, detect login marker). 
  2. If HVF boots: login, env, launch anvil, wait 30s, run wlclient, check UXTR ACC/SND/RCV +
     wl.log roundtrip + screenshot. Client serviced => SLOW-not-stuck PROVEN + exit crit 1 in one shot.
  3. If HVF never boots: fall through to TCG uefi-tcg, launch anvil, poll anvil.log line-count
     every 60s up to ~35 min. Growth past line 40 = SLOW. Flat + 300% CPU = STUCK.
Output -> ~/code/leandros-artifacts/notes/m4d-resolve.log

## RUN HISTORY (m4d)
- HVF BOOTS RELIABLY FIRST-TRY this session (2/2) — the virtio-input hang did NOT recur. HVF is viable.
- Run 1 (bjnu... earlier): my in-script guest_login was broken (empty-line nudge caused Login incorrect
  loop) -> switched to driver.py login (proven robust).
- Run 2: login OK but anvil launch line serial-corrupted (shell exec'd '/tmp/anvil.log', execve EINVAL)
  because my Serial.send() lacked the CR-wait-for-prompt sync. FIXED: send() now mirrors driver.py
  _serial_send (drain, send \r, wait for '#' redraw, THEN 8-byte-chunk 2-space-pad write).
- Run 3 (bjnuyqxrs): RUNNING with robust send. Watch ~/code/leandros-artifacts/notes/m4d-resolve.log
  for "[poll hvf] ... lines=N" with N>0 (anvil actually launched) then forward-progress verdict.

## *** SLOW-VS-STUCK RESOLVED: SLOW-NOT-STUCK (2026-07-22 ~22:24) ***
- DECISIVE: HVF anvil screenshot /tmp/m4d-hvf-anvil.png shows FULL lavender desktop (compositor clear
  color) + rendered cursor sprite @ 1280x800. anvil completes EGL/GLES softpipe first-frame render and
  IS in its calloop. The TCG "compute-bound at Creating new Output" was pure softpipe wall-clock slowness,
  NOT a hang. Verdict: the code path is correct; TCG is just slow. HVF is the exit vehicle (boots reliably
  first-try this session, virtio-input hang did NOT recur).
- HVF anvil guest-time to modeset/"Setting new mode Native" ~= 2:11 guest (settle 90s wall). anvil uses
  SMITHAY_USE_LEGACY=1 legacy-DRM path (quieter logs).
- BUT client did NOT composite in that run (client screenshot == desktop, no window). ROOT CAUSE of the
  mess: serial corruption STRIPPED the redirect from `anvil ... >/tmp/anvil.log 2>&1 &` -> anvil stderr
  flooded serial (7 smithay lines incl libinput New device event0/1 leaked), driver.py cmd timed out,
  and wl.log/anvil.log reads were drowned. wlclient likely never launched cleanly.
- FIX: baked /bin/gorun + /bin/clrun launcher scripts onto the f2fs image (redirects+env INSIDE the
  file), invoked as `brush /bin/gorun &` — short corruption-proof serial line, redirect survives, no
  serial flood. Local patch in scripts/mkfs-f2fs-populated.py (REVERT before final commits). Launchers
  at ~/code/leandros-artifacts/m4-launchers/{gorun,clrun}. aarch64 image regenerated 22:29 with them.

## *** MAJOR FINDING (2026-07-22 ~22:40): M4 blocker is a KERNEL BUSY-SPIN, not slowness ***
Full HVF exit run (m4d_exit.py, /bin/gorun launcher, image w/ launchers):
- anvil renders desktop+cursor (frame 0) — screenshots A/B/C/D/E all show lavender desktop + cursor
  sprite at TOP-LEFT.
- CRIT1 FAIL: wlclient "connected to display" -> "roundtrip: requesting globals..." -> HANG (both B
  and E). UXTR = exactly ONE `CON` line, NO ACC/SND/RCV. anvil NEVER accepts the client.
- CRIT2 FAIL: QMP tablet move 6000,6000 then 26000,20000 -> cursor does NOT move (C==D==A, cursor
  stuck top-left). anvil processes NO input either.
- DECISIVE: QEMU at 303% CPU / 17min CPU-time, SUSTAINED, long after desktop rendered. anvil is
  BUSY-SPINNING (3 vCPU threads), NOT idle in epoll_wait. So it's neither pure-slow nor pre-calloop-
  stuck: anvil renders frame 0 then busy-loops and services NO event source (not the wayland listen
  socket, not libinput). Prior waves misread the 303% as "TCG first-frame softpipe slowness" — WRONG,
  it persists under HVF forever.
- anvil.log (clean, 40 lines): normal init through "Creating new Output"; NOTE WARN "failed to create
  signaled syncobj err=PermissionDenied" (DRM SYNCOBJ_CREATE EPERM); big guest-time gap 1.79s(modeset)
  -> 90s(libinput New device event0/1); then SILENT (no vblank/flip/frame logs).
- Kernel DRM event channel (drivers/src/drm_device_interface.rs:349-425) throttles flip events to
  100Hz via drm_tick -> so a ~100fps flip-driven render loop = 303% softpipe is plausible, BUT that
  alone shouldn't starve accept (calloop returns to epoll each frame). Need to know WHICH syscall anvil
  hammers.

## DIAGNOSTIC IN FLIGHT
- Added gated SYSTRACE to kernel dispatch_inner (syscall.rs): samples 1/100000 syscalls -> serial
  "SYSTR num=<hex> pid=<hex>". Under the spin the dominant syscall localizes the loop; if SYSTR barely
  advances while CPU pegged -> spin is USERSPACE compute (unthrottled render). UXTRACE also still on.
- Building aarch64 (bg bky577vha) -> ~/code/leandros-artifacts/notes/m4d-build-aarch64.log. After build:
  boot HVF, /bin/gorun, ~40s, read SYSTR stream (grep 'SYSTR num=' in serial log; histogram the nums).
  Syscall numbers: epoll_pwait/epoll_wait, ppoll, ioctl, clock_gettime, sched_yield, recvmsg are the
  suspects. Map hex num -> name via kernel/src/syscall.rs const table.

## Launcher/harness recipe that WORKS (use for all further runs)
- Serial corruption strips redirect chars from long launch lines -> bake launchers on image:
  /bin/gorun (anvil), /bin/clrun (wlclient), invoked `brush /bin/gorun &`. Redirects live in the file.
  Sources: ~/code/leandros-artifacts/m4-launchers/. Local patch in scripts/mkfs-f2fs-populated.py
  (bin_files append) — REVERT before final commits.
- driver.py cmd is the robust delivery path; my m4d_exit.py uses it. Screenshots via monitor screendump
  -> convert `sips -s format png`. wl.log/anvil.log are CLEAN to cat now (redirects intact, no serial
  flood).

## CONFIRMED (2026-07-22 ~23:xx): anvil spins in PURE USERSPACE, ~ZERO syscalls
- SYSTRACE (dispatch_inner, 1/2000, pid>=3, +caller ELR): 0 samples during a 35s spin window while
  QEMU at 298% CPU. SYSTRACE verified working (3 samples at boot; "SYSTR num=" string in kernel bin).
  => anvil makes <2000 syscalls in 35s (<57/s) while burning ~3 vCPUs. It is NOT epoll-polling, NOT
  doing a syscall-driven render loop. It is stuck in a PURE USERSPACE COMPUTE/BUSY-LOOP.
- Cursor does NOT move to QMP tablet input (C==D) AND client never accepted (UXTR: CON only) => anvil's
  calloop is NOT dispatching ANY event source. So anvil renders frame 0 (desktop+cursor) then gets
  stuck inside a single non-returning userspace operation (never returns to event_loop.dispatch).
- smithay render path (anvil/src/udev.rs): main loop event_loop.dispatch(Some(16ms)) @547; renders are
  VBlank/timer driven; reschedule logic @1542 re-arms a repaint timer each frame (reschedule_timeout
  saturates to 0 if a softpipe render exceeds one frame -> back-to-back renders = high CPU). BUT that
  still returns to calloop each frame, so the stuck-ness means anvil is stuck INSIDE render_surface /
  Mesa softpipe (main thread blocks on GL worker completion, or a userspace fence spin). Clue: anvil.log
  WARN "failed to create signaled syncobj err=PermissionDenied" (kernel has NO SYNCOBJ ioctl impl).
- anvil ELF: own .text vaddr 0x1d6dd0..0x62c1b8 (~4.3MB); PCs far above => shared libs (Mesa/libgallium
  softpipe / libEGL). load base unknown (dyn-PIE via ld-musl; no /proc/maps).

## DIAGNOSTIC: PC sampler (arch/aarch64/src/exception.rs, gated PCSAMPLE)
- Timer-IRQ (100Hz) samples interrupted PC every 12th tick, tagged EL0 (userspace) vs EL1 (kernel):
  "PCSAMP EL0 elr=<hex>". EL0-dominant => userspace spin (anvil/Mesa); EL1-dominant => kernel spin.
  Run m4d_systr.py -> ~/code/leandros-artifacts/notes/m4d-pcsamp.log (RUNNING b5h9ii6tz).
- To symbolize an EL0 PC in anvil text: subtract load base, llvm-nm/objdump on anvil-aarch64. If PC is
  in a high shared-lib range it's Mesa softpipe (confirms render-stuck).

## THREE gated diagnostics now in tree (uncommitted, kernel/src/syscall.rs + arch/aarch64/src/exception.rs):
  UXTRACE (keep, flip false for M5), SYSTRACE (revert or keep-false), PCSAMPLE (revert or keep-false).
  Plus LOCAL scripts/mkfs-f2fs-populated.py launcher patch (REVERT before commit).

## *** ROOT CAUSE FOUND (2026-07-22): sys_wait4 busy-poll starves the compositor ***
PCSAMP (timer-IRQ PC sampler, EL0 vs EL1): 1050 samples, **100% EL1 (kernel)**, 0 EL0. anvil is NOT
spinning in userspace — it is BLOCKED. The 298% CPU is the KERNEL. 3 hot PCs only:
  - sched::scheduler_run_loop +0x230 / +0xb8  (72% of samples) — scheduler churning, never idles/WFI
  - kernel::syscall::sys_wait4 +0xb0          (28%)            — a process busy-polling in wait4
Symbolized via llvm-nm on target/final-aarch64/kernel (base 0xffffffffc0080000).
DIAGNOSIS: sys_wait4's StillRunning branch was `irq_window(); yield_now("wait4")` = a BUSY-POLL. For a
LONG-RUNNING child (anvil runs for the whole session) the reaper (init / the launching shell) never
sleeps — it stays perpetually runnable, pins a CPU, and churns scheduler_run_loop across vCPUs. This
saturates the run-queue and (via scheduler/lock churn) starves anvil: anvil renders frame 0 then blocks
in epoll_wait and its wake (handle_connect DOES call wake_poll, net lib.rs:1154) can't get it scheduled
-> client never accepted (UXTR CON, no ACC), input never dispatched (cursor frozen). This is the SINGLE
red herring that fooled EVERY prior wave into "anvil is compute-bound / TCG softpipe slow." It was never
anvil and never slowness — it was a kernel wait4 spin. Short-lived children hid the bug for years.

## FIX APPLIED (kernel/src/syscall.rs sys_wait4 StillRunning branch)
Replaced the yield_now spin with a proper block on the poll wait-channel:
  block_on_poll_prepare(); register_poll_deadline(ticks()+2);
  if wait_peek(sel,tgid)==StillRunning && !interrupted { block_on_poll_commit() } else { cancel() }
Woken by child-exit SIGCHLD -> wake_poll (sched:343); wait_peek re-check closes the check-then-sleep
lost-wake; 2-tick poll deadline bounds any missed edge to ~20ms (never a hang). Mirrors sys_epoll_wait's
three-phase block. Drops a long-running child's reaper from 100% spin to ~0% idle.
Building (bw0t35qwy). After build: boot HVF, confirm shell/reaping still work, then run FULL M4 exit
(m4d_exit.py aarch64 uefi-hvf) -> expect CPU to drop AND anvil to accept+composite the client + cursor
+ key. If it works: M4 UNBLOCKED. PCSAMPLE still on to confirm the scheduler spin is gone.

## FIX PROGRESS
- After wait4 fix: CPU 298% -> 106% (wait4 spin gone). But PCSAMP still showed scheduler_run_loop
  (0xC00C4FDC/C5154) => a SECOND busy-poller remained: sys_waitid had the IDENTICAL irq_window();
  yield_now("waitid") pattern (brush's blocking waitid on the foreground anvil under `brush /bin/gorun`).
  Applied the SAME block-on-poll fix to sys_waitid (kernel/src/syscall.rs ~line 1904).
- Disabled console-flooding diagnostics for clean runs: SYSTRACE=false (syscall.rs), PCSAMPLE=false
  (arch/aarch64/src/exception.rs). UXTRACE=true kept (low-volume, confirms ACC). [The PCSAMP flood was
  also drawing over anvil's framebuffer in the interim run — screenshot B showed console text.]
- Rebuilding + clean full M4 exit (build5 + m4d-exit-hvf3.log, bg bjyoxg8vu). Expect: CPU ~low, anvil
  desktop displays (no console flood), wl.log roundtrip done + configured->painted, cursor C!=D, KEY.

## GATED DIAGNOSTICS / LOCAL CHANGES STATE (for final commit hygiene)
- kernel/src/syscall.rs: UXTRACE=true (KEEP, gated), SYSTRACE=false (revert helper or keep gated).
- arch/aarch64/src/exception.rs: PCSAMPLE=false + handle_irq(from_el0) sig change (revert or keep gated).
- REAL FIX (KEEP+COMMIT): sys_wait4 + sys_waitid block-on-poll (replaces yield_now spin).
- scripts/mkfs-f2fs-populated.py launcher patch (gorun/clrun) — LOCAL test scaffolding, REVERT before commit.
  (Decide: bake the launchers permanently? Probably revert; they were harness workarounds.)

## STATUS after wait4+waitid fixes (2026-07-23)
- Clean run (build5, diagnostics off): anvil DESKTOP DISPLAYS (screenshot A lavender+cursor) — renders
  fine. CPU 204%. BUT client STILL not accepted (UXTR CON only, no ACC), cursor still frozen. So the
  wait4/waitid busy-polls were REAL bugs (worth keeping the fix) but NOT the accept blocker.
- PCSAMP2 (fixes in, PCSAMP on): 929 samples, STILL 100% EL1, now all in sched::scheduler_run_loop
  +0x230/+0xb8 (the wait4/waitid samples are GONE). scheduler_run_loop idle path DOES wfi (sched/src/
  lib.rs:1450) — so pick_next() keeps returning a runnable task => SOME task is perpetually runnable,
  yielding immediately. NOT wait4/waitid (fixed), NOT anvil (0 EL0 => anvil is BLOCKED, not spinning).
- Endemic pattern: many yield_now busy-poll sites in syscall.rs (nanosleep:2281, read/write sock/vfs,
  net_blocking_op:5840, read_stdin:3559...). The remaining spinner is likely another of these that
  anvil or init/shell hits. Added pid= to PCSAMP (build7, m4d-pcsamp3.log RUNNING b84wagle8) to ID the
  perpetually-runnable task. NOTE: scheduler_run_loop samples may show pid=0 (scheduler ctx); look for
  any task-PC sample or a dominant non-zero pid.
- OPEN QUESTION unresolved: is the accept-failure caused by spin starvation, or an independent epoll/
  unix-listen-wake bug? handle_connect DOES wake_poll (net:1154); epoll_wait blocks on POLL_WAIT_CHANNEL.
  Plan: kill the remaining spinner(s) -> CPU ~0 -> re-test accept. If still fails, it's an independent
  epoll bug (investigate listening-fd readiness delivery to anvil's epoll).

## SPINNER IDENTIFIED = PID 1 (init), root cause = sys_nanosleep busy-poll (2026-07-23)
- PICKTRACE (rate-limited log of the dispatched pid in sched::scheduler_run_loop): 2102/2102 dispatches
  = pid 1 (init). init is the perpetually-runnable task churning the scheduler at ~200% CPU.
- init getty loop (userland/init/src/main.rs:66-79): fork /bin/login, wait4(pid,0), usleep(1_000_000).
  sys_nanosleep (kernel/src/syscall.rs:2261) was a yield_now BUSY-POLL loop until the deadline -> init's
  usleep(1s) spins a CPU for the whole second, and the getty loop re-spins. THIRD instance of the same
  yield_now-instead-of-block anti-pattern (wait4, waitid, nanosleep).
- FIX: nanosleep now blocks on the poll wait-channel with register_poll_deadline(deadline) (woken exactly
  at the deadline tick). Same shape as the wait4/waitid fixes.
- Rebuild+full M4 exit RUNNING (build9 + m4d-exit-hvf4.log, bg bh8qtiiih), PICKTRACE off, PCSAMPLE off,
  UXTRACE on, nanosleep+wait4+waitid fixes in. HYPOTHESIS: CPU drops to ~0, init stops starving anvil,
  anvil accepts+composites the client (M4 unblocked). If accept STILL fails at ~0% CPU -> independent
  epoll/unix-listen-wake bug (next investigation).

## THE REAL PATTERN (for the report)
Multiple blocking syscalls busy-poll via yield_now instead of blocking (violating the kernel's own
"~0% idle" goal): sys_wait4, sys_waitid, sys_nanosleep (all fixed). Others still using yield_now:
sigsuspend:1943, sigtimedwait:1990, sys_read_* / sys_write_* / net_blocking_op (those wait on data and
may be OK). Short-lived callers hid these for years; a long-running compositor (anvil) + init's getty
usleep exposed them as a sustained ~300% CPU that EVERY prior wave misread as "anvil TCG-softpipe slow."

## THE DOMINANT SPINNER = net_daemon (2026-07-23)
- YIELDTRACE (log yield_now reason+pid rate-limited): 2081/2081 = ("net_daemon", pid 1). The persistent
  ~100-200% CPU is servers/net/src/lib.rs:592 net_daemon() — the smoltcp poll loop spawned as a kernel
  task (kernel/src/init.rs:89). It did `poll(); yield_now("net_daemon")` in a TIGHT loop = a CPU pinned
  at 100% FROM BOOT. This is a PRE-EXISTING busy-poll, the biggest single component of the "300% compute-
  bound" that every prior wave misread as anvil/TCG-softpipe slowness.
- FIX: net_daemon now block_on_poll_prepare + register_poll_deadline(ticks()+1) + commit -> polls smoltcp
  at ~100Hz instead of 1.4M/s, woken earlier by any wake_poll (socket traffic). CPU 100%->~0.
- NOTE: net_daemon shares NET_STACK.lock() with the unix accept/connect path (handle_accept:886,
  handle_connect:1047) — BUT M1/M2 unix wayland roundtrips worked despite net_daemon spinning, so it is
  likely NOT the anvil-accept blocker by itself. This build (11) is the decisive test: CPU should hit ~0;
  if anvil THEN accepts -> collective busy-poll starvation was the cause (M4 unblocked); if accept STILL
  fails at ~0% CPU -> the anvil-accept failure is an INDEPENDENT bug (epoll/unix-listen readiness edge
  specific to anvil's calloop usage) needing separate investigation.

## FULL FIX SET so far (all the SAME yield_now-busy-poll anti-pattern -> block_on_poll):
  sys_wait4, sys_waitid, sys_nanosleep (kernel/src/syscall.rs); net_daemon (servers/net/src/lib.rs).
  These are genuine, correct kernel bug fixes regardless of M4. Build11 + m4d-exit-hvf5.log (bg b862bspwr).

## ============ FINAL CONCLUSION (2026-07-23) ============
CPU ladder across the 4 busy-poll fixes: 300%(orig) -> 200% (wait4/waitid) -> 200% (nanosleep) ->
100% (net_daemon). There is STILL ~1 more busy-poller (CPU floor ~100%, not chased).
*** DECISIVE: the anvil client-accept FAILURE persisted IDENTICALLY at 300%, 200%, AND 100% CPU. ***
=> The M4 blocker (anvil never accept()s the wl client: UXTR CON, no ACC; cursor frozen; no composite)
   is INDEPENDENT of the busy-poll starvation. Reducing CPU did NOT help. So the busy-poll spins were a
   real, long-standing red herring (the "compute-bound"/"TCG-slow" story of EVERY prior wave) but are
   NOT the accept blocker.

WHAT IS RESOLVED (primary mandate): slow-vs-stuck. It is NOT slowness and NOT TCG-softpipe. anvil renders
its desktop+cursor (frame 0) and DISPLAYS it under HVF within seconds (screenshots m4d-aarch64-hvf-A..E).
The sustained ~300% CPU that fooled everyone = a STACK of kernel busy-poll syscalls (yield_now instead of
block): net_daemon (the biggest, spins from boot), sys_wait4, sys_waitid, sys_nanosleep. anvil itself is
BLOCKED (0 EL0 samples in 3000+ timer-IRQ PC samples), not computing.

THE REAL M4 BLOCKER (still open): anvil renders frame 0, then never services its event loop — never
accepts the wl client, never processes QMP input. 0 EL0 => anvil is blocked in a SYSCALL. handle_connect
DOES wake_poll (net:1154) and epoll_wait blocks on POLL_WAIT_CHANNEL, yet no ACC. Leading hypothesis:
anvil is blocked NOT in epoll_wait but in a DRM path (page-flip/vblank completion wait after the first
eglSwapBuffers) that never completes, so it never returns to calloop to poll the listening socket. NEXT
WAVE: enable SYSCALL_TRACE (syscall.rs:796, currently false; note it excludes 0x16 epoll_pwait/0x65) OR a
targeted per-anvil-pid last-syscall logger to capture anvil's exact blocked syscall after the client
connects. Compare vs kmscube (which animates via the same drm_tick flip channel) — anvil's smithay
legacy-DRM flip-wait may differ. Also re-verify NET_POLL(UnixListening) returns POLLIN for anvil's
pending connect and that anvil's epoll actually has the listening fd armed.

TREE STATE (all UNCOMMITTED, NOT committed — M4 not achieved, fixes not regression-validated):
- REAL FIXES (correct, boot/login/DHCP/reaping all still work; keep pending regression): sys_wait4 +
  sys_waitid + sys_nanosleep block-on-poll (kernel/src/syscall.rs); net_daemon block-on-poll
  (servers/net/src/lib.rs). >=1 more busy-poller remains (CPU floor ~100%).
- UXTRACE=true (keep, gated). SYSTRACE=false, PCSAMPLE=false (+handle_irq from_el0 sig change in
  exception.rs), PICKTRACE=false, YIELDTRACE=false (+PICK/YLD code in sched/src/lib.rs) — gated-off
  diagnostics; revert or keep-gated.
- scripts/mkfs-f2fs-populated.py launcher patch (gorun/clrun) — LOCAL test scaffolding, REVERT before any
  commit. Launchers at ~/code/leandros-artifacts/m4-launchers/. Needed to run anvil robustly for testing.
- x86_64 NOT rebuilt (still 16:37 pre-UXTRACE). Regressions NOT run. plan-doc NOT updated (M4 not done).

## (obsolete NEXT below — superseded by FINAL CONCLUSION)
## NEXT (resume here after the background script exits)
- Read ~/code/leandros-artifacts/notes/m4d-resolve.log. Determine slow-vs-stuck + which accel works.
- If slow-not-stuck: proceed to full M4 exit (wlclient composited screenshot, cursor via QMP tablet,
  keyboard event) on whichever accel is timely per arch; TCG wall-clock -> document as M7 perf item.
- Regression on FRESH f2fs images both arches: vfstest 34/34 FIRST, drmsmoke 20/20, scmtest 19/19,
  epolltest 8/8, evtest2 8/8, idletest 0, kmscube -D. Then commit (UXTRACE gated false), plan-doc.
