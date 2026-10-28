# lane piinit 2026-10-28 (items 4, 5, bthid without hci0) - FIXED

- init: `gpu_precheck_refused()` runs `/bin/sh -c '. /bin/gpu-env ...'` (120 s bound, skipped when /proc/nvrm_state is not "absent" or dm.precheck=0); exit 78 prints the NO GPU banner, no greetd, no sweep. gpu-env itself is unchanged (it is sourced, not executed; the `--check` flag of the design became this wrapper).
- init: bthid starts without /dev/hci0 (`start_bthid_early`), btd still needs hci0.
- userland/grep: grep/egrep/fgrep shim over /bin/rg (7 host tests). populate prints MISSING optional and honours LEANDROS_REQUIRE; pi_sync requires rg,grep,dmesg,coreutils (LEANDROS_PISYNC_ALLOW_MISSING=1 overrides; no remote.py flag). /etc/profile PATH gains sbin dirs. build-all builds grep.
- Evidence: aarch64 full regress PASS, guest grep test PASS, x86_64 box regress only icmp_unbound FAIL (nettag2 flake family), Pi trial: grep/dmesg work, banner once, no greetd/stray, btd+bthid+sshd up.
- Not verified: positive GPU path (no GL-capable QEMU here); /dev/dri stub on the Pi exists and precheck correctly refuses.
- Shared ports/greetd artifact goes stale when lanes race (run ports/greetd/build.sh).
