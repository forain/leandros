# M4 aarch64: a Vulkan client photographed presenting into COSMIC, through Venus

**2026-08-07, aarch64 under TCG, Linux box, fresh images, `-cpu max,lpa2=off`.**
`vkwl` presenting into cosmic-comp inside LeandrOS on aarch64 is now measured on
the scanout. The last open half of M4 is closed, and the recorded blocker for it
— "COSMIC on aarch64 is impractically slow" — was **wrong by three orders of
magnitude**: the full COSMIC session bound its Wayland socket **2 seconds** after
the launch command.

Reproduce (Linux box, repo root):

```
python3 scripts/mkfs-f2fs-populated.py f2fs-data0-aarch64.img aarch64
cp f2fs-data0-aarch64.img f2fs-data1-aarch64.img
python3 .claude/skills/run-leandros/driver.py stop
python3 .claude/skills/run-leandros/driver.py start aarch64 --venus
python3 .claude/skills/run-leandros/driver.py login root root
python3 -u artifacts/m9_vkcap_a64.py /tmp/m9vk-a64-r2 session 28 180 90 30 1800
```

Pass criteria were fixed in writing before the first capture:
`precommit-pass-criteria.txt`, committed in `f65d2d3`. 28 frames instead of
x86_64's 304 because `28 == 4 (mod 6)` too, so the last two frames are still
`cols[2]` and `cols[3]` and the two arches' censuses compare directly.

## The census — run 2, full COSMIC session

| | A (control) | B (seq=26) | C (seq=27) |
|---|---|---|---|
| resolution | 1280x800 | 1280x800 | 1280x800 |
| distinct colours | 252681 | 208570 | 208570 |
| `0x2666f2` (`cols[2]`) | **0** | **151868** | 0 |
| `0xf2cc19` (`cols[3]`) | **0** | 0 | **151868** |
| bbox of held colour | — | x=401..878 y=128..445 (478x318) | identical |
| bbox fill | — | 0.9991 | 0.9991 |
| vs swapchain extent 480x320 | — | 98.87% | 98.87% |
| byte-swapped colour | — | 0 | 0 |
| uncovered pixels | 0 | 0 | 0 |

`vkCreateSwapchainKHR` returned `VK_SUCCESS` at 480x320 with **5 images**;
`frames requested=28 acquired=28 presented=28`, `queue_present_all_frames: PASS`,
exit 0. All five per-phase criteria PASS for both phases, plus the sixth: both
predicted colours occupy the **same** bounding box.

Every number that can be compared with x86_64 (`132d4df`) is **identical** —
151868 px, 478x318, 0.9991 fill, 98.87% of extent, `0xf2cc19` versus a
round-half-up prediction of `0xf2cc1a`. The two arches differ only in screen
resolution and therefore in where the window is centred.

Three further cross-feet, none of which is "no errors in the log":

* All six of `vkwl`'s cycle colours are **zero** in control A
  (`r2-session-colourcycle-probe.log`), so the harness is not matching some
  desktop colour that happens to be near the prediction.
* The panel clock reads **00:00:47 / 00:02:40 / 00:05:14** across A / B / C, so
  the three captures are three distinct live frames, not one buffer read thrice.
* B's exact-match count is 151868 of 151868 — the tolerance is doing no work
  on the blue channel of `cols[2]`.

## What "impractically slow" actually was

Measured host-side from sentinel arrival, so a blocked guest cannot fake it:

| step | standalone cosmic-comp | full COSMIC session |
|---|---|---|
| launch -> `wayland-1` bound | **1 s** | **2 s** |
| `vkwl` start -> 28th present | ~6 s | ~3 s |
| present sentinel -> pixels on the scanout | <= 6 s | 6-26 s |

The COSMIC session is not slow on aarch64/TCG. The 51-subtest `vkrender` timing
(`dc013c0`) already said the Venus guest half was cheap here; nothing about the
compositor contradicted it, and nothing had measured it.

## One sample per hold is not enough, and run 1 proves it

Run 1 (standalone compositor, `r1-comp-*`) **FAILED its own pre-committed
criteria** and is kept here unedited, because how it failed is the finding.

It sampled each hold once, 6 s after `VKWL: HOLD READY`. Phase C passed
perfectly. Phase B showed a solid rectangle of `cols[1]` — 151868 px, bbox
x=401..878 y=101..418, 478x318, fill 0.9991 — i.e. **`vkwl`'s own previous
frame, in the identical rectangle, at the identical count and fill.**

`VKWL: HOLD READY seq=N` means the *client* has returned from
`vkQueuePresentKHR` for frame N. It does not mean the compositor has composited
that buffer and the scanout has been flipped to it. `vkwl` bursts all 28 frames
in a few seconds and then parks; the compositor is still draining when the first
sample lands. Phase C only passed because the 180 s hold on seq=26 had left the
compositor completely idle beforehand.

**A single sample cannot distinguish "the pixels never arrive" from "the pixels
had not arrived yet", and those are opposite conclusions.** So the sample became
a series (`sample_hold`) and the ambiguity became a number. In run 2 the
progression is even starker than run 1's off-by-one: at +6 s the window is **not
on the screen at all** (`r2-session-capB-seq26-sample1-t6s.png` is a bare
desktop, all six cycle colours zero), and at +26 s it is there in the correct
colour. That is first-map-plus-composite latency, not a per-frame lag.

Every sample is written out, not just the scored one — a series that starts
wrong and ends right *is* the evidence for the latency, and discarding the early
frames would hide it.

## Landmines

* **`HOLD_RE` can match a partially arrived line.** The serial link delivers
  8 bytes at a time, and phase C in run 2 matched with `secs=1` still mid-line,
  which set that hold's sampling budget to zero. It happened to pass on sample
  #1; had it not, the harness would have scored a hold it never really sampled.
  Anchor the sentinel regex on a line end before reusing this.
* **`screendump` remains structurally incapable** at any `device=`, and no
  amount of arch-switching changes that — `qemu_console_surface()` returns NULL
  for `SCANOUT_TEXTURE`. The route is `-display egl-headless` plus a paired
  `-vnc ...,display=venusgpu` (`f1bf200`).
* The `1.13%` shortfall against 480x320 is COSMIC's rounded corners and its 1 px
  active-window border (`0x63d0df`, 4684 px — the same count as x86_64), both
  visible in the captures.
* The framebuffer console no longer scrolls the scanout out from under the
  compositor (`edad115`). This run **relies on** that fix rather than re-proving
  it; `vkwl` is still run `quiet`, so it is not a test of it either.

Related: [[project-venus-vulkan-m4]], [[project-venus-vulkan-m2]],
[[console-authority]], [[gpu-accel-lane]].
