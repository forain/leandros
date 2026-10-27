# lane/pikexeccpu 2026-10-27 — Pi 5 kexec keeps all CPUs (FIXED in code, Pi unverified)

Symptom (scratchpad piirq/boot.log l.7262): after kexec the new kernel printed
`[SMP] mpidr=0x100 accepted CPU_ON but never came online`, `1 CPUs online`.
The old kernel's AFFINITY_INFO wait passed (no "still not OFF"), so TF-A reported
the secondaries OFF and returned 0 for CPU_ON, then never restarted them.
Cause: the Raspberry Pi TF-A port has no real CPU power control. CPU_OFF ends in
rpi3_pwr_down_wfi (RMR_EL3 warm reset in older trees, which upstream says does not
work on BCM2711; a software hold pen in newer ones). On the Pi 5 firmware's BL31
the powered-off core does not come back. Our side was correct apart from relying on it
(cpus from the carried FDT = 0x0/0x100/0x200/0x300, enable-method psci, smc).

Fix (kernel/src/kexec/exec_aarch64.rs park_off/final_cpu0, purgatory_aarch64.s
kexec_purgatory_off, arch/aarch64/src/smp.rs smp_init, boot/src/mpidr.rs):
- rpi5 default: secondaries do not call CPU_OFF. They flush their private L1/L2
  by set/way (MMU off), store an arrival word (park page +0x900 + 8*idx), and wait
  in the existing park-page mailbox loop. `kexec.cpuoff=1|0` overrides (A/B knob).
- The kexec'ing CPU waits for every arrival word before the purgatory runs.
- Next kernel: CPU_ON returns ALREADY_ON (-4); with a posted mailbox that now
  counts as started (the mailbox branch does `ic iallu` first).
- One line per AP: `[SMP] cpuN mpidr 0x.. psci rc R (started|ALREADY_ON: kexec park mailbox|failed)`.

Evidence: QEMU aarch64 HVF -smp 4, runtests --suite serialxfer: CPU_OFF kexec 4/4
(rc 0 x3) then park-mode kexec 4/4 (rc -4 x3, mailbox_starts=3); host tests 5/5
(boot/memmap-tests psci_ap_tests); check-rpi5.sh rpi5 + raspi4b OK.

Next Pi kexec should print (old kernel) `[KEXEC] secondaries: park loop, no PSCI
CPU_OFF ...`, `[KEXEC] cpu0: other CPUs parked; ...`; (new kernel) three
`[SMP] cpuN mpidr 0x...N00 psci rc -4 (ALREADY_ON: kexec park mailbox)` and
`[SMP] 4 CPUs online`. If instead a `cpuN never reached the park page` or a
`failed` line appears, retry with kexec.cpuoff=1 to compare.
