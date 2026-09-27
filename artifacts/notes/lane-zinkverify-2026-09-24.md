# lane/zinkverify — gpuirq re-run on x86_64/KVM Zink, audio re-run, spin-before-park

Machine: linux desktop (x86_64 KVM + RADV, aarch64 TCG). Worktree
`/run/media/forain/samsung970pro512/leandros-siblings/leandros-zinkverify`, branch
`lane/zinkverify` = `93184de` (on `fc5a6bc`). Pushed.

## Method
Same data images for every run. The only variable was the kernel: `mcopy` of a kernel-only build into a copy
of the boot image's FAT (`img@@1048576`). Baseline "base" = `7a4e820` (main just before the gpuirq merge;
`7a4e820..fc5a6bc` is kernel + drmsmoke only). Every build had `DRM_STATS = true` except the audio "final" builds.
Ran `scripts/zinkbench.py <tag> --zink` (80 s window, 40 s settle) with `LEANDROS_RUN_ID=zinkverify`,
`LEANDROS_VNC_PORT=5931`. Logs are in `~/zinkbench-zv/` on the desktop. The greeterleak lane's QEMU was running at the same time.

## Zink fps (x86_64/KVM, --venus + LEANDROS_ZINK=1)
| kernel | fps runs |
|---|---|
| base 7a4e820 (pre-gpuirq) | 20.32, 20.43 |
| fc5a6bc (gpuirq) | 21.40 |
| fc5a6bc + histogram, park at step 0 | 20.54, 20.48 |
| lane (spin 200 us, then park) | 21.23, 20.51 |
There was no fps regression: every run landed at 20.3–21.4 fps, and every one had 0/40 zero-flip samples, 0 timeouts, and a VNC picture of the desktop.
With gpuirq, 1768 of 1771 flips were delivered on the fence path (`flips_irq`), with about 16k MSI-X interrupts per run.

## Regression found: parked sync waits (whole session, ~130 reply-needing cmds, all at session start)
| | mean µs | max µs | ≥1 ms | ≥5 ms |
|---|---|---|---|---|
| base (spin) | 55 | 2517 | – | – |
| gpuirq (park at step 0) | 329 / 131 / 369 | 8828 / 2246 / 9551 | 7 / 4 | 3 / 0 |
| lane (spin 200 us first) | 64 / 52 | 2771 / 2397 | 2 / 1 | 0 / 0 |
All of this time is spent holding `VIRTIO_GPU`. The ~9 ms tail equals one tick, which fits a lost wake: the MSI goes only to the BSP, and the BSP does not take it while it runs at IF=0. For example, the BSP may be spinning for the `VIRTIO_GPU` lock the waiter holds, so the waiter has to sleep until its own tick.
Fix: `CtrlqWait` spins for `CTRLQ_SPIN_BEFORE_PARK_US = 200` before it parks. Now only 5 of ~130 waits reach the parked phase.
This does not affect steady-state fps, because there is no sync traffic after session start.
DRMSTAT gains `park_phase wh0..wh4` at the end of the line.

## Tests
- x86_64/KVM drmsmoke **72/72** on the lane kernel. The flip-event latency mean is 4.1 ms, 240 flips/s, 32/32 flips on the fence.
- aarch64/TCG drmsmoke **72/72** (full aarch64 build of the lane tree). INTx is armed, and 32/32 flips were delivered on the fence.
- **Storm guard exercised** (aarch64/TCG, a throwaway kernel with virtio-net's INTx-disable bit removed, not committed): once DHCP traffic began, `[GPU] INTx storm: INTID 0x23 held asserted by another function; masked, polling only` appeared. The system stayed up and drmsmoke ran 68/72. The 4 failures are the IRQ-armed assertions, which is expected. All 32 flips were still delivered, with 0 timeouts.
- Audio `audio-glitch-test.sh` x86_64/KVM (the script's Mac `cd` had to be patched in a copy): lane ×2 and base ×1 all **PASS**, with 0 recoveries, 0 gaps, 100.0% speed, no holes, and 0 `[SND] TX stalled` lines. Each run has a 3.4 s zero-run that also appears on base, so it is part of the game's audio and not a glitch.

## Left open
- The parked wait still holds `VIRTIO_GPU`. Moving the wait out to the call sites is still the real fix, but after this change it matters only for the slow tail.
- `SYNC_CMD_PARKED`/`CTRLQ_PARKED` counts waits that were allowed to park, not waits that actually parked.
- `audio-glitch-test.sh` has `cd /Users/forain/code/leandros` hardcoded, so it cannot run on the Linux machines unless that path is patched.
