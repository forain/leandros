# lane/kexecsmap (2026-10-23) - L7.4 kexec remainder VERIFIED

x86_64 TCG release, main deed9432 (SMEP/SMAP on). Chain: `kexec -l ... --reuse-cmdline
--append=kexec.gen=N`, `kexec -e` (gen1), `kexec -e -c 2` (gen2), `kexec -e -c 3` (gen3).
- gen1, gen2, gen3 all booted and logged in (`/proc/cmdline` = kexec.gen=1/2/3), 4 CPUs each.
- Added boot line `[CPU] CR4 SMEP=<b> SMAP=<b>` (arch/x86_64/src/smp.rs, before "[SMP] N CPUs online").
  Seen 4 times (gen0-3): `SMEP=1 SMAP=1` every generation.
- 0 "SMAP violation"/"SMEP violation"/"KERNEL PANIC" lines in the serial log.
- No purgatory/entry/AP change needed: the defensive CR4 clear works.
