# M4e wave — progress + resume

Owner: M4e deep-reasoner. Exclusive git/QEMU/images. Inherit uncommitted tree, do NOT reset.
Mission: root-cause + fix anvil client-accept blocker, complete M4 exit both arches.

## Inherited (from m4d, VERIFIED — do not re-derive)
- anvil renders frame 0 (desktop+cursor) under HVF in seconds. NOT slowness/TCG-softpipe.
- Accept blocker persists IDENTICALLY at 300/200/100% CPU => INDEPENDENT of busy-poll starvation.
- anvil: 0 EL0 PC samples => BLOCKED IN A SYSCALL after frame0. UXTR shows CON, never ACC.
- Leading hyp: anvil blocked in DRM flip/vblank wait after 1st eglSwapBuffers, never returns to calloop.
- KEEP fixes (uncommitted): wait4+waitid+nanosleep block-on-poll (syscall.rs), net_daemon (net/lib.rs).
- Gated diag: UXTRACE=true(keep), SYSTRACE/PCSAMPLE/PICKTRACE/YIELDTRACE=false.
- LOCAL mkfs launcher patch (gorun/clrun) — REVERT before commit. Launchers in ~/code/leandros-artifacts/m4-launchers/.
- x86_64 image STALE (pre-UXTRACE), rebuild later.

## TASK 1 (decisive first): capture anvil's exact parked syscall after client connect.

## STATUS LOG
- [start] Read m4d-progress.md. Creating checkpoint. Next: read kernel syscall dispatch + DRM event code
  to design a per-pid last-syscall logger for a one-shot capture.

## [m4e step 1] Instrumentation + capture launched
- Added PARKTRACE (kernel/src/syscall.rs): per-pid last-syscall table, dumped from a tick hook
  (park_dump_tick, registered init.rs:81) every ~2s for any pid parked >1s. Output "PARK p= nr= a0= for=".
  Bumped MAX_TICK_HOOKS 4->6 (sched/src/lib.rs) so the diag can't lose the registration race. All gated
  by PARKTRACE=true — REVERT/gate-false before commit (bump too).
- Built aarch64 (m4e-build2.log, exit 0), fresh f2fs image w/ launchers.
- CANDIDATE BUG spotted by inspection: servers/net/src/lib.rs handle_poll UnixListening (line ~2113-2128)
  returns net_poll_reply(revents, seq=None). If epoll edge-emulation needs a seq to re-arm the listening
  fd, a None seq could drop the connect-readiness edge -> anvil's epoll never reports the listener
  readable -> no ACC. M1/M2 used blocking accept() (worked); anvil uses epoll/calloop on the listen fd.
- RUNNING: m4d_exit.py aarch64 uefi-hvf 120 (bg b188qjhcb) -> m4e-capture1.log. After: grep serial for
  PARK (anvil's parked syscall: nr 0x16=epoll_pwait? 0x1d=ioctl on card fd? 0x49=ppoll?) + UXTR CON/ACC.
- Syscall nr map needed: epoll_pwait aarch64=22(0x16), ppoll=73(0x49), read=63(0x3f), ioctl=29(0x1d),
  recvmsg=212(0xd4), ppoll_time64... confirm via nr:: table in syscall.rs.

## [m4e step 2] CAPTURE 1 RESULTS (m4e-capture1.log, serial m4d-exit-aarch64-hvf-serial.log)
Syscall decode (aarch64): 0x16=epoll_pwait(22), 0x62=futex(98), 0x3F=read(63), 0x49=ppoll(73),
  0x5E=exit_group(94) [STALE — a thread that exit_group's never returns from dispatch_inner so park_exit
  never runs; PARK_IN stays stuck-true. IGNORE nr=0x5E entries.]
- CLIENT wlclient = pid 0x0E(14): UXTR CON v=0 (connected OK), then PARKED in ppoll(0x49) a0=stack, "for"
  GROWING (0x230->0x550->0xB90) = stuck in wl_display_roundtrip awaiting server reply. NO ACC ever.
- anvil candidate main threads that NEVER wake (for grows monotonically, no reset across whole session
  INCLUDING across the client-connect wake_poll):
    p=5 nr=0x62 FUTEX a0=0x403A12B0     for 0x2EA6->0x3F0E
    p=6 nr=0x16 EPOLL_PWAIT a0=0x403    for 0x2E9B->0x3F03->0x4223
- Processes that DO wake (for resets): p=3,p=4 epoll (finite timeout, woken by poll_deadline_tick),
  p=8 read fd5.
- CONTRADICTION to resolve: p=6 is in epoll_pwait(infinite) and NEVER woke, even though the client's
  connect calls handle_connect->wake_poll (net lib.rs:1161) which unblock_port(POLL_WAIT_CHANNEL) wakes
  ALL poll-channel waiters. If p=6 were on the channel it MUST have woken (and re-blocked, resetting
  "for"). It didn't. => either p=6 is NOT anvil (anvil=p=5 futex, Mesa-GL-stuck), OR there is a real
  epoll edge-wake delivery bug for infinite-timeout waiters. Mesa workers use futex NOT epoll, so p=6 in
  epoll is anvil-main or an unrelated daemon.
- epoll_wait block path (syscall.rs ~6358): correct 3-phase block_on_poll_prepare/reprobe/commit on
  POLL_WAIT_CHANNEL. wake_poll=unblock_port(POLL_WAIT_CHANNEL) (sched). No obvious bug by inspection.
- DECISIVE NEXT: added EXEC log (pid->path at execve) + tg= (tgid) to PARK. Rebuild (build3, bg bjgux7lx4)
  + rerun to map anvil's pid and see if anvil-main is p=5(futex) or p=6(epoll). Then pick the fix.

## [m4e step 3] KEY REINTERPRETATION + targeted probe
- park_enter/park_exit run once per syscall in dispatch(); sys_epoll_wait re-blocks in an INTERNAL loop{}
  without returning. So growing "for" = syscall never RETURNED, NOT "never woke". p=6 (anvil?) likely
  wakes on each edge, re-probes the wayland listener, finds it NOT ready, re-blocks -> never accepts.
- probe_fd_events_seq (syscall.rs): net socket readiness = (state & requested) | err_bits. Listener
  returning POLLIN with an EPOLLIN interest DOES fire. Path correct. => bug is handle_poll returning 0
  (pending sock_id match fails) OR listener not in anvil's epoll.
- Added LSNTRACE (servers/net/src/lib.rs handle_poll UnixListening): when ANY UnixPendingAccept exists,
  logs "LSN pid fd lsid psid" (poller pid, listener fd, listener sock_id, pending connect target sock_id)
  via klog4/arch_serial_putc. If lsid!=psid -> match bug found. If never fires with anvil pid -> listener
  not in anvil epoll. EXEC log maps pid->binary; tg= groups threads.
- build4 (bg be2pk466j) batches EXEC+tgid+LSN. One run answers: anvil pid, its parked syscall, and the
  listener match result. Then design fix.

## [m4e step 4] *** ROOT-CAUSE LOCALIZED: anvil (pid 8) stuck in read(fd 5) ***
CAPTURE 2 (build4, m4e-capture2.log / serial): EXEC maps pids:
  p3=login->brush(interactive), p5=brush(runs gorun), p8=/bin/anvil, pE(14)=wlclient.
- anvil = PID 8 (tg=8), SINGLE-THREADED (softpipe, no worker threads). Parked in nr=0x3F READ, a0=fd 5,
  "for" RESETS (~0x178-0x240) => the read RETURNS every few seconds then re-enters (cycling), NOT one
  infinite block. anvil's calloop main loop is NOT reaching epoll_wait/ppoll — it's stuck reading fd 5.
- LSN NEVER FIRED => anvil never polled the wayland listener while the connect was pending => that is
  exactly why no ACC / no composite / no input dispatch. The listener/epoll/net paths are all FINE; anvil
  simply never gets back to its event loop.
- MECHANISM (kernel): default VFS read branch (syscall.rs ~3702 `_ =>`) BUSY-LOOPS on EAGAIN via
  yield_now("sys_read_vfs") for BLOCKING fds. So a blocking read on a device that returns EAGAIN spins a
  vCPU (= the remaining ~100% CPU floor prior waves saw) AND blocks anvil's main thread. card0 read via
  DRM server returns EAGAIN when no events.
- smithay reads card0 once-per-calloop-readiness (receive_events, drm crate single read), so a
  calloop-driven card0 read shouldn't block => fd 5 may NOT be card0, or anvil reads it synchronously
  outside poll, or our poll spuriously reports fd 5 readable. NEED fd 5 identity + open flags (O_NONBLOCK
  bit).
- Added OPEN log (sys_openat: "OPEN p= fd= fl= <path>"). build5 (bg pending) + rerun => fd 5 path/flags
  => decides the fix (kernel read-path nonblock/EAGAIN vs poll-readiness vs DRM event delivery).

## [m4e step 5] *** ROOT CAUSE CONFIRMED + FIX APPLIED ***
fd-probe run (build5): anvil (pid 8) OPENs show fd 5 reused for lib loads then a NON-openat creator (no
OPEN line for the final fd 5). anvil is a calloop 0.14.4 app; calloop's Timer uses a timer-wheel (no
timerfd) BUT the `polling` crate (calloop's poller) creates a **timerfd with TFD_NONBLOCK** for precise
poll timeouts on Linux -> that is fd 5. anvil parked in read(fd 5) cycling.
ROOT CAUSE (kernel): timerfd_create / eventfd2 / signalfd4 DROP their creation flags:
  - dispatch `TIMERFD_CREATE => sys_timerfd_create(a0)` dropped a1(flags); sys_timerfd_create passed no
    flags to VFS. sys_eventfd2 ignored `_flags`. sys_signalfd4 ignored `_flags`.
  - VFS handle_eventfd/handle_timerfd_create/handle_signalfd_create set `flags: 0` (handle_pipe already
    threaded flags correctly — that's why pipes worked).
  => fd_nonblock()=false for these fds. The VFS read EAGAIN path (syscall.rs `_ =>` ~3702) yield-spins
  (`yield_now("sys_read_vfs")`) for BLOCKING fds. So the polling crate's read of its NOT-yet-fired
  TFD_NONBLOCK timerfd busy-loops in-kernel instead of returning EAGAIN, PINNING anvil's single (softpipe)
  thread in the read -> calloop never returns to epoll_wait -> never polls the wayland listener (LSN never
  fired) -> client never accepted, input never dispatched. This is ALSO the remaining ~100% CPU spinner.
FIX (thread flags -> record O_NONBLOCK|O_CLOEXEC):
  - kernel: TIMERFD_CREATE dispatch passes a1; sys_timerfd_create/eventfd2/signalfd4 forward flags to VFS.
  - vfs: handle_eventfd/timerfd_create/signalfd_create take flags, store `flags & (O_NONBLOCK|O_CLOEXEC)`
    in FdEntry.flags (new module const O_NONBLOCK_FL). EventFd/TimerFd/SignalFd reads already return -11
    EAGAIN when empty, so nonblock now short-circuits the spin.
  - MKFD logging added (timerfd/eventfd/signalfd) to CONFIRM fd 5 identity on the verify run.
Building build6 -> full HVF exit run expected to accept+composite+cursor+key.

## [m4e step 6] VERIFY RUN (build6, m4e-verify1.log, bg b6qchso59)
- Full HVF exit: anvil+client+cursor+key. Watch for MKFD (fd 5 identity), anvil pid8 NOW in epoll_pwait
  (not read fd5), UXTR ACC, wl.log roundtrip+painted, cursor C!=D, KEY.
- FILES: kernel/src/syscall.rs (TIMERFD_CREATE dispatch a1; sys_timerfd_create/eventfd2/signalfd4 forward
  flags + MKFD log), servers/vfs/src/lib.rs (O_NONBLOCK_FL const; handle_eventfd/timerfd_create/
  signalfd_create take+store flags).

## [m4e step 6b] fd 5 CONFIRMED = eventfd (MKFD live serial)
MKFD p=8: fd3=eventfd fl=0x80800, fd4=timerfd fl=0x80800, fd5=eventfd fl=0x80800, fd6=eventfd, fd9=timerfd.
=> fd 5 = an eventfd created EFD_CLOEXEC|EFD_NONBLOCK (0x80800). Exactly the dropped O_NONBLOCK.
POST-FIX: anvil (pid 8) shows ZERO read-fd5 stalls and NO >1s PARK entries — it now cycles its event loop
normally (unstuck). Awaiting client-connect ACC + composite in verify run.

## FINAL-COMMIT CLEANUP PLAN (after verify + regression)
KEEP + COMMIT (real fixes):
  - busy-poll->block: sys_wait4, sys_waitid, sys_nanosleep (kernel/src/syscall.rs); net_daemon (net/lib.rs)
  - M4 FIX: eventfd/timerfd/signalfd O_NONBLOCK|O_CLOEXEC flag threading (kernel/src/syscall.rs +
    servers/vfs/src/lib.rs, O_NONBLOCK_FL const)
  - UXTRACE code, gated FALSE (M5 diagnostic per task)
REVERT/REMOVE (diagnostic scaffolding):
  - PARK table + park_enter/park_exit/park_dump_tick + OPEN log + MKFD log (kernel/src/syscall.rs)
  - init.rs park_dump_tick registration; sched MAX_TICK_HOOKS 6->4 bump
  - LSNTRACE + klog4 (servers/net/src/lib.rs)
  - PCSAMPLE + handle_irq from_el0 sig (arch/aarch64/src/exception.rs); PICKTRACE/YIELDTRACE (sched)
  - scripts/mkfs-f2fs-populated.py launcher patch (gorun/clrun)

## [m4e step 7] Clean verify in flight (build7: PARKTRACE=false, LSNTRACE=false, UXTRACE=true)
- build6 verify: anvil UNSTUCK (0 read-fd5 stalls live) but serial was truncated by PARK/OPEN flood so
  wl.log/UXTR unreadable; B-client screenshot = clean lavender+cursor, NO visible client window (couldn't
  confirm composite due to flood).
- Flipped PARKTRACE/LSNTRACE false, rebuilt build7. Running m4d_exit.py aarch64 uefi-hvf 120 ->
  m4e-verify2.log. Waiting (backgrounded grep-until loop, MY child) for UXTR ACC or RUN DONE.
- RESUME: read /tmp/leandros-serial.log for UXTR CON/ACC/SND/RCV; read wl.log via a fresh short driver.py
  cmd if needed (serial now quiet). Success = UXTR ACC + wl.log "roundtrip done"+"configured -> painted" +
  composite screenshot. Then: QMP cursor delta (C!=D), KEY, x86_64 rebuild+exit, fresh-image regressions
  both arches (vfstest FIRST), diagnostic revert per KEEP/REVERT table, mkfs revert, commits, plan-doc.
- Fix files (KEEP): kernel/src/syscall.rs (timerfd/eventfd/signalfd flag threading; wait4/waitid/nanosleep
  block-on-poll), servers/vfs/src/lib.rs (O_NONBLOCK_FL + 3 handlers), servers/net/src/lib.rs (net_daemon
  block-on-poll). Plan-doc draft: notes/m4e-plandoc-draft.md.

## [m4e step 8] Harness serial-corruption was masking evidence; robust launcher
- verify2 (build7): client NEVER connected — `brush /bin/clrun &` + `cat /tmp/wl.log` got serial-corrupted
  ("cat /tp/wl.lo" etc.), so no CON/ACC. anvil itself HEALTHY (clean lavender desktop+cursor, no console
  flood, screenshot m4e-v2-B). NOT an anvil failure — pure harness serial-send corruption on multi-command
  sequences.
- FIX for harness: single on-image launcher /bin/m4run (anvil bg + sleep 100 + wlclient FOREGROUND with
  stderr->serial + wait). Invoked ONCE as `brush /bin/m4run &`. Evidence via UXTR CON/ACC/SND/RCV + wlclient
  serial lines ("roundtrip done","configured -> painted","KEY code=") + screenshots. No fragile cat/tail.
  m4run added to m4-launchers + mkfs patch loop (REVERT with the rest). build8 baked it.
- RUNNING: m4e_exit_robust.py aarch64 uefi-hvf (bg bipa217nv) -> m4e-robust1.log. whole-script-is-the-bg-cmd.
- RESUME: read m4e-robust1.log + serial for UXTR ACC/SND/RCV + "configured -> painted" + KEY; view
  m4e-r-aarch64-hvf-{B,C,D,E}.png for composite + cursor delta. If good => x86_64, regressions, cleanup, commit.

## [m4e step 9] HARNESS FIX: persistent serial reader (2026-07-23)
- robust1 serial was 9KB, ZERO UXTR/markers. ROOT: QEMU serial chardev is server=on,wait=off — it DROPS
  output when no client is connected. During the 150s Python sleep NO driver.py client held the serial
  socket, so ALL UXTR during anvil+client was lost (documented feedback_qemu_serial_capture gotcha).
  anvil DID launch (screenshots pending confirm). Not an anvil failure.
- FIX: m4e_exit_robust.py now attaches a PERSISTENT serial reader thread (own AF_UNIX client to
  leandros-serial.sock, answers ESC[6n) right after `brush /bin/m4run &`, held through settle+QMP.
  screenshots use monitor sock, QMP uses qmp sock — no serial conflict (driver.py cmd used only once,
  before the reader attaches). Capture -> m4e-robust-<tag>-serial.log.
- RESUME: rerun m4e_exit_robust.py aarch64 uefi-hvf (bg). Then grep capture for UXTR CON/ACC/SND/RCV +
  wlclient "roundtrip done"/"configured -> painted"/"KEY code=". View m4e-r-*.png B/C/D/E.

## [m4e step 10] *** CRIT1 VISUALLY CONFIRMED — CLIENT COMPOSITED ***
robust1 B-client screenshot (m4e-r-B-client.png) shows the wl_shm client WINDOW composited: a
green->magenta gradient rectangle (wlclient's painted buffer) on the lavender desktop + cursor.
=> anvil ACCEPTS the wayland client + roundtrip + wl_shm buffer attach/commit + COMPOSITE all work.
The M4 accept blocker is FIXED (eventfd/timerfd/signalfd O_NONBLOCK flag fix). Serial UXTR was lost to the
QEMU-no-client-drop harness gap (now fixed with persistent reader) but the composite screenshot is direct
proof. robust2 (bd8f642gx) will corroborate UXTR ACC/SND/RCV + cursor(C/D) + key(E).
REMAINING M4 exit: cursor delta across C/D, key reaches client (KEY code= / window color change on E).
Then: x86_64, regressions both arches (vfstest FIRST), cleanup, commits, plan-doc.

## [m4e step 11] CRIT1+CRIT2 CONFIRMED (robust1 screenshots)
- CRIT1 composite: PASS (B: green->magenta gradient client window on desktop).
- CRIT2 cursor delta: PASS (B cursor top-left -> D cursor center-right after QMP tablet move to 26000,20000;
  window still composited).
- CRIT3 key: E==D (no visible window color change). wlclient bumps color_index per key press (palette base),
  so key-reached would change the base color. robust1 serial lost => inconclusive. robust2 (persistent reader)
  will show "[wlclient] keyboard focus ENTER" + "[wlclient] KEY code=". CAVEAT: pointer sits OFF the window in
  D/E; if anvil uses pointer-based kbd focus, move cursor OVER the window (~370,470) before injecting keys.
- Awaiting robust2 (bd8f642gx). Then: fix CRIT3 if needed, x86_64, regressions, cleanup, commits, plan-doc.

## [m4e step 12] aarch64 EXIT VERDICT (robust2 persistent serial + screenshots)
- CRIT1 roundtrip+composite: PASS x2 — UXTR ACC fired (anvil accepted client), UXTR SND/RCV data flow,
  wlclient serial: "roundtrip done"+"shm buffer created (480x320)"+"configured -> painted (color 0)";
  screenshot B = composited gradient window.
- CRIT2 cursor via QMP tablet: PASS — cursor B(top-left)->D(center-right).
- CRIT3 key: client bound keyboard ("seat has keyboard") but NO "keyboard focus ENTER"/"KEY code" — window
  never got keyboard focus (pointer was off-window). FIX: move pointer over window (tablet 9600,19660) +
  click (anvil sets kbd focus on button-press) THEN key -> expect "keyboard focus ENTER"+"KEY code" +
  window base-color change (color_index++). Rerunning m4e_exit_robust.py (CRIT3 focus-click added).

## [m4e step 13] Parallel: robust3 (aarch64 CRIT3 focus-key, bg b7ah277bm) + x86_64 build (bg b94zt3v5i)
- robust3 -> m4e-robust3.log: expect "keyboard focus ENTER"+"KEY code=" after focus-click; E-key window
  color change (color_index 0->2). If KEY still absent -> investigate anvil focus-on-click (smithay
  PointerHandle button -> keyboard set_focus).
- x86_64 build -> m4e-build-x86.log (fixes apply to both arches via shared kernel/vfs code; m4run baked).
- WHEN RE-INVOKED: check BOTH. If x86 build done -> run m4e_exit_robust.py x86_64 uefi (TCG, budget
  wall-clock, maybe longer settle). If robust3 KEY good -> aarch64 M4 EXIT COMPLETE.
- THEN: fresh-image regressions BOTH arches (vfstest FIRST; watch timertest/polltest/waittest re nanosleep
  now blocking), KEEP/REVERT cleanup + mkfs revert, commits, plan-doc from notes/m4e-plandoc-draft.md.

## [m4e step 14] CRIT3 window-focus fix (find_window)
- robust3: window at top-right (837,199), my hardcoded click (375,480) MISSED it -> no focus, no KEY.
  Window placement is per-run nondeterministic but stable within a run.
- Added find_window() to m4e_exit_robust.py: parses B ppm, density-thresholds the gradient window vs
  lavender bg, returns center -> tablet coords. Validated on robust3 B: (837,199)->tablet(21426,8150). CRIT3
  now moves+clicks the REAL window center for keyboard focus, then keys.
- Running robust4 (aarch64) for full exit incl CRIT3. Then x86_64 exit (image built m4e-build-x86, TCG).

## [m4e step 15] CRIT3 still failing after correct window-focus click -> evdev trace
- robust4: find_window located window (371,473), E0 screenshot CONFIRMS cursor over window center, click
  fired, but STILL no "keyboard focus ENTER"/"KEY code", color stays 0. anvil on_pointer_button DOES set
  kbd focus (input_handler.rs:227 update_keyboard_focus). So click-button likely not reaching anvil as
  PointerButton, OR key/button not reaching evdev at all.
- evdev design is correct: event0=keyboard, event1=tablet(pointer, BTN_LEFT, no INPUT_PROP_DIRECT).
- Added EVKTRACE (servers/evdev push_event, EV_KEY only, low volume): "EVK dev=<id> code=0x<code> val=<v>".
  Rebuild+run: QMP key 'a' -> expect EVK dev=0 code=0x1e; QMP click -> expect EVK dev=1 code=0x110. Absence
  localizes the break (virtio driver / QEMU routing). NOTE: M4 ACCEPT BLOCKER (the mission) is FIXED+proven
  by CRIT1(composite)+CRIT2(cursor); CRIT3 is an input-routing detail independent of the eventfd/timerfd fix.

## [m4e step 16] EVK diagnostic run in flight (b25pr70uf -> m4e-crit3diag.out / m4e-robust5.log)
- Combined child: waits for EVK build, runs m4e_exit_robust.py aarch64, greps EVK dev= + KEY/focus.
- INTERPRET: QMP key 'a' should log "EVK dev=0 code=0x001e"; QMP click should log "EVK dev=1 code=0x0110".
  - Both present but no focus/KEY -> anvil-side focus bug (unlikely, code sets focus on PointerButton).
  - key present, button absent -> virtio-tablet button not delivered (QEMU routing / virtio driver).
  - both absent -> QMP input not reaching virtio at all.
- REMINDER: M4 accept-blocker FIXED+PROVEN (CRIT1 composite + CRIT2 cursor). CRIT3 is input-routing, separate.
- PENDING after CRIT3 resolution: x86_64 exit run (image built), regressions both arches (vfstest FIRST),
  KEEP/REVERT cleanup (incl EVKTRACE=false), mkfs revert, commits, plan-doc from m4e-plandoc-draft.md.

## [m4e step 17] CRIT3 verdict + INPUT_PROP_POINTER attempt
- EVK PROVES: QMP click -> EVK dev=1 code=0x110 (BTN_LEFT) AND keys -> EVK dev=0 code=0x1e/0x30 (KEY_A/B)
  BOTH reach kernel evdev on the correct devices, AFTER client painted. So kernel/input path is SOUND;
  the CRIT3 break is anvil-side keyboard-focus-on-click.
- HYPOTHESIS: EVIOCGPROP returned all-zero -> tablet lacked INPUT_PROP_POINTER -> libinput may not deliver
  BTN_LEFT as a pointer button -> anvil on_pointer_button never fires -> no focus. FIX: EVIOCGPROP now
  returns INPUT_PROP_POINTER (bit0) for the tablet (event1). Legitimate correctness fix (real abs pointers
  set it). Testing via combined build+run (robust6).
- If still no focus after this: CRIT3 documented as anvil-side focus limitation with QEMU virtio-tablet;
  keys VERIFIED reaching compositor input stack (best-available evidence); mission (accept blocker) already
  proven via CRIT1+CRIT2. Proceed with ladder regardless.
