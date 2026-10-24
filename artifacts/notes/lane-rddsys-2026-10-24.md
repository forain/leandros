# lane/rddsys (L3.7) — 2026-10-24

## Status: FIXED (spin cut + guard); laptop result below

## Commits
- cd5ca0a4 sysstat: per-ioctl-request (0xE000|sub; DRM 'd' = 0x800|nr, 0xE86C = I915_GEM_WAIT) and user page-fault (0xfffb) rows. Guard: proctest `sysstat_subrows` (main lacks the rows, so the case cannot pass there; PASS here, 3/3 in regress).
- bda95b31 drm: `wait_fences()` spin bounded by `drm.fencespin=<us>` (default 20, was a fixed 200; 200 reproduces the old behaviour). `/proc/sysstat` gains `f waits spin_hits spin_ns sleep_ns spin_max_ns spin_us`.
  Test hook `i915.fake_lat_us=<us>` (fake device only): a soft-engine fence reads pending for that long after submit. New suite `i915fence` (x86_64) = i915test with fake_lat_us=2000; case `fence_wait_spin_bounded` (spin ns/wait <= 60 us).

## Evidence
- Laptop baseline (rddsys1, YouTube BBB): RDD 14.7 % CPU / 8.9 % sys; GEM_WAIT 120/s, 117 us on-CPU of 422 us wall (1.40 %). Census sum == RDD sys_ns, so there is no "unaccounted" sys time.
- Guard: i915fence with `drm.fencespin=200` -> RC=1, only `fence_wait_spin_bounded` fails; default -> PASS. Fake latency broke no other i915test case.
- aarch64 regress PASS (90 RC=0), x86_64 i915fake PASS.

## i915 completion interrupt (quick evaluation, not built)
The i915 driver has no interrupt path: MSI is never enabled (mod.rs "no MSI / display interrupt"), exec.rs masks the engine GT IMR bits at init, and retirement is the 200 us poll thread (`i915.poll_us`) + gpu_fence_irq_hint. Using MI_USER_INTERRUPT / the execlist context-switch interrupt would need: MSI enable on 00:02.0, an ISR (MASTER_IRQ off, read+ack GT IIR banks 0/1, CSB processing or COMPLETED reread, `syncobj_retire_fence`-style lock-free wake), unmasking the user/CS bits per engine, and MI_USER_INTERRUPT after the breadcrumb store in the request tail. It is the right end state (wake latency ~poll/2 -> ~us, poll thread could stop when idle), but it is a multi-day lane with real-hardware-only validation, not a trivial change.

## Open
- recvmsg 36 us + sendmsg 31 us/call (Firefox IPC, SCM_RIGHTS) and the close/dup churn; PRIME_HANDLE_TO_FD 84 us/call.

## Laptop (trial 8d82bc8bee10 = main e3ea2a51, deskplay phase1, 3 valid runs, YouTube BBB; PROMOTED good, cmdline iwlwifi.mgr=1)
| | baseline rddsys1 | rddsys2 (r1/r2/r3) |
|---|---|---|
| RDD cpu / sys % | 14.7-14.9 / 8.9-9.0 | 14.3-14.5 / 8.4 |
| RDD census % | 11.7-11.9 | 10.3-10.4 |
| GEM_WAIT on-CPU / wall us | 117 / 422 | 36 / 443-450 |
| RDD ioctl total | 59 us/call, 2.83 % | 41 us/call, 1.95 % |
| total desktop CPU % | 152-156 | 150-154 |
| flips/s, exec_vcs/s | 52.8, 60 | 52.5-53.2, 60 |
| cosmic-comp % | 18.2 | 18.2-18.6 (flat) |
`f` line: 5880 waits in 31 s, 0.4 % finish inside the spin, spin 20.2 us/wait. Wake latency +~22 us/wait (wall). Heartbeat 323 lines, 0 gaps > 2 s.
Incident: between the run and promote another lane's trial (kid 9a01052102e7, kernel 1aa27127 = older main) was booted, and the first `promote` promoted it; fixed by redeploying 8d82bc8bee10 with `--cmdline iwlwifi.mgr=1 --promote` (good=8d82bc8bee10). Keep deploy+measure+promote inside ONE `lock.sh laptop` hold.
