# lane/ffmremap — 2026-10-23 (openbugs L11.11) — REFUTED (both findings), no kernel change

Mac aarch64/HVF, `--virgl`, release build of main `44f8b5db` + a temporary (uncommitted) klog in
`syscall_dispatch`: every failing mremap (first 200) and every 256th mremap with args/ret/ELR/LR,
and every 2048th sched_yield with tgid/pid/ELR/LR/48 user-stack words. Run `ffmr1`:
`ffsession.py aarch64 ffmr1 --url http://192.168.105.1:8776/ffv.html --put lane-ffmremap-files/ffm2.sh:/tmp/ffm.sh --post "sh /tmp/ffm.sh" --post-timeout 300 --wait 60`
(`ffm2.sh` = ffverify's `ffm.sh` without the blank phase, plus every Firefox process's executable maps
for symbolising the LRs).

## (1) 2048/4096 mremap calls per process — musl measuring the main-thread stack, kernel == Linux

- 295 of 308 logged calls are `mremap(p, 0x1000, 0x2000, 0, 0) -> -ENOMEM` with `p` stepping down
  one page per call from just below the stack top (e.g. `0x7ffffffa6000, …a5000, …a4000`), from
  `ld-musl` (ELR 0x30080060). This is musl `pthread_getattr_np()` for the main thread:
  `while (mremap(p-l-PAGE, PAGE, 2*PAGE, 0) == MAP_FAILED && errno == ENOMEM) l += PAGE;` — it
  probes downward until mremap answers something other than ENOMEM (EFAULT below the stack VMA).
- Linux semantics for each probe: range inside one VMA, not ending at the VMA end, no MAYMOVE →
  can't grow in place → ENOMEM; first page below the VMA → EFAULT. Ours answers exactly that.
  The loop length is the stack VMA size: 2048 pages = 8 MiB, 4096 = 16 MiB (our stacks are full-size
  VMAs; Linux's grow-down stack VMA is only the touched part, so musl reports ~132 KiB there).
  Reporting the real reserved size is the more useful answer (SpiderMonkey sizes its native stack
  limit from it); shrinking the VMA to imitate Linux would make Firefox under-report its stack.
- Cost: one-shot at startup, 1.3–3.4 ms of syscall time per process (sysstat ns column:
  parent 4097 calls / 2.15 ms, content 4096 / 2.6–3.4 ms, others 2048 / 1.3 ms). Not worth anything.
- The other 13 calls: 12 MAYMOVE grows (in place or moved, all succeed) + 1 shrink.

## (2) ~8k sched_yield/s in the parent — not the Renderer, not a virgl fence poll

- Parent over 60 s screen on: sched_yield 8117/s (all return 0, 0.9 % of a CPU in total),
  clock_gettime 11047/s, futex 2849/s (half return 0), ioctl 2022/s (3.4 %).
- 547 yield samples, by thread: glean.dispatcher 331 (61 %), WRRenderBackend#1 69, WRSceneBuilder#1 59,
  rest scattered. **Renderer: none.** All yields come from libxul (Rust: LR offsets 0x29451d4,
  0x29453bc, 0x2948594, 0x7607308, 0x7608020) — the spin-then-yield-then-park backoff of Rust
  channel/lock code on threads that hand messages to each other; not Mesa.
- VIRTGPU_WAIT (`drivers/src/drm_device_interface.rs` `virtgpu_handle_wait`): NOWAIT is a probe
  that reaps first and answers EBUSY only for a really busy BO; without NOWAIT it parks on the
  fence tag (woken by `gpu_fence_event`), 15 s timeout -> EBUSY, as upstream. No yields come from
  the virgl winsys, so nothing here polls a fence.
- Renderer thread = 6 % CPU and the ~2k ioctl/s (≈33/frame at 60 fps): normal virgl traffic.

## Open

- glean.dispatcher ~2 % CPU of yield/futex churn with the screen on: legitimate userspace backoff
  unless futex waits return early. A per-thread futex-WAIT return census (0 vs EAGAIN vs ETIMEDOUT)
  would settle it; not done (budget).
- Video-playback fps / parent CPU unchanged from ffverify v4 (parent 18.6 % here, 60 s, no blank).

## Changes

- `artifacts/notes/lane-ffmremap-files/ffm2.sh` (probe script). No kernel change, so no guard and
  no regress run.
