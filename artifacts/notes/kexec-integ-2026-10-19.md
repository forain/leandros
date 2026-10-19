# kexec phase 2 integration (2026-10-19)

Tree: main 7c59caea (detached), release build-all, Mac.

- build-all.sh: OK, both arches.
- regress suite: aarch64 PASS (28/28 RC=0), x86_64 PASS (all RC=0). No known-red cases hit.
- kexec chain (Limine -> `kexec -l /boot/kexec --initrd=/boot/kexec-initrd --reuse-cmdline --append=kexec.gen=N`, `kexec -e`; gen2 via `kexec -e -c 2`):
  - aarch64 HVF, 4 CPUs: gen0/1/2 reach login, nproc 4, MemTotal ~4.1 GB; gen1 stopped from cpu1, gen2 from cpu2 (`[KEXEC] stopping 3 other CPU(s) from cpuN`, `cpu0: other CPUs off; purgatory ... -> entry`); `[SMP] 4 CPUs online` each gen; /proc/cmdline `kexec.gen=1` then `kexec.gen=2` (single word, no accumulation).
  - x86_64 TCG, 4 CPUs, 6 GiB: same, nproc 4 (xAPIC), MemTotal 6257616 kB; gen1 `from cpu0 (BSP)`, gen2 `from cpu2 (AP): NMI to the BSP`, `BSP cpu0: INIT sent to 3 CPU(s)`; cmdline `kexec.gen=2` only.
  - device reset lines: virtio status-reset only (0 functions hard-reset, none "not back"); 00:1f.2 AHCI not reset (no FLR/PM, no parent bridge).
- `--direct` (driver direct mode): x86_64 4 CPUs (MemTotal 4.1 GB), aarch64 4 CPUs, both reach login.
- KVM (linux desktop 172.16.158.150, also tried Tailscale 100.106.210.103): NOT RUN, ssh port 22 timed out from the Mac (no ping reply). Open.
