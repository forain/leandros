# lane/vtclock 2026-10-23 (L11.6 remainder, L11.7 verification)

## (1) EVIOCSCLOCKID applied -- FIXED
`servers/evdev`: the ioctl now stores `id+1` (0 = never set = monotonic, the
historical default; Linux itself defaults to REALTIME but libinput always sets
its clock) and `VFS_READ` re-stamps the drained chunk (`restamp`): CLOCK_REALTIME
adds the current realtime-minus-monotonic offset; BOOTTIME == MONOTONIC (nothing
suspends). Caveat: stored per device node, not per open (Linux: per open).
Guard: `/bin/evtest2 clock` + `run-leandros/evclocktest.py` (QMP motion, compares
the first stamp with clock_gettime of the selected clock, < 50 ms).
- Before: `clock_realtime_diff_us=1792776673005573` FAIL (monotonic 4175 us PASS, boottime 1897 PASS)
- After:  realtime 9271 us PASS, monotonic 3448 us PASS, boottime 7764 us PASS (aarch64 TCG/HVF Mac).

## (2) "clicks ignored while the pointer moves" -- REFUTED at evdev
`/bin/evtest2 btn` + `evbtntest.py [clicks] [gap_ms]`: each click = QMP batch
[abs x, abs y, btn down] then [abs x+50, abs y+50, btn up] (button in the SAME
input-send-event batch as motion). 100 clicks at 5 ms and at 20 ms gaps:
downs=100 ups=100 frames=200 SYN_DROPPED=0 both runs. Nothing is dropped or
merged in virtio-input -> evdev. (An early 57% "loss" was two harness bugs: the
reader window counted wakeups not time, and the sender started before the reader
opened.) The remaining suspect is compositor side (cosmic-comp/libinput).

## (3) VT-switch with COSMIC session -- BLOCKED (not verified)
`ffsession.py aarch64 ... --gpu virgl` and `driver.py start aarch64 --virgl` on
this Mac boot the guest but `gpu-env` logs `virgl probe failed (rc=139
renderer='')` (the probe SIGSEGVs) and refuses to start COSMIC ("NO GPU
RENDERER"). virglarm (10-20) had a working virgl boot, so something since broke
the probe on aarch64 (candidates in between: panfix2 strict PAN, gpuq2 RING_IDX
refusal, tmpfsmmap). Repro: `driver.py start aarch64 --virgl; login root root;
/bin/gpuprobe gl; echo $?`. vt_handoff stale_presents=0 vt_restores=0 on the
console-only boot (no switch exercised). Added `ffsession.py --hostpost CMD`
(host-side command with the session live) for the chord/screenshot loop once the
probe works again.
