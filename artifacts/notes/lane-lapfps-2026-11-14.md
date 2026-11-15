# lane/lapfps 2026-11-14 (laptop FPS, Firefox on iris)

Laptop HP 840 G5 (UHD 620), LeandrOS one-shot from systemd-boot, kernel b454263c then trial 00b78409 (fb517e39).

Measured (i915 /proc counters + page console.log via devtools.console.stdout.content):
- Firefox rAF canvas animation: 60.0 FPS, flips ~60/s, firefox 35% + cosmic-comp 20% of one core.
- WebGL2 (renderer string Intel, hardware): 720p shader 60 FPS (render engine ~50% busy);
  1080p heavy shader 35 FPS with render engine 100% busy at 1100 MHz -> GPU-bound, no software bottleneck.
  Hardware WebRender: prefs force it and disable software fallback; GPU exec/busy counters advance. Firefox on iris is proven for GL/WebGL/compositing.
- Canvas 2D (CPU Skia) 1900x1000, 1500 gradient arcs + 40 text lines: 5 FPS, content process 100% user time (sys ~0), CPU at 2.7-2.9 GHz. AVX did not change it (4.9 FPS).
- Finding: CPUID OSXSAVE was never set, so all userland ran SSE-only. Fixed: AVX enabled (sched/src/context.rs ymmh, arch enable_sse, signal frame). Fail-first on laptop: sigtest avx_enabled FAIL on old kernel, PASS on the new one, plus signal/preemption ymm test PASS.
- No Linux reference (no Firefox installed on EndeavourOS; not modified).
- Not done: IRQ-driven vblank (DRM_VBLANK_EVENT) - no FPS evidence that it matters; TODO i915 item unchanged.
Laptop left booted into EndeavourOS (default untouched); trial kernel not promoted.
