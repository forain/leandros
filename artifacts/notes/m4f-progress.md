# M4f FINISHER progress

## [m4f step 0] Bootstrap (2026-07-23)
- Read m4e-progress.md steps 8-17. State confirmed:
  - CRIT1 composite PROVEN (aarch64), CRIT2 cursor PROVEN, CRIT3 keys reach kernel evdev (EVK trace) but
    anvil-side focus quality is a judgment call.
  - Tree: 8 modified files uncommitted. KEEP/REVERT table in task brief.
  - robust6 = latest aarch64 run WITH INPUT_PROP_POINTER fix (notes/m4e-robust6.log).
- TASKS: (1) CRIT3 verdict from screenshots; (2) x86_64 rebuild+exit TCG; (3) cleanup+regressions both arches;
  (4) commits; (5) plan-doc rewrite; (6) final report.
- RESUME: currently examining robust6 log + E0/E screenshots for CRIT3 verdict.

## [m4f step 1] CRIT3 VERDICT (aarch64) — recorded
- Viewed robust6 B-client / E0-focusclick / E-key (INPUT_PROP_POINTER build). All THREE pixel-identical:
  color 0 green->magenta gradient. Window NEVER changed color after keys.
- wlclient kb_key bumps color_index + redraws on every PRESSED key (color 1=green,2=red). Unchanged window
  => client did NOT receive wl_keyboard key events. No "keyboard focus ENTER" in serial either.
- BUT EVK trace (crit3diag.out) PROVES keys reach kernel evdev: dev=0 code=0x1e/0x30 (KEY_A/B), dev=1
  code=0x110 (BTN_LEFT). Kernel input path SOUND.
- VERDICT: CRIT3 = keys reach kernel evdev (proven); client-side key delivery NOT achieved because anvil
  never grants keyboard focus to the surface on click (anvil-side focus behavior, downstream of + independent
  from the M4 accept-blocker). Mission (accept blocker) fully proven by CRIT1 composite + CRIT2 cursor.
  Determinable from existing evidence — NO extra aarch64 run spent (disciplined: question already answered).

## [m4f step 2] x86_64 rebuild + exit run (TCG) IN FLIGHT
- Rebuilt x86_64 via build-all.sh --arch x86_64 (m4f-build-x86.log, exit0, "Build Complete!"). Fresh images
  12:36: leandros-limine/f2fs-data0/data1 x86_64. m4run launcher baked (mkfs patch present).
- Gates in tree: UXTRACE=true, EVKTRACE=true, PARKTRACE/LSNTRACE=false, INPUT_PROP_POINTER present.
- Adapted script m4f_exit.py (settle arg, m4f- serial capfile, longer QMP settles, wider evidence grep).
- LAUNCHING: m4f_exit.py x86_64 uefi 600  (bg). TCG slow -> 600s settle. Whole-script-is-the-bg-cmd.
- RESUME: read notes/m4-screenshots/m4f-x86_64-tcg-serial.log + the run's stdout log; view
  m4e-r-x86_64-tcg-{B,C,D,E0,E}.png. Expect UXTR CON/ACC + wlclient "roundtrip done"/"configured->painted"
  + composite window (CRIT1), cursor delta (CRIT2), EVK dev= keys (CRIT3 kernel path). Note wall-clock.

## [m4f step 2b] x86_64 TCG run HEALTHY — CRIT1 proven mid-settle (12:41)
- Serial capture (clean, uncorrupted on x86): UXTR CON, **UXTR ACC pid=9** (anvil accepted client!),
  SND/RCV flow, wlclient "roundtrip done"/"shm buffer created"/"configured -> painted (color 0)",
  [MMAP] DynamicDevice framebuffer maps (compositing). Boot to brush was <120s under TCG (no boot-timeout).
- The accept-blocker fix works on x86_64 too. Waiting for settle->screenshots B/C/D/E0/E (~12:50).
- DO NOT start cleanup/rebuild until x86 screenshots captured (rebuild regenerates images + reverts mkfs
  launcher, so no further anvil run possible after). Hold edits.

## [m4f step 2c] x86_64 TCG EXIT COMPLETE — DONE, mirrors aarch64
- CRIT1 accept+composite PROVEN: serial UXTR CON->ACC pid=9->SND/RCV, wlclient "roundtrip done"/
  "configured -> painted (color 0)"; find_window located composited window (445,529); B-client.png shows
  green->magenta client window on lavender desktop (1920x1080 x86 display).
- CRIT2 cursor PROVEN: B(top-left)->D(center-right ~1530,670) via QMP tablet, window still composited.
- CRIT3: EVK dev=1 code=0x110 (BTN_LEFT), dev=0 code=0x1e/0x30 (KEY_A/B) reach x86 kernel evdev; E-key
  window UNCHANGED color 0 (client focus not granted — SAME anvil-side behavior as aarch64).
- Wall-clock: boot->shell <120s; anvil composite+paint ~1-2 min after launch under TCG (softpipe usable,
  NOT the feared wall). Screenshots m4e-r-x86_64-tcg-{B,C,D,E0,E}.png; serial m4f-x86_64-tcg-serial.log.
- QEMU stopped. Proceeding to CLEANUP (KEEP/REVERT), rebuild both, regressions.

## [m4f step 3] CLEANUP DONE + rebuilding both arches
- Reverted wholly-diagnostic/scaffolding: sched/src/lib.rs, kernel/src/init.rs, arch/aarch64/src/exception.rs,
  scripts/mkfs-f2fs-populated.py (git checkout). Launcher sources preserved in m4-launchers/.
- Partial edits: net (removed LSNTRACE/klog4/call-site, kept net_daemon block-on-poll); evdev (removed
  EVKTRACE, kept INPUT_PROP_POINTER); syscall.rs (removed park_enter/exit calls, SYSTRACE block, EXEC/OPEN
  logs, PARK table+fns, 3 MKFD blocks; flipped UXTRACE=false; kept uxtrace fn+4 calls, flag threading,
  block-on-poll). Grep confirms ZERO residual diagnostic refs.
- Remaining diff: 4 files (syscall.rs, vfs, net, evdev) = KEEP fixes + gated uxtrace only.
- Rebuilding BOTH arches (build-all.sh) -> m4f-build-both.log. Fresh images for regressions.
- RESUME: after build, run m4f_regress.py aarch64 uefi-hvf, then x86_64 uefi (TCG, slow). Then commits +
  plan doc + final report.

## [m4f step 4] Both-arch rebuild CLEAN (Build Complete!, both compile) — fresh images 12:54/12:55
- Launching aarch64 regression (HVF) -> m4f-regress-aarch64.log. Then x86_64 (TCG). vfstest FIRST.
- Commits held until regressions green. Commit-split plan: syscall.rs diff is now purely 3 KEEP concerns
  (flags/block/uxtrace) -> git apply --cached hunk-split into 4 logical commits.

## [m4f step 5] aarch64 regression GREEN (HVF)
- vfstest 34/34 (FIRST, fresh img), drmsmoke 20/20, scmtest 19/19, epolltest PASS, evtest2 PASS,
  polltest PASS, sigtest PASS, timertest PASS (nanosleep-block safe), idletest pass=2/fail=0,
  kmscube -D ANIMATING (FRAME_DIFF 148557). 84 PASS lines, only 1 TIMEOUT (kmscube launch, expected).
- ONLY failure: waittest wait_on_process_group. VERIFIED PRE-EXISTING FLAKE via m4f_waitcheck (5 runs, same
  boot/binary): PASS,FAIL,PASS,FAIL,FAIL = intermittent race (deterministic break would fail 5/5).
  blocking_wait_for_exit PASSes consistently => wait4/waitid block-on-poll core path sound. Pre-authorized.
- evtest2 no_INPUT_PROP_DIRECT checks bit1; INPUT_PROP_POINTER=bit0 => unaffected (verified source line 174).
- Launching x86_64 regression (TCG, slow) -> m4f-regress-x86.log. Commits held until x86 green.

## [m4f step 6] x86 regression GREEN + COMMITS DONE — M4 COMPLETE
- x86_64 regression (TCG): vfstest 34/34 FIRST, drmsmoke 20/20, epolltest/evtest2/polltest/sigtest/
  timertest all done+PASS, idletest pass=2/fail=0, 87 PASS lines. Only failure = same wait_on_process_group
  flake (blocking_wait_for_exit PASS). kmscube: setsid missing on image -> re-verified setsid-free:
  "Rendered 6 frames in 2.36s (2.5fps)" = DRM render+present path WORKS on x86 (screenshot-diff STATIC was
  capture-timing artifact: kmscube exited before shots under slow TCG; drmsmoke 20/20 corroborates).
- COMMITS (5, no Claude mentions), main now cb2fa61..a52b994:
  cb2fa61 kernel/vfs: honor O_NONBLOCK on eventfd/timerfd/signalfd creation
  48e38c6 kernel/net: block instead of busy-polling in wait4/waitid/nanosleep and net_daemon
  e92f22b evdev: report INPUT_PROP_POINTER for the virtio-tablet
  535eb07 kernel: add a gated unix-socket exchange trace (off by default)
  a52b994 docs: record M4 done — accept + composite + cursor proven both arches
- git status CLEAN; git diff HEAD empty (HEAD == built+tested tree). Plan doc (repo) + memory both updated.
- M4 COMPLETE. STOP (do not start M5).
