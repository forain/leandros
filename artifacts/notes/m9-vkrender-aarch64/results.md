# aarch64 Vulkan-to-scanout, photographed — 2026-08-07

`vkrender --present` under `--venus` on aarch64/TCG puts its Vulkan-rendered
image on the scanout, and the census matches the prediction that was committed
before QEMU was started.

Host: `forain@172.16.158.150` (x86_64, so the aarch64 guest is TCG).
Repo at `6beef3b`; criteria committed at `8634425` **before** the first capture.
Fresh `f2fs-data0/1-aarch64.img` regenerated from `mkfs-f2fs-populated.py`
immediately before the boot. One boot, three captures, no `vfstest`.

Guest command, literal: `vkrender --present --present-hold-ms=240000`.
Harness: `artifacts/vkrendercap.py` (venuscap.py's capture route, new subject).

## The three captures

All 1280x800, all 0 uncovered pixels, all fetched over RFB from
`-vnc 127.0.0.1:9,display=venusgpu` paired with `-display egl-headless`.
`screendump` was never attempted; QMP/HMP was never opened.

| | capture | distinct | census |
|---|---|---|---|
| **A** | control, same boot, **no DRM client had run** | 3 | `0x000000` 985722, `0xffffff` 38135, `0xcd0000` 143 — the text console |
| **B** | `vkrender --present`, held | 3 | `0x181818` **958464**, `0x0000ff` **47104**, `0xff0000` **18432** |
| **C** | `drmsmoke --hold`, same boot, **after** vkrender exited | 2 | `0x181818` 958464, `0xff0000` 65536, block bbox exactly x=64..319 y=64..319 |

A contains no `0x0000ff`, no `0xff0000` and no `0x181818` at all. C contains no
`0x0000ff`. So neither the console nor drmsmoke nor the capture route can
account for the blue in B.

## Geometry of B

```
non-background : 65536 px  bbox x=512..767 y=272..527  256x256  fill = 1.0000
0xff0000       : 18432 px  bbox x=544..735 y=305..495  192x191  fill = 0.5026
0x0000ff       : 47104 px  bbox x=512..767 y=272..527  256x256  fill = 0.7188
```

`do_present()` centres the 256x256 render at `ox=(1280-256)/2=512`,
`oy=(800-256)/2=272`. Measured bbox is that, to the pixel, with fill 1.0.
The red bbox is the triangle apex (128,32) / base (32,224)-(224,224) mapped
into screen space, minus the rows the top-left fill rule drops; its fill 0.5026
is a triangle occupying half its bounding box.

## Why this is not a coincidence

The same run printed, from its own CPU-side readback of the rendered image
*before* any of it touched DRM:

```
[INFO] s2_coverage: triangle=18432 clear=47104 other=0 (total 65536)
[INFO] s2_checksum: FNV-1a over 262144 bytes = 0x02C0FDC5
[INFO] present: mode 1280x800
present_addfb2: PASS
present_setcrtc: PASS
```

The scanout census is `18432` / `47104` / `0` — equal to that triple, exactly,
with no tolerance. `0x02C0FDC5` is the value already pinned across x86_64/KVM,
x86_64/TCG and aarch64/TCG, so the photographed bytes are tied to the render
that had already been verified numerically. All 15 pre-committed criteria pass;
`vkrender` finished `failures = 0, skipped = 0`.

The PPMs were re-censused independently of the harness, straight from the files
on disk, and agree.

## What this does and does not prove

**Does**: an aarch64 guest drives a real host GPU through Venus, gets back a
pixel-correct image, and gets that image onto the scanout, in one process, with
no compositor. Photographed, not inferred.

**Does not**: this route is `vkCmdCopyImageToBuffer` -> host-visible memory ->
DRM dumb BO -> `SETCRTC`. There is no swapchain, no WSI, no `vkQueuePresentKHR`
and no dmabuf anywhere in it. The x86_64 M4 result went through
`vkQueuePresentKHR` into cosmic-comp; **that** path remains unmeasured on
aarch64, and this does not substitute for it.

Cost note, since the recorded blocker was cost: the whole 51-subtest run plus
the present reached its hold **7.1 s** after the command was sent, on TCG. The
Venus guest half is thin enough that TCG is not the obstacle it was assumed to
be for a compositor-free client.

## Files

* `precommit-pass-criteria.txt` — committed at `8634425`, before QEMU started
* `capA-control.png`, `capB-vkrender.png`, `capC-drmsmoke.png`
* `harness.log` — full harness transcript including the verdict table
* `serial.log` — raw guest serial for the whole session
* `../../vkrendercap.py` — the harness
