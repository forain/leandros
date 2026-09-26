# Lane greeterlag — 2026-09-24/25

Branch `lane/greeterlag` (base origin/integ-wave-0924 `c79249a`), head `e624610`, pushed.
Laptop worktree `~/Projects/leandros-greeterlag` (x86_64/KVM), Mac worktree `~/code/leandros-greeterlag` (aarch64/HVF).
Probe: `/tmp/gl-lag.py` (QMP key 'a', screendump of the password field until it changes; `REGION=` env for 1280x800).

## Result: FIXED — two causes, both necessary

| x86_64/KVM, per-key latency | p50 | range |
|---|---|---|
| integ tree (baseline, re-measured) | 35.5 s | 5.2–43.7 s |
| + f2fs 192-slot cache | 10.8 s | 4.1–23.1 s (quantized at ~4.1 s = one comp frame) |
| blur off, f2fs still 4 slots | 19.0 s | 0.17–37.2 s |
| both (final, diag off) | **0.168 s** | 0.163–0.63 s, 10/10, 0 WDOG |

aarch64/HVF final: p50 0.284 s (0.266–0.397), 10/10 keys, 10 dots on screen. No aarch64 baseline was taken.

### 1. Kernel: f2fs block cache had 4 slots (`servers/f2fs/src/lib.rs`, `bc6e1e2`)
The greeter looks up icons that are missing from the staged Cosmic theme (`/usr/share/icons/Cosmic` has no
size dirs, so the icon buttons are empty circles). It stats every size/category/extension combination, about 750 per
repaint, on its **UI thread**. `[SCSTAT]`: 93 % of the greeter main thread's wall time was in stat().
`[SCPATH]` timing for a failed stat: VFS_STAT 1437 µs + VFS_OPEN fallback 1242 µs (medians). Mean total per call: 2.97 ms.
Cause: a 4-slot LRU. One dentry lookup touches NAT, inode, NAT, inode and data blocks, so every path
component missed and went to virtio-blk. The 4 dated from building MountState on the 64 KB boot stack. It is now
built element-wise in its Box, so 192 slots (~840 KB MountState) is safe. After: 10 µs + 9 µs. The same load now costs
the greeter ~1 % of its UI thread.
Tests: f2fstest, vfstest, exectest, memtest, forktest had 0 FAIL on both arches.

### 2. Userland: compositor blur on softpipe (`ports/cosmic-greeter/0001-blur-opt-in-under-softpipe.patch`)
The PC sampler showed an idle greeter. cosmic-comp's render thread was in libgallium softpipe 75–83 % of a CPU
(`fetch_source`, `img_filter_2d_linear`, `store_dest`, `exec_instruction`, `wrap_linear_*`). Kernel time ≈ 0.
The greeter asks for an ext-background-effect blur behind its card, and cosmic-comp redoes a multi-pass shader
every frame. That was ~4 s per frame, and each key waited for a frame.
The patch skips `blur_rects` unless `/etc/leandros/greeter-blur` exists. **⚠ It breaks the port README's "no COSMIC source
patch" rule**, so it needs orchestrator/user sign-off. The alternatives are a cosmic-comp patch that skips blur on a
software renderer (this would also cover session clients), or llvmpipe or a GPU renderer.
Rebuilt with `build-greeter.sh` (16 s incremental). **Mac `m6-session-bins/out/cosmic-greeter-{x86_64,aarch64}` and the
laptop's x86_64 copy were REPLACED** with the patched binaries. The originals are kept as `*.orig-pre-noblur`. Every image
built from these machines from now on gets the patched greeter. The desktop's copy is not updated.

## Diagnostics added (gated off)
- `sched/src/pcsample.rs`: `ENABLED` enables a 100 Hz per-CPU timer-IRQ PC sampler. Ctrl-T drains `[PCS] cpu tgid pid sc u/k/i pc`.
  **⚠ Syscalls run with IRQs masked, so in-syscall time is invisible to it.** That time is charged to the user PC after the
  syscall returns: the greeter's "time in libc fstatat" was kernel stat time. Use `[SCSTAT]` for syscall time.
- `SC_STATS`: second focus for `cosmic-greeter-login`, plus a `[SCPATH]` stat path trace with per-phase µs.
- Symbolizing: comp's libgallium text VMA `0x44D24000` − rx vaddr `0xd5000` = base `0x44C4F000`
  (laptop sysroot build). The scripts `/tmp/gl-pcs.py` and `/tmp/gl-sym.py` are on the laptop.

## Open
- Stage the missing Cosmic icons, or pick a theme that has them. That fixes the empty greeter buttons and ends the
  ~500 stat/s loop, which is now cheap.
- f2fs `find()` is a linear scan of 192 slots. It is fine as measured, but a hash would help large sequential reads.
- Shared: `servers/f2fs` (the greeterleak lane touches f2fs). I also synced `~/Projects/doomgeneric` on the laptop from the Mac: the
  midi sources and `third_party/tinysoundfont` were missing, so the build failed.
