# lane/ctrlqout — sync control-queue waits out of VIRTIO_GPU, real park count, audio script path

Branch `lane/ctrlqout` @ `b8c492d` (on zinkverify `93184de`). Desktop worktree
`/run/media/forain/samsung970pro512/leandros-siblings/leandros-ctrlqout`; edited from Mac worktree `~/code/leandros-ctrlqout`.

## Changes
- `drivers/src/virtio_gpu.rs`: `submit` split into `submit_begin`/`submit_finish`. There is a new `GpuSync` trait: the reply-needing commands are written once, for either a bare `&mut VirtioGpuDevice` (which waits holding the lock, via `submit_locked`) or a `GpuLocked` handle (`lock_gpu()`). `GpuLocked` enqueues under the lock, drops it for the wait (`wait_sync_unlocked`), and retakes it. Completion is tracked per chain: `SYNC_DONE`/`SYNC_WAITER`, set in `ctrlq_reap`, which IPIs the waiter. `ctrlq_tick` returns whether it got the lock. When it did not, the ISR kicks every CPU in `SYNC_WAITING_CPUS` so each reaps for itself. A waiter never parks while the used ring holds unreaped entries. This uses a lock-free `CTRLQ_USED_IDX_PTR`/`CTRLQ_LAST_USED`. `ctx_create` now reserves its id before the wait. `CTRLQ_PARKED` now counts waits that really parked. DRMSTAT gains `unlocked_waits stranded lost_wake` at the end of the line, and `park_phase` now repeats `ctrlq_parked`. `[CTRLQ-SLOW]` lines appear for waits ≥1 ms (DRM_STATS).
- `drivers/src/drm_device_interface.rs`: every DRM site that issues a sync command now uses `lock_gpu()`. New ioctl 0x1009 (root only) sets spin-before-park. `kernel/src/syscall.rs` routes it.
- `userland/drmsmoke`: SYNC_CMD_PARKED forces spin=0 around ADDFB2, so it now proves a real park and wake.
- `audio-glitch-test.sh`: `cd` is now relative to the repo root.

## Results (x86_64/KVM, greeterleak QEMU plus another lane's aarch64 QEMU running at the same time)
- zinkbench --zink, 8 runs: 21.04, 20.75, 20.58, 20.70, 20.74, 20.53, 20.73, 20.71 fps. Every run had 0 zero-flip samples, 0 timeouts, `lost_wake=0`, and stranded 0–2.
- Unlocked waits: ~120–145 per session. Across 6 instrumented runs only one took ≥1 ms (CREATE_BLOB, 2.6 ms, self-reaped). Every other ≥1 ms wait was a LOCKED one-shot KMS-init RESOURCE_CREATE_2D (pid of the first card opener) at 1.1–22 ms. That is host latency: one of them never parked and still took 22 ms. That wait sets `ctrlq_max` and the mean: whole-session means were 52–205 µs and max 2.7–23 ms, against zinkverify's 52–64 µs and 2.4–2.8 ms.
- Lost-wake theory: not observed after the change (0 across 8 runs). Most of the old tail came from the BSP spinning at IF=0 for the lock the waiter held, and releasing the lock removes that. There was no need to re-aim MSI.
- drmsmoke 72/72 x86_64/KVM: d_parked=3, 3 sync cmds in 936 µs parked vs 92 µs spun. 72/72 aarch64/TCG: d_parked=2, flip mean 99.7 ms; the base kernel under the same contention measured 129.7 ms.
- audio x86_64/KVM ×2 PASS: 0 recoveries/gaps, 99.97% and 100%, 3.3–3.4 s zero-run (game audio, also on base). The runs were launched from /tmp.

## Open
- Still locked: KMS init (kms.rs), boot console, the cursor (`cursor_init`/`cursor_present` sync TRANSFER), `send_command` Transfer*3d, and ring-full waits.
