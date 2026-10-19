# lane/kexeclaptop (2026-10-19)

First kexec LeandrOS→LeandrOS on real hardware: HP EliteBook 840 G5
(i5-8350U 4c/8t, 16 GiB, Samsung PM981 NVMe, two xHCI (00:14.0 holds the USB
root stick, 3a:00.0 Thunderbolt), UHD 620 (i915), iwlwifi 8265, HDA CX20632,
I219 e1000e). Reachable only over Wi-Fi.

Tree: main 076011b6, release `build-all.sh --arch x86_64`
(`LEANDROS_KERNEL_TAG=kexec-076011b6`). No code changes in this lane.

## Procedure
1. `remote.py deploy-kernel --netwatch 180 --reboot`: trial e26db6bdc41a,
   `#1 kexec-076011b6`. This is gen0 (Limine).
2. Pushed `target/final-x86_64/kernel-direct` → `/boot/kexec`,
   `initrd-x86_64.cpio` → `/boot/kexec-initrd`, and the userland `kexec`
   → `/bin/kexec`. The stick's root fs had no `/bin/kexec` and no `/boot`.
   sha256 matched in the guest.
3. `kexec -l /boot/kexec --initrd=/boot/kexec-initrd --reuse-cmdline --append=kexec.gen=N`,
   then `nohup sh -c "sleep 3; kexec -e [-c CPU]" &`. Waited for sshd.

## Results

| gen | initiator | sshd back (from the `kexec -e` command) | nproc | MemTotal | cmdline |
|---|---|---|---|---|---|
| 0 (Limine) | - | - | 8 | 16437284 kB | - |
| 1 | `kexec -e` (cpu0 path) | ~19 s (uptime 15 s) | 8 | 16583136 kB | `kexec.gen=1` |
| 2 | `kexec -e -c 3` | ~19 s | 8 | 16583136 kB | `kexec.gen=2` only (no accumulation) |
| 3 | `kexec -e -c 5`, `kexec.fbbm=0` | ~18 s | 8 | 16583136 kB | `kexec.gen=3 kexec.fbbm=0` |

In gen3, the tool's own log (written to /root before reboot) said
`kexec: reboot(KEXEC) from cpu5`, so the AP→NMI→BSP hand-off works on real
silicon. Every gen printed `[SMP] 8 CPUs online (xAPIC)` with the same
cpu:apic map as Limine, and `direct HHDM: RAM to 0x43F800000` (all 16 GiB,
gbpages). The direct kernel reports 146 MB more MemTotal than the Limine
boot, probably memory Limine/EFI kept reserved.

Devices in every kexec'd gen, compared with gen0:
- **NVMe**: `controller already disabled` (the CC.EN=0 hook worked), ready in 12 ms, SMART ok.
- **xHCI**: bus 1 had 4 devices and the mass-storage stick was re-enumerated. The root f2fs
  mounted (the previous gen logged a clean unmount checkpoint) and espctl read the ESP.
  Bus 2 had 0 devices, the same as gen0.
- **i915**: probe ok, GOP scanout on eDP pipe A matched the boot framebuffer,
  GT reset + `G3b exec ready` (rcs0/bcs0/vcs0). cosmic-comp + greeter ran with
  GPU exec ~38/s and flips ~1.9/s, the same numbers as gen0. Nobody looked at
  the panel, so nothing was checked visually. kexec.fbbm=0 (gen3) behaved the same.
- **iwlwifi**: firmware INIT+runtime ALIVE, associated in ~130 ms, DHCP got the same address.
- **HDA**: codec 14f1:2008 probed, stream 7 running at 48 kHz (pipewire).
- **e1000e**: up, no link (no cable), the same as gen0.
- No panics, no new error lines. Greeter log noise is the same as on a Limine boot.

## Observations / open
- The `[KEXEC]` per-function devreset lines and the `stopping ... from cpuN`
  line are printed **after** the klog final flush. The laptop has no serial
  port, so they never reach the log partition and were not seen on real
  hardware. The last persisted line is `[REBOOT] phase 1 done ...`. To see
  them: hand a small results buffer to the next kernel (bootinfo or a reserved
  page), or do a second klog flush after `devreset::execute` (a
  polled block write with interrupts off, which is risky).
- i915 display interrupts survive the jump: the next kernel finds
  `MASTER_IRQ=0x80000000`, DE pipe A `IER=0x9` (gen0 had 0/0). The i915
  kexec hook stops the rings and resets the GT but does not mask display IRQs.
  It was harmless here, because interrupts stay off until the new driver sets
  them up again. A one-line mask in the hook would make the hand-off cleaner.
- KVM and other real hardware (the PC with NVIDIA as framebuffer owner) are untested.

Laptop left on its good kernel main-cfe62bbf (d3c90702a894). The trial was
discarded, and /boot/kexec*, /bin/kexec were removed again.
Redacted logs: artifacts/remote-logs/20261019-1431..1437-172.16.149.179 (not committed).
