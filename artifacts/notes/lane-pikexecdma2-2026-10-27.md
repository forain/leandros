# lane/pikexecdma2 (2026-10-27): fixes for the pikexecdma review

Review: review-pikexecdma-2026-10-27.md. Status: FIXED in code, hardware test pending (orchestrator).

## Changes
1. High: the Bus stage (pcie-perst + bridge reset) no longer runs from `kernel_kexec`
   on aarch64 (option a). It runs in `final_cpu0` right after every AP reports the
   park page, via `dma_shutdown::run_parked` (no alloc, forces the registry and
   `CFG_LOCK`, which a parked CPU may hold). The 100 us PERST# delay is a counted
   register read. Other architectures still run Bus in `power.rs`.
   The gem hook now takes `NCR_LOCK` (bounded try_lock spin, then proceeds).
2. Medium: `cfg_access` re-reads `REGS` under `CFG_LOCK`, all-ones when 0.
3. Medium: early reset only when a kexec prevlog was handed over
   (`boot::kexec_prevlog().is_some()`) and `pcie.early_reset!=0`. It no longer reads
   or writes MISC_PCIE_CTRL; it only SETs the bridge reset bit. Cold boots and
   `pcie.rpi5=0` boots are untouched.
4. Medium: V3D `v3d` Stage::Device hook (registered in `submit::after_mmu`, so inert
   unless v3d probed up): refuses new jobs, CTRSTP on CT0/CT1, waits (bounded) for
   CTRUN clear and CSD active count 0, then MMU_CTL=0. If not idle the MMU stays on
   and the hook reports FAILED. No core reset, no TFU idle poll. CSD_STATUS active
   mask (bits 15:8) is from memory of Linux, unverified; a wrong mask only yields a
   false FAILED.
5. Low: #5 registry try_lock now spins and logs `[kexec] dma_shutdown registry busy`;
   #6 skips the RP1 command write when the read is all-ones. #7, #8 left (cosmetic /
   documented: the early reset is defence in depth, the old-kernel hooks are primary).

## Expected serial lines on the Pi (kexec from a kernel with this change)
Old kernel:
- `[kexec] quiesced: gem xhci0 xhci1 dma40` (no `pcie-perst` in this line any more;
  `v3d` appears too when v3d is up, and no `FAILED:`)
- then, after the CPU park wait: `[kexec] bus quiesced: pcie-perst`
  and no `never reached the park page` line.
New kernel, early: `[pcie] early reset: RC held in reset before mm init`
(or `... (bridge was already in reset)` when the old kernel's Bus stage ran).
Cold SD boot and `pcie.rpi5=0`: NO `[pcie] early reset` line at all.
Unwanted: `early reset: FAILED`, `registry busy`, `FAILED: v3d`.

## Tests
aarch64 release build with and without --rpi5: ok. Host: macb-tests 17, brcmstb-tests 18,
v3d-tests 63 pass. aarch64 regress: see result in the merge report.
