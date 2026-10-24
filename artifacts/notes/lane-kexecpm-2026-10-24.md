# lane/kexecpm (L8.5, laptop part) — 2026-10-24

FIXED: after a LeandrOS->LeandrOS kexec the firmware's LTR latency values were gone.

## Measurement (HP EliteBook 840 G5, `/proc/pcipm`, new in this lane)
Cold boot vs one kexec, same kernel (kexecpm-a, unfixed), 25 functions:
- Only difference: LTR max snoop/no-snoop (LTR ext cap +4) on the three FLR'd
  endpoints went `10031003` -> `00000000`: 01:00.0 iwlwifi 8086:24fd,
  3a:00.0 Thunderbolt xHCI 8086:15db, 3b:00.0 NVMe 144d:a808 (PM981).
  DevCtl2 LTR enable stayed set (devreset restores DevCtl2), so the endpoints
  ran with zero latency tolerance — that blocks deep package C-states.
- L1SS ctl1/ctl2, LnkCtl ASPM/CLKREQ, DevCtl, AER mask/severity/cap-ctl:
  identical (L1SS survives FLR here; AER is sticky). Root ports / TB bridges
  are never reset, so their state never changed.
- The hypothesis "kexec loses L1SS/AER" is REFUTED on this laptop; only LTR.

## Fix (kernel/src/kexec/devreset.rs)
plan() records the LTR (0x18), L1SS (0x1E) and AER (0x01) ext-cap offsets;
execute() saves LTR, L1SS ctl1/ctl2, AER uncor mask/severity, cor mask, cap/ctl
with the header and `restore_pm()` writes back what differs (LTR before
DevCtl2; L1SS ctl2, ctl1 without enables, ctl1, with LnkCtl ASPM off, like
Linux pci_restore_aspm_l1ss_state), then clears AER status latched during the
reset. The per-function `[KEXEC]` line says "restored LTR/L1SS/AER".

## Verification (kexecpm-b)
- Cold boot dump identical to kexecpm-a's cold dump. After kexec: all LTR,
  L1SS, ASPM and AER config equal to cold boot. Only diff: AER correctable
  status 0x2000 (Advisory Non-Fatal, latched by firmware at cold boot) is now
  cleared on 3a:00.0/3b:00.0 — intended (Linux AER init clears status too).
- NVMe after kexec: dd 64 MiB from nvme0n1 at 746 MB/s. Wi-Fi: SSH session over
  wifi0, 20 MB rx.
- Idle package power / PC-state residency: NOT measured (no RAPL/MSR interface
  exposed to userland). Open.

## Open
- No QEMU guard: in QEMU only virtio functions are touched by devreset and no
  emulated device has an LTR cap, so a q35 kexec compare cannot fail on the
  unfixed code. `/proc/pcipm` before/after a kexec on the laptop is the check.
- NVIDIA/GSP part deferred (no PC).
