# lane/kexeclogverify (2026-10-19)

Real HP EliteBook 840 G5, kernel 367ea81c (`kexec-367ea81c`), release x86_64.
Same procedure as lane-kexeclaptop: trial deploy, stage kernel-direct + initrd + /bin/kexec,
gen1 = `kexec -l ... --append=kexec.gen=1`, `kexec -e -c 3`. sshd back within ~40 s.

(a) The new kernel's log has 37 `[prevkexec]` lines, including the per-device `[KEXEC]` results.
Reset method per device:
- 00:1f.6 I219 e1000e: AF FLR
- 01:00.0 iwlwifi 8265: FLR
- 3a:00.0 Thunderbolt xHCI: D3hot->D0
- 3b:00.0 NVMe: FLR (plus driver hook CC.EN=0)
- 4 functions reset, 0 not back, 115 ms
- Not reset: 00:02.0 i915 (boot framebuffer, driver hook only), 00:14.0 xHCI (no FLR/PM reset, no parent bridge; driver hook only), 00:1f.3 HDA (same; driver hook only), 04:00.0 and the other class-not-reset functions.
- The driver hooks (xHCI HCRST, HDA CRST, e1000e, iwlwifi, i915 GT reset) are logged as well.

(b) i915 `kms: vblank: before:` shows MASTER_IRQ=0x00000000 and pipe A IER=0x00000000. The hook
logged 0x80000000 -> 0 and 0x9 -> 0.

(c) nproc 8, MemTotal 16583116 kB, wifi0 carrier up, greetd + cosmic-comp running,
GPU exec ~38-42/s and flips ~2/s as before. No regression, so no gen2.

End state: files removed, trial discarded, rebooted to good d3c90702a894 (main-cfe62bbf).
