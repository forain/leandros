# Lane netwatch2/netwatch3 (L8.9) - continuous trial watchdog - FIXED

Trial kernels (leandros.slot=trial + netwatch.* knobs) now ping the gateway continuously (kernel/src/netwatch.rs, servers/net/src/netwatch.rs),
reboot into the fallback after netwatch.reach seconds without an answer, and disarm on promote (NETCTL_NETWATCH_DISARM). deploy-kernel defaults to 180 s on real machines.

## Evidence (aarch64 QEMU, host:trialwatchtest.py, reach=40 every=5)
- Test now seeds a promoted first kernel so a `good` slot exists (the laptop situation) before the trial.
- main kernel (aa262ce7): loss case FAILS - no unreachable line, never reboots (151 s), stays on slot=trial.
- branch: setup/blip/loss/promote all PASS; loss reboot 37 s after the cut ("unreachable for 40 s": the count starts at the last answered probe, up to every=5 s before the cut), next boot slot=good. Window asserted 34..52 s.
- netwatchtest.py PASS (nonic/withnic/loss); regress aarch64 PASS (first run had a boot failure from running netwatchtest concurrently; clean rerun green).
- Not in regress: trialwatchtest takes ~10 min. Real-laptop behaviour untested here.
