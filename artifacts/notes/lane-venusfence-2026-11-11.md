# lane/venusfence 2026-11-11 — REFUTED (already done by lane/gpuq); docs only

- The TODO premise was stale. Ctrlq ISR is wired (MSI-X on x86_64, INTx on aarch64; CtrlqWait parks until IRQ, lane ctrlqout b8c492d, gpuq 0e698b3e).
- FENCE_FD_IN (kernel/src/syscall.rs), in_syncobjs/out_syncobjs/SYNCOBJ_RESET (drm_device_interface.rs virtgpu_handle_execbuffer) were FIXED by lane/gpuq 2026-10-19; fence_is_done floor + gpu_fence_event wake syncobj waiters.
- Fail-first tests already exist: /bin/gpuqtest FENCE_FD_IN_WAITS, FENCE_FD_IN_NOT_A_FENCE, OUT_SYNCOBJ_INSTALLED, IN_SYNCOBJ_NULL_EINVAL, IN_SYNCOBJ_RESET (11/11 virgl aarch64).
- Not done: `gpuqtest --venus` (adds DEAD_CTX_FAST) never run. Box run `box/regress.sh main x86_64 --venus /bin/gpuqtest` sat >590 s "waiting for box slot" (box busy) and was killed; re-run when idle.
- Still open: Venus submit stays synchronous (busy/park wait per submit), true async completion is not implemented; cross-open dmabuf; aarch64 vkwl.
