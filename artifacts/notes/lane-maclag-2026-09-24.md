# Lane maclag, 2026-09-27

Branch `lane/maclag`, cut from `origin/integ-wave-0924` at `bc52498a`. Head is `3d7b410b`, and it is pushed. The worktree is `~/code/leandros-maclag`.

## Outcome: ROOT-CAUSED. The problem was a stale image, not the VNC path and not the current code.

The symptom: about 12 s per greeter key over TigerVNC, with the user's QEMU at 270–470 %.

**Cause.** The user booted `~/code/leandros/leandros-limine-aarch64.img`, dated **2026-09-16 22:26**. That is the build of `503b11e`, and it was never rebuilt at a5b5b62. That image has:
- no `/bin/gpu-env` and no `/bin/gpuprobe`;
- a `/bin/greeter-env` that exports `GBM_ALWAYS_SOFTWARE=1`.

So cosmic-comp renders on softpipe, including the greeter's blur, which costs about 4 s per frame (see greeterlag). The clock reads "Thursday, January 1", and the icon buttons are empty.

"Lightning fast in Screen Sharing" was a **different VM**: finalverify's integ QEMU on :5951 (pid 93379), which runs a fresh image.

## Measurements

All runs use my own QEMU (`~/.local/qemu-gpu-gles31`, aarch64/HVF) and a raw-RFB probe (`scratchpad/ml/glat.py`). The probe learns the blinking caret's mask, types a key, then measures until the first update outside that mask.

| image | client | key → echo | host QEMU CPU |
|---|---|---|---|
| copy of the user's 16-Sep images | raw RFB, no TigerVNC | **4.8–29.6 s** (bksp 15–29 s) | **254–274 %**, steady from boot |
| integ `bc52498a`, fresh build | raw, keys via QMP | 53–67 ms | 9–14 % |
| integ, fresh build | TigerVNC 1.16.2 attached, through a logging proxy | 52–58 ms | 11–13 % |
| integ, fresh build | keys as RFB / QEMU-ext key events (includes the 50 ms hold) | 103–138 ms | 9–26 % |
| integ, fresh build | the same, under a 120 Hz pointer flood | 103–110 ms | 18–23 % |
| integ + this lane, via `run-qemu.sh` | ext key events | 105–110 ms | 5–11 % while typing, **3 % idle** |

TigerVNC's own traffic, seen through the proxy:
- it negotiates Tight first;
- it sends no SetDesktopSize, even with `-geometry 1500x950`;
- it receives about 100–200 B/s, which is the caret.

QEMU runs lossless and non-adaptive, because `lossy` defaults to off.

**No VNC, QEMU or TigerVNC fix is needed.** One side note: TigerVNC defaults to `Shared=0`, and QEMU's `share=allow-exclusive` then drops every other VNC client, so harness probes get an EOF. `-Shared=1` avoids that.

## Fix

`3d7b410b` changes `scripts/run-qemu.sh` (21 added lines, uefi mode only). It prints `⚠️ STALE IMAGE … Rebuild: ./scripts/build-all.sh --arch <arch>` when `$DISK_IMAGE` is older than the last commit touching kernel, servers, ports, userland or the image scripts.
- On the user's image it fires: 2026-09-16 22:26 against the 2026-09-27 09:12 source commit.
- On a fresh build it stays silent.

It overlaps finalverify's run-qemu.sh edits only as a new standalone block, placed after the data-disk block. There were no conflicts at `bc52498a`.

## Found: main + the current shared stage = no GPU

main `a5b5b62` built against the shared `gpu-stage-aarch64` (27 Sep) boots with `gpu-env: card0=none … NO GPU RENDERER`:
- the stage's `gpuprobe` → `libzstd.so.1` needs `libleandros_ssp.so.1`;
- only integ's `mkfs-f2fs-populated.py` ships that library (`79bcdb7d`).

So rebuilding **main** gives a greeter-less boot. Rebuild from integ, or wait for the merge.

## What the user should run now

```
cd ~/code/leandros-integ   # or ~/code/leandros once integ-wave-0924 is merged to main
./scripts/build-all.sh --arch aarch64
./scripts/run-qemu.sh aarch64
```

No TigerVNC options are needed.

Test coverage: only aarch64 was tested. The change is a shell-only warning that is independent of arch, and x86_64 was not booted.
