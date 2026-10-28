# Review: lane/pikexecdma (merge 6c22d99c), 2026-10-27

Scope: `git diff 6c22d99c^1 6c22d99c` plus the lane note. Code review only, nothing run.
Totals: 1 High, 3 Medium, 4 Low.

Main premise checked: in `kernel_kexec` the dma_shutdown hooks run after
`machine_prepare()`, which only masks interrupts on the **calling** CPU. The other
CPUs keep scheduling kernel threads until `exec_aarch64::stop_and_jump` sends SGIs
(kernel/src/kexec/exec_aarch64.rs:235). So every hook races live driver threads.

## 1. HIGH: a bridge reset while other CPUs still run RP1 drivers can stall a CPU on AXI forever
- Where: drivers/src/brcmstb_pcie.rs:434 `kexec_reset` (Stage::Bus), called from kernel/src/power.rs:408. The CPUs are parked later, in exec_aarch64.rs:235.
- Failure: the `macb` driver thread (drivers/src/macb.rs, loop with `msleep(1000|250)`) never checks `RUNNING`. Neither do `tx_check`, MDIO `w.poll`, `sample()`, `rp1_fan::rpm()` (pi5_board.rs:111), rp1_gpio or the RP1 DWC3 xHCI threads. They keep touching RP1 BAR MMIO on other CPUs. Once `bridge_assert` has run, any access through the outbound window never completes, and that CPU stalls on AXI (stage-5 lesson from lane pipcie3). The stall lasts at least 5 ms: the 50-character `[kexec] quiesced:` line at 115200 baud, then the stop logs, then the SGI. Then:
  - `final_cpu0` waits 2 s and prints "never reached the park page; jumping anyway".
  - The next kernel's PSCI CPU_ON for that core fails with ALREADY_ON, so a core is lost.
  - Worst case: the next kernel's `brcmstb_pcie::init` releases the bridge (stage 6). The stalled load completes, and the old CPU resumes old-kernel code in memory the new kernel now owns. That causes silent corruption, the same class as the bug this lane fixes.
- Same root cause, smaller effect: `kexec_shutdown` writes NCR without `NCR_LOCK`. `ncr_set` (macb.rs:198) is a read-modify-write under that lock, so a concurrent `ncr_set(TSTART)` can write TE/RE back on after the hook cleared them. BME-off and PERST# contain this, but it shows the hooks are not exclusive.
- Fix (pick one):
  - (a) Preferred: run `dma_shutdown::run(Stage::Bus)` in `final_cpu0` after the AP park-page wait, before the jump. Use a timer-free spin delay there, because `timers_off()` has already run. Also run it when the kexec started on CPU0.
  - (b) Before the Device stage, make each RP1 driver thread stop and acknowledge. For example, `RUNNING=false` is checked at the top of the macb loop and in `ncr_set`, and the hook waits up to about 2 s for an "exited" flag. Same for the fan, gpio and xHCI threads. Also take `NCR_LOCK.try_lock()` in `kexec_shutdown`.

## 2. MEDIUM: `cfg_access` reads REGS before taking CFG_LOCK, so it can use a stale base after `kexec_reset`
- Where: drivers/src/brcmstb_pcie.rs:121 (`let base = REGS.load`) and :124 (`CFG_LOCK.lock()`), against :445-449.
- Failure: a CPU in `cfg_access` (any config read on another CPU, see #1) loads `base != 0` and then waits on CFG_LOCK. `kexec_reset` holds the lock, clears REGS, asserts the bridge and drops the lock. The waiter then:
  - bus 0: reads `base + o`, an RC access while the bridge is in reset, and the CPU stalls forever;
  - bus 1: `rd()` reloads REGS = 0 and reads VA 0x4068, which faults the kernel on that CPU.
  The comment at :428-431 promises "every later cfg_access returns all-ones instead of stalling", and that is not true for a caller already inside the function.
- Fix: load REGS after `CFG_LOCK.lock()` and return `ones` if it is 0. Use that `base` for `rd`/`wr` inside the critical section, not a fresh global load.

## 3. MEDIUM: the early reset runs on every rpi5 boot, including cold firmware boots and `pcie.rpi5=0`, and its first RC read happens before rescal
- Where: kernel/src/main.rs:1056-1078 and drivers/src/brcmstb_pcie.rs:470-478 (`read(MISC_PCIE_CTRL)`).
- Failure: before this change, the first RC read only ever came after `rescal_reset` (stage 3). Lane pipcie3 showed stage 4 reads work on a cold boot, but always after rescal. If the RC block does not answer before rescal on some firmware or board revision, every Pi 5 boot hangs before mm init. That includes `pcie.rpi5=0` boots, which never touched pcie2 before. Nothing reaches the log, and the only recovery is adding `pcie.early_reset=0` to cmdline.txt on the SD card. The lane note lists this as unverified.
- Second-order effect with `pcie.rpi5=0`: RP1 is now left in PERST# for the whole session. Before, it was left as the firmware set it up. This affects the RP1-driven fan on GPIO45 (PWM1 ch3, inverted polarity) and the header UART/GPIO. A board that relied on the firmware's RP1 state loses it without warning.
- Fix:
  - Do not touch MISC_PCIE_CTRL early. A SW_INIT SET of the bridge (reset-controller block only) already cuts RP1 off from AXI/DRAM. Stage 7 of `init` asserts PERST# after the bridge is released anyway.
  - Gate the early reset on evidence of a kexec hand-over (physical x0 / prevlog magic), so cold firmware boots stay exactly as before.
  - Check on the real Pi that the fan state with `pcie.rpi5=0` is acceptable.

## 4. MEDIUM: V3D has no dma_shutdown hook
- Where: drivers/src/v3d/ (queue.rs:377 starts CLE via `CT0QBA`). Neither drivers/src/v3d nor kms_v3d.rs registers a kexec, shutdown or quiesce hook.
- Failure: V3D writes to DRAM (render targets, TFU, CSD) through its own MMU, whose page tables live in old-kernel pages. A job still running, or a hung one, at kexec keeps writing into pages the next kernel treats as free. The next kernel also inherits a V3D MMU base pointing into its own free memory. Userspace teardown makes this unlikely but does not prevent it.
- Fix: register a `"v3d"` Stage::Device hook that:
  - stops the CLE/CSD/TFU queues;
  - waits for idle with a bound, then resets the V3D core (GMP/hub reset as in Linux `v3d_reset`);
  - disables the V3D MMU.
  HVS scanout only reads memory and is covered by `handover_fb`.

## 5. LOW: `dma_shutdown::run` skips every hook, including `pcie-perst`, when `try_lock` fails
- Where: drivers/src/dma_shutdown.rs:60.
- Failure: the other CPUs are still running (see #1), so a concurrent `register` (for example a late `dma40` window map) makes `run` return empty. The log then says "(no bus masters registered)" and RP1 keeps mastering. The failure is invisible. The comment's premise ("a CPU parked mid-register") is wrong at this call site.
- Fix: spin on `try_lock` with a bound of about 10 ms, and on timeout print `[kexec] dma_shutdown registry busy`. The registry is tiny, so a plain `lock()` also works once #1(a) moves the Bus stage after parking.

## 6. LOW: `kexec_reset` sets RP1 command bits when the bus-1 read returns all-ones
- Where: drivers/src/brcmstb_pcie.rs:437-440.
- Failure: the link is up but the config read gets UR (`CFG_READ_UR_MODE` returns all-ones). Then `cmd & 0xffff & !4` = 0xfffb, which sets MEM/IO/SERR/INTx-disable and more on RP1 just before PERST#. This is harmless only because PERST# follows.
- Fix: skip the write when the read returns 0xffff_ffff.

## 7. LOW: the xHCI count in the `[kexec] quiesced:` line is a string match and never reports failure
- Where: kernel/src/power.rs:278 and :417.
- Failure: a DWC3 that fails to halt is not counted and does not show up under `FAILED:`, so the line can't be used as a pass/fail check on the Pi. On x86 the same count names every xHCI `xhciN`. That part is cosmetic.
- Fix: have `usb_hcd::quiesce_all` return a structured ok/failed list, or register the xHCI halt itself as a Stage::Device hook.

## 8. LOW: the new kernel has an unprotected window before `early_reset`
- Where: kernel/src/main.rs:1056.
- Failure: if the old kernel did not cut RP1 (an older kernel, or hooks skipped as in #5), RP1 DMA can land in the kexec'd image, initrd or DTB before the early reset runs. The early reset only protects the buddy free lists that come after it. This is defence in depth only, so the old-kernel hooks must stay the primary fix.
- Fix: none required. State it in the lane note so the early reset is not counted as a full guarantee.

## Checked and fine
- Ordering inside one CPU is correct: Device hooks (gem, dma40), then `quiesce_all_pci` (BME off), then PERST#, then the bridge reset. No RC access follows the bridge reset on the kexec CPU, because REGS is cleared and cfg returns all-ones on rpi5 since ECAM base is 0.
- `early_reset_identity` follows the SW_INIT SET/STATUS layout of `bridge_assert`. PUD 64 covers 0x10_0012_0000 and 0x10_0150_4318.
- Stage 4 now skips RC reads when the bridge is in reset, and that path then goes through stages 5 and 6. This is the pipcie3 sequence, but it now runs on every Pi boot and has not been tested on the Pi.
- x86 and QEMU virt: the registry is empty, so the only change is the extra log line. The early reset is behind `cfg(feature = "rpi5")`.
- dma40 `abort` is bounded (100 us) and leaves firmware channels alone.
