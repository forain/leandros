# lane/gspkexec, 2026-10-26: NVIDIA/GSP kexec hook (openbugs L8.5 remainder)

Status: **PARTIAL (implemented and tested in software, opt-in).** Validation on the 3070 is pending because the card is unavailable for about a month.

## Problem
Before this lane, kexec left GSP-RM running on the RTX 3070. devreset keeps the boot-framebuffer GPU as it is ("boot framebuffer for the next kernel"), so WPR2 stayed up. In the next kernel, OpenRM `kgspBootstrap` then stops at
`kernel_gsp.c:3465` `if (kgspIsWpr2Up_HAL(...)) "unexpected WPR2 already up, cannot proceed with booting GSP"`
(OpenRM 570.86.16; `kgspIsWpr2Up_TU102` reads NV_PFB_PRI_MMU_WPR2_ADDR_HI 0x1FA828, VAL = bits 31:4).

## What the driver must do (sources)
- Linux `kernel-open/nvidia/nv.c` `nv_shutdown_adapter` runs these steps in order: `rm_disable_adapter` (RmDisableAdapter turns GPU interrupts off with `intrSetIntrEn(DISABLED)`), then it stops the bottom-half queue, `free_irq`s, and calls `rm_shutdown_adapter`.
- `rm_shutdown_adapter` calls RmShutdownAdapter (osinit.c:2105), which runs `gpuStateUnload` and then `gpuStateDestroy`. `gpu.c:3266` then calls `kgspUnloadRm(KGSP_UNLOAD_MODE_NORMAL, GPU_LEVEL_0)`. kgspUnloadRm_IMPL (kernel_gsp.c:3824) does three things:
  - it sends `NV_RM_RPC_UNLOADING_GUEST_DRIVER(bInPmTransition=0, bGc6Entering=0, level 0)`;
  - it calls `kgspWaitForProcessorSuspend`;
  - it calls `kgspTeardown_TU102`, which resets the GSP falcon, runs FWSEC-SB to put the PreOsApps back, and runs booter unload to take WPR2 down.
- nouveau `r535_gsp_fini(gsp, suspend=false)` follows the same order. It calls `r535_gsp_rpc_unloading_guest_driver`, polls falcon reg 0x040 bit 31 for up to 2 s, and then runs `nvkm_falcon_reset`, `nvkm_gsp_fwsec_sb` and `r535_gsp_booter_unload`.
- LeandrOS links NVIDIA's own RM (libnvrm.a), so the hook calls these same RM entry points. It does not re-implement the RPC.

## Implementation (commit 27219995)
- `drivers/nvidia/src/kexec_sm.rs` is a pure state machine that depends only on `core`. Its pieces:
  - `worker()` runs disable, then stop_irq, then shutdown, and publishes the phase before each step.
  - `run()` handles the knob and GSP-up checks, a static once-guard, starts the worker, polls it with a deadline (10 ms steps), and then reads WPR2 back.
  - Outcomes are Unloaded, Timeout(phase), WorkerFailed and Wpr2StillUp. The last three ask for an FLR.
- `drivers/nvidia/src/kexec.rs` binds the state machine to the system:
  - the worker runs on kthread `nvrm-unload`;
  - it calls `rm_disable_adapter` and `rm_shutdown_adapter`, plus `device::kexec_stop_irq` (cancel and flush the bottom half, then free the MSI);
  - it reads WPR2 through an ioremap of BAR0 + 0x1FA828.
- `device.rs` records the nv_state and BDF only after `rm_init_adapter` OK on real hardware. Rehearsal runs (`nvrm.fakegpu`) never arm the hook.
- `kernel/src/power.rs` adds `nvidia_kexec_unload()` and calls it right after `shutdown_prepare` (filesystems synced). At that point interrupts, the scheduler and bus mastering are still on, which matters because the GSP RPC queues live in system memory. It runs before `quiesce_phase1`, which clears bus mastering on class 03xx.
- `kernel/src/kexec/devreset.rs` adds `force_reset(b,d,f)`, which overrides the boot-framebuffer Keep so that the function gets FLR (or AF FLR, PM, SBR, as planned).
- Knobs:
  - `nvidia.kexec_unload=1|0`, **default 0**;
  - `nvidia.kexec_timeout_ms=N`, default 10000.
- Messages go to serial with the `[KEXEC] nvidia: ...` prefix. Through the log hand-off, they show up as `[prevkexec] [KEXEC] nvidia: ...` in the next generation's dmesg.

### Why the default is 0
The RM teardown path (`rm_disable_adapter` / `rm_shutdown_adapter`) has never run on LeandrOS's OS shim, and it calls OS callbacks that boot never exercised: unmap, free_pages, and possibly stubs that "log and fail". Three more things are unproven:
- what the display does after unload, when the 3070 owns the GOP framebuffer;
- whether in-kernel or userspace RM clients still alive at kexec time make `gpuStateUnload` assert;
- whether FLR alone clears WPR2 on GA104.

The deadline and FLR fallback bound a hang, but they cannot catch a panic inside RM. Flip the default to 1 after the validation below passes.

## Evidence (software)
- Host test, `cargo test --manifest-path drivers/nvidia/kexec-tests/Cargo.toml --target aarch64-apple-darwin`: **8/8 pass**. The tests cover:
  - Linux call order, with the unload sent exactly once across two kexec attempts;
  - a hang in rm_shutdown_adapter: Timeout(Unloading) within 100 ms + less than 500 ms, FLR requested, no second unload afterwards;
  - a hang in rm_disable_adapter: rm_shutdown_adapter is never called;
  - WPR2 still up: FLR;
  - WPR2 unreadable: not a failure;
  - spawn failure: FLR;
  - knob off or GSP down: nothing touched, and the guard is not consumed.
- Mutation check:
  - with the once-guard removed, 2 tests FAIL;
  - with the deadline removed, the suite hangs (killed by `timeout 20`).
- x86_64 `build-all.sh --arch x86_64 --nvidia` is clean, with no warnings from nvidia-rm or the kernel.
- x86_64 TCG kexec chain on that `--nvidia` kernel with `nvidia.kexec_unload=1`: gen0, gen1 and gen2 each reach login, with nproc 4 and `/proc/cmdline` `kexec.gen=2`. The hook is a no-op without an NVIDIA GPU (`no NVIDIA display controller present`), and the devreset plan is unchanged.
- aarch64 regress: 48 suites, all green except nettag2test unbound_recv (known flake), which passed on the rerun (1/1). The nvidia feature is x86_64-only, so on aarch64 only the devreset `force_reset` plumbing is new code.

## 3070 validation step (exact)
Run this on the PC from the USB stick, with a kernel built by `./scripts/build-all.sh --arch x86_64 --nvidia` and a cmdline containing `nvidia.kexec_unload=1`:
1. **gen0:** in `dmesg | grep -E "INIT_DONE|nvrm\] ===="`, expect `rm_init_adapter OK ... GSP-RM is up (INIT_DONE)`.
2. **Jump to gen1:** run `kexec -l /boot/kexec --initrd=/boot/kexec-initrd --reuse-cmdline --append='kexec.gen=1 nvidia.kexec_unload=1' && kexec -e`.
3. **gen1:** in `dmesg | grep -E "prevkexec.*nvidia:|INIT_DONE|WPR2"`, expect both of these:
   - `[prevkexec] [KEXEC] nvidia: GSP-RM unloaded in N ms, WPR2 down`, with N well under 10000;
   - this kernel's own `rm_init_adapter OK ... (INIT_DONE)`, with no "WPR2 already up".
4. **gen2 and gen3:** repeat step 2 with `kexec.gen=2`, then `kexec.gen=3`. Each generation must show the unload line and INIT_DONE.
5. **Display:** note whether the screen keeps the console across each jump. If it goes black, display restore after RM unload is a separate follow-up.
6. **Negative control:** from a fresh boot, run gen1 with `nvidia.kexec_unload=0`. Expect `GSP-RM left running` and then a gen1 GSP failure ("unexpected WPR2 already up"). This proves the hook is what fixes the chain.
7. **Fallback probe:** from a fresh boot, run gen1 with `nvidia.kexec_timeout_ms=1`. This forces Timeout, then `devreset will reset 01:00.0`, then FLR. Record whether gen1 GSP boots after FLR alone, which shows whether FLR clears WPR2 on GA104. If it does not, the fallback must escalate to SBR on the root port.

If steps 1-4 pass, flip the default in `drivers/nvidia/src/kexec.rs` (`enabled`) to on.

## Open
- The display state after RM unload on the framebuffer GPU is unknown (step 5).
- Whether FLR clears WPR2 on GA104 is unknown (step 7).
- RM clients still alive at kexec time are not freed first. Linux frees them through nvidia-drm/modeset unload and fd close before the last-close `nv_stop_device`.
