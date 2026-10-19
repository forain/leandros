# lane/kexecapcpu (2026-10-19)

Two things: x86_64 `reboot(KEXEC)` issued from a CPU other than the BSP
(never exercised before), and the CPU count of a plain x86 `--direct` boot.

## 1. kexec from an AP

No kernel bug found: the existing hand-off (exec_x86_64.rs: the AP arms
`NMI_DIVERT`, NMIs the BSP and halts; the BSP INITs every other CPU, resets
PCI, jumps) worked on every run. What was missing was a way to trigger it
deterministically:

- `getcpu(2)` now reports the real CPU (`sched::cpu_id()`); it returned 0
  unconditionally before. Node is still 0.
- `kexec -e -c CPU` (`--cpu=CPU`): `sched_setaffinity` is admission-only
  (there is no CPU affinity in the scheduler), so kexec sleeps/yields with
  varying intervals until `getcpu` reports CPU, prints
  `kexec: reboot(KEXEC) from cpuN`, then calls reboot. 30 s cap. The kernel
  log line `[KEXEC] stopping ... from cpuN (AP)` is the authoritative
  initiator (a migration between getcpu and the syscall is possible; it
  never happened in these runs).
- `[KEXEC] BSP cpuN: INIT sent ...` now names the CPU that does the jump.

Test images: APFS clones of main's images, this lane's Limine kernel in
the ESP clone, kernel-direct on a raw NVMe disk, the new kexec binary on a
second raw NVMe disk (the guest's own kexec is main's). Scripts:
scratchpad `acpu/{kbuild,mkimg,chain}.sh`, same pattern as lane
kexecdevreset.

| run | initiators | result |
|---|---|---|
| x86_64 TCG, q35 4 CPUs, idle | cpu1, 2, 3, 1, 2 (5 chained) | 5/5: AP NMI -> `BSP cpu0` jump, every gen `[SMP] 4 CPUs online`, login, cmdline `kexec.gen=1..5` |
| x86_64 TCG, 4 x `yes` busy loops (BSP in user mode) | cpu2, 3, 1 | 3/3, 4 CPUs each |
| aarch64 TCG | cpu1, 2, 3 | 3/3: `cpu0: other CPUs off`, 4 CPUs each |
| aarch64 HVF (Mac) | cpu1, 2, 3 | 3/3, 4 CPUs each (main's HVF AP mailbox fix holds) |

Residual risk, not fixed: the x86 NMI gate has no IST stack. An NMI that
lands in the few instructions of the SYSCALL entry before the kernel stack
switch would push its frame on the user stack (SMAP -> #PF -> #DF), and the
kexec would hang after the AP's 2 s wait. Not seen; the watchdog NMI probe
has the same exposure. A fix is an IST for vector 2 (the probe's frame
walk would need to read the interrupted RBP from the frame instead).

## 2. Plain `--direct` (QEMU multiboot1) gets all CPUs

`boot::acpi::scan_legacy_rsdp`: EBDA first KiB (segment at 0x40E, only when
it lies in 0x80000-0xA0000), then 0xE0000-0xFFFFF, 16-byte steps, ACPI 1.0
checksum, and for revision >= 2 the extended checksum over Length.
`find_rsdp_in` is the pure part (host test `legacy_rsdp_scan` in
boot/bootparse-tests, 12/12). Called from `kernel_main` only on a
non-Limine x86 boot whose protocol passed no RSDP (multiboot1; multiboot2
from kexec and PVH carry one).

QEMU `-kernel` (SeaBIOS, multiboot1), q35 `-smp 4`:
`[early] no RSDP from the boot protocol; legacy BIOS scan: 0xF52C0`,
`[CPUS] BSP apic=0, MADT lists 4 enabled CPU(s)`, `[SMP] 4 CPUs online`
(was 1 CPU, `no ACPI MADT`). An OVMF-only path would have no legacy RSDP,
but QEMU `-kernel` on x86 always goes through SeaBIOS.

## Not tried
KVM / real hardware AP-initiated kexec (laptop): the path is the same, but
x2APIC (the laptop) was not exercised here (TCG ran xAPIC).
