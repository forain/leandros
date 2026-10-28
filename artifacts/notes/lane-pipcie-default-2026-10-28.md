# lane pipcie-default 2026-10-28
Default flipped: PCIe/RP1 on unless `pcie.rpi5=0` (kernel/src/init.rs); off logs `[pcie] off (pcie.rpi5=0)`. `rp1.irq` stays default off.
Audit: RP1 registrations (kexec Bus/Device hooks, usb_hcd pending list, macb netdev, fan) are reached only through `rp1::probe`, called only when `brcmstb_pcie::is_up() && stage(16)`; on failure nothing registers. aarch64 virt unaffected (cfg rpi5 only).

## Pi matrix (trial 8e2fd94ff6, uname -v "#1 8e2fd94ff6-20261028T1424Z", from good 1f82dbdc)
A/B: PASS, kexec x3 trial->trial: early reset, `bus quiesced: pcie-perst`, quiesced gem/xhci0/xhci1/dma40, no CORRUPT/FAILED, eth0 1000M up, usb0/usb1 xhci up, 4 CPUs, btd ready.
C (pcie.rpi5=0): PASS, `[pcie] off`, no stage, no macb, btd ok. D (-> prev 902a07c4... ): PASS, no CORRUPT.
F idle 60 s: 100.0% idle both on and off. G soak 28+ min: no controller gone/WDOG/panic, thermal 32.5C. H lsusb: two 1d6b:0003 root hubs.
E (no cable) and a cold boot: USER steps, not run.
8BitDo E4:17:D8:82:A2:0A: btd/bthid auto-started with no manual step; bthid connected out (failed 0x0b), pad then connected incoming (handle 0x00b), but btd logged "idle, disconnecting" (reason 0x16) and no new /dev/input/event* appeared (event0, event1 only). Not caused by this lane (BT is uarta).
aarch64 box regress: 57 suites, 0 bad, PASS.
