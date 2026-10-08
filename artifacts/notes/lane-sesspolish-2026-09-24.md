# Lane sesspolish — 2026-09-27

Branch `lane/sesspolish` from integ-wave-0924 `0c8d50a`, head `b3c9eb0e` (pushed).
Laptop x86_64/KVM (UHD 620): Venus→zink/ANV and virgl/iris. Mac aarch64/HVF virgl (GPU QEMU).
Laptop worktree `~/Projects/leandros-sesspolish`; scripts `~/Projects/sp-{boot,hog,all,la,cycle}.sh`, results `~/Projects/sp-out/`.

## 1. Esc does not close the workspaces overview: FIXED (kernel, `0821838`)
Root cause is not COSMIC, xkb or focus. The UART drain pushes every serial byte as
`EV_KEY code=<byte> value=2` (arch/*/timer.rs). The VT chord recogniser (`servers/tty/src/vt.rs`
`chord_key`) treated any value≠0 as "down". Four bytes map to modifier keycodes:
- 'a' (97) = KEY_RIGHTCTRL
- 'd' (100) = KEY_RIGHTALT
- '8' (56) = KEY_LEFTALT
- 0x1d = KEY_LEFTCTRL

So any serial command, such as the driver's `rm -f /etc/leandros/text-login`, latched Ctrl+Alt for the rest of the boot. A later plain Esc was then the **Ctrl+Alt+Esc rescue**: it released evdev grabs, revoked the scanout and showed VT1 text. The overview "stayed" and the screen went to the text console.

Proof on the baseline, Venus:
- with no serial input, Esc closed the overview 3/3 (p50 57 ms);
- after `echo ad` over serial, Esc closed it 0/2 and the text console appeared.

Fix: CHORD_MODS moves only on value 1 and value 0.

After the fix, with `echo ad8` sent on purpose, Esc closes the overview 5/5 on every path:

| path | Esc close p50 |
|---|---|
| Venus | 35 ms |
| virgl | 40 ms |
| aarch64 | pass (see caveat below) |

## 2. Launcher latency: root cause + numbers (`39557ae`, `scripts/vnclat.py`)
The launcher **never opened on any Linux-built image**. mkfs sourced the pop-launcher `plugin.ron` files from a hardcoded `~/code/cosmic-epoch` path, which exists only on the Mac. That is the same trap sessmisc found for the Shortcuts files.

With zero plugins, `search()` queries nobody, so `finished()` never runs and no Update is sent. cosmic-launcher shows its window only on the first Update. mkfs now resolves the path through `_find_cosmic_epoch()` and fails the build if either plugin is missing. The two `.ron` files were added to the laptop and desktop cosmic-epoch subsets (README.subset notes it).

Probe: `vnclat.py` holds a persistent VNC connection, so updates are pushed rather than polled. Keys go in via QMP. Laptop, 8 rounds, p50:

| path | Super → visible | type 't' → echo | type 't' → results settled | Esc close |
|---|---|---|---|---|
| Venus | 174 ms | 68 ms | 186 ms | 72 ms |
| virgl | 142 ms | 61 ms | 190 ms | 66 ms |

Overview open: Venus 306 ms, virgl 175 ms.

aarch64 VNC timings came out at about 3 ms, which is not credible, so aarch64 is a functional pass only. It was pixel-verified: overview open/closed, launcher results.

## 3. Memory guard on x86_64 in a real GPU session: adaptive sampling (`e4c94a44`)
The hog now runs inside cosmic-term, started with Super+T. `memtest hog [MiB/s]` is a paced hog.

Baseline init with 5 hogs at 32 MiB/s (160 MiB/s total):
- the guard fired only when MemAvailable was down to **16 MiB**;
- 3 of the 5 hogs died outside the guard.

New sampling:
- every 2 s normally;
- every 250 ms when MemAvailable is within floor+25 %, or when at the current fall rate it would reach the floor within 5 s;
- kill after 1 s below the floor, or immediately if memory would be exhausted within 2 s;
- 2 s grace after each kill.

Results with the new init. Every run killed only the hog, and the session survived with a pixel-verified desktop.

| path | hog rate | MemAvailable at kill |
|---|---|---|
| Venus | 32 MiB/s | 155–162 MiB |
| Venus | 200 MiB/s | 145–183 MiB |
| Venus | 400 MiB/s | 183 MiB |
| virgl | 32 MiB/s | 157 MiB |
| virgl | 200 MiB/s | 157 MiB |
| aarch64 | 32 MiB/s | 157 MiB |
| aarch64 | 200 MiB/s | 173 MiB |

## 4. Written private file pages → RssAnon (`543d02f`, mm/vmm.rs, mm/cow.rs)
Mechanism:
- Each VMA carries a per-page `written` bitmap, used only for private lazy file VMAs.
- Clean pages inside the file extent are installed read-only.
- The first write sets the bit and upgrades the PTE. That write can be a user fault, `prefault_range` before a kernel store, or `write_user_buf`.
- CoW promotions set the bit.
- fork copies the bitmap, `split_at` cuts it, and mprotect keeps clean pages read-only.
- `file_anon_split` counts written pages as anon.

memtest `proc_rss_file_written` (new) passes on x86 Venus, x86 virgl and aarch64:
- RssFile 1060→804 and RssAnon +256 after a 256 KiB user write;
- another 64 KiB moves on a read(2) into the mapping;
- the file is unchanged.

## Tests
memtest, forktest, drmsmoke (failed=0), exectest and killmt all returned rc=0 on:
- x86 Venus;
- x86 virgl;
- aarch64 HVF virgl.

## ⚠ Found, not fixed
- **aarch64 virgl greeter crash-loops on the current Mac stage**: `m3-gl-stack/gpu-stage-aarch64`, dated 27 Sep 00:21, which is the wgpuebadf stage.
  - Symptom: greeter chain EL0 write fault at FAR=0x224 right after the virgl context is created, then `EGL BAD_DISPLAY`, and "graphical login exited after 1 s".
  - It is not this lane: it reproduces with this lane's mm change reverted (kernel-only rebuild).
  - With `LEANDROS_GPU_STAGE` pointing at `gpu-stage-aarch64.prev`, the session works. All aarch64 session results above used `.prev`. The shared stage was not touched.
- The kernel fbcon mirrors the serial shell onto the scanout only after the rescue fires. That was a symptom of item 1, not a separate bug.
- `/tmp` in the guest returned ENOSPC for a `tail > /tmp/g` (root f2fs 51 % used). Not investigated.
