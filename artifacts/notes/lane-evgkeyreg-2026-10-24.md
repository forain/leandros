# lane/evgkeyreg (L11.13) - evgkeytest red in full regress - 2026-10-24

Status: FIXED (test race; product NOT regressed). Commit e6acabb3.

## Root cause
evgkeytest.py started QMP input on a fixed 10 s timer after launching `evtest2 held` over serial.
When driver._type_line misses the prompt (a kernel log line such as `[fslock] F2FS_MOUNTS` lands
on the prompt repaint - visible in tests-nettx-reg/serial.log right before the evgkey command),
it waits its 10 s echo timeout, ^C, and retypes. evtest2 then opens+drains event1/event0 after the
first BTN_LEFT press/release and KEY_A press/release were already sent; it sees only the
overflow-phase press, waits 20 s for a release that never comes (held_btn_bit_cleared FAIL), then
20 s for KEY_A (held_key_seen_down FAIL).

## Evidence (aarch64, HVF)
- Timing signature: both red runs (nettx-reg, hhdmw-reg) took 56.4/56.5 s. Late guest start predicts
  10+6+20+20 = 56 s; a lost release/key event (product bug) predicts ~50 s. Green runs: 22.1 s.
- Repro on unfixed test: guest delayed with `sleep 12;` -> RC=1, 56.4 s, fails exactly
  held_btn_bit_cleared,held_key_seen_down. Same delay with the fixed test -> PASS (25.0 s).
- main 1107d783 unfixed: alone PASS; evherdtest+evgkeytest x2 in one boot PASS (no left-over state).
  Bisect across d0393d5d / e0d3b688 not needed: the failure depends on serial prompt timing, not on
  udevhot/evdyn; EVIOCGKEY follows the events on event0/event1 in every run that opened in time.

## Fix
evgkeytest.py shares one _Serial with serial_run and sends input only after the guest prints
`evtest2 held: waiting` (nodes open, drained); the motion flood waits for `overflow phase`.
Missing markers are reported as FAIL. EVGKEY_GUEST_PREFIX delays the guest to exercise the path.
All evtest2 checks unchanged (still strict).

## Results
Full aarch64 regress PASS (45 suites, evgkeytest 13.0 s); evgkeytest 5x in one boot PASS; x86_64 build OK.

## Left
- evbtntest.py still uses a fixed pre-delay (same race class; not in regress).
- driver._type_line: kernel log lines on the prompt repaint cost a 10 s echo timeout for every
  serial_run; any host test that times input by wall clock is exposed.
