# lane/canvasperf 2026-11-14 (Firefox canvas-2D CPU raster ~5 FPS)

Tooling: /proc/pcsample (existing, always on) + new scripts/pcprof.py (host symboliser, maps-aware,
--bucket/--disasm for stripped libxul) + .claude/skills/run-leandros/canvasperf.html (1500 gradient arcs +
40 text lines, 1900x1000, logs CANVASPERF fps). See docs/development/profiling.md.

A/B (same page, same Alpine 3.21 Firefox binary unless noted):
- LeandrOS aarch64 on Mac HVF (ffsession, GPU session): 10.5-11.2 fps (one outlier 7).
- Linux arm64 (Alpine 3.21 firefox in Docker on the same Mac, headless): 12.6-12.9 fps.
- Linux x86_64 Ryzen 9 7950X, Alpine 3.21 firefox in podman, headless: 8.3-8.9 fps.
- Linux x86_64 same box, distro Firefox 160 (Arch build, headless): 20-28 fps (different build; not a like-for-like).
=> On aarch64 LeandrOS is within ~15% of native Linux for the identical binary: no kernel/libc deficit.
The page is simply expensive for the Alpine Firefox CPU raster path (and ~3x faster in a distro build).

Profile (aarch64, content process main thread = 100% user, sys ~0): ~75% of its samples sit in one ~24 KiB
block libxul+0x330a000..0x3310000, scalar shift/table/stack-spill code (scanline edge walker of Skia's
path/stroke AA rasteriser), not memcpy/memset (scudo 0.3%, no libc in the top), no futex/page-fault/syscall time.
AVX/NEON enablement cannot help: the hot loop is scalar.

x86_64 guest on KVM A/B: see bottom (pending if absent).
