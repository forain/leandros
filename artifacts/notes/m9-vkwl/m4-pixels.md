# M4 pixels: a Vulkan client photographed on the scanout, through Venus

**2026-08-07, x86_64/KVM, Linux box, fresh images.** `vkwl` presenting into
cosmic-comp inside LeandrOS is now measured on the scanout, not inferred from
`VK_SUCCESS` return codes. The `drmsmoke` capture route carries over to a
Vulkan client unchanged.

Reproduce (both steps on the Linux box, repo root):

```
python3 scripts/mkfs-f2fs-populated.py f2fs-data0-x86_64.img x86_64
cp f2fs-data0-x86_64.img f2fs-data1-x86_64.img
python3 .claude/skills/run-leandros/driver.py stop
python3 .claude/skills/run-leandros/driver.py start x86_64 --venus
python3 .claude/skills/run-leandros/driver.py login root root
python3 -u artifacts/m9_vkcap.py /tmp/m9vk 304 150 90
```

`m9_vkcap.py` runs a positive control, sends `brush /bin/m4-vkwl sw 304 150 90`,
and takes three RFB captures on one held serial connection: A before `vkwl`
starts, B and C while it is parked on each of its last two frames.

## The census

`vkwl` clears its swapchain to a 6-colour cycle, so 304 frames end on `cols[2]`
and `cols[3]`. Both colours were predicted before the capture and printed by the
client itself in its `VKWL: HOLD READY seq=... extent=... rgb=...` sentinel.

| | A (control) | B (seq=302) | C (seq=303) |
|---|---|---|---|
| resolution | 1920x1080 | 1920x1080 | 1920x1080 |
| distinct colours | 334503 | 311411 | 300057 |
| `0x2666f2` (cols[2]) | **0** | **151868** | 14750 |
| `0xf2cc19` (cols[3]) | **0** | 0 | **151868** |
| bbox of held colour | — | x=721..1198 y=163..480 (478x318) | identical |
| bbox fill | — | 0.9991 | 0.9991 |
| vs swapchain extent 480x320 | — | 98.87% | 98.87% |
| byte-swapped colour | — | 0 | 0 |
| uncovered pixels | 0 | 0 | 0 |

The 1.13% shortfall against 480x320 is COSMIC's rounded corners and its 1 px
active-window border (`0x63d0df`, 4684 px in B), both visible in the frame.
`0xf2cc19` versus the client's round-half-up prediction `0xf2cc1a` is one LSB of
UNORM rounding on the blue channel, inside the harness's +-2 tolerance.

Two independently predicted colours landing in the *same* 478x318 rectangle,
each absent from the same-boot control, is what separates "our pixels reached
the scanout" from "something coloured was on screen".

## The carry-over held

`drmsmoke` reaches the scanout through the dumb-BO/2D path; `vkwl` reaches it
through cosmic-comp compositing a `wl_shm` buffer Mesa filled by memcpy
(`WSI_WL_BUFFER_SHM_MEMCPY`, selected by Venus reporting no
`VK_EXT_external_memory_host`). Both end at `virgl_cmd_set_scanout`, and the
same `egl-headless` + paired VNC listener photographs both. Route unchanged
from `f1bf200`; only the subject changed.

## The framebuffer console scrolls the scanout out from under the compositor

Found on the way, and it is a real bug, not a harness artifact. The framebuffer
is the primary console on LeandrOS and it scrolls the **entire** framebuffer up
on every line printed — including the region cosmic-comp is scanning out of.
cosmic-comp repaints only what is damaged, so anything static is scrolled away
and never redrawn.

Measured, first run, with `vkwl` logging one line per frame:

* distinct colours collapsed 334503 -> **177**, and 79% of the screen went pure
  black: the wallpaper was scrolled off and, being undamaged, never repainted.
* the client's four previous frame colours were smeared into bands **above** the
  current one, each band an exact multiple of the console's 15 px text row.
* the panel bar survived intact — its clock ticks every second, so it damages
  and repaints itself.

Running the client `quiet` (one line per phase instead of one per frame) is what
produced the clean census above; the single 31 px `0x2666f2` band left in
capture C is exactly the two console lines visible at the bottom of that frame
(`VKWL: HOLD END seq=302` and `VKWL: HOLD READY seq=303`). Prediction made
before the run, confirmed after: wallpaper survives, bbox fill >= 0.95, held
count drops toward the swapchain extent.

**Do not fix this by silencing clients.** The kernel console should stop
painting the framebuffer once a DRM master owns the scanout.

## Landmines

* **Do not redirect the client to a file and relay sentinels from a polling
  loop.** Tried; `cur=$(grep ... | tail -1)` in a `while` loop next to a
  backgrounded `vkwl` wedged the guest shell outright — 20 minutes of total
  console silence, no sentinel, no `M4: DONE`, not even the post-loop `cat`.
  Keep the client in the foreground on the console and cut its output at the
  source instead.
* `vkwl` unmaps its window when it exits, so a run without `hold` has nothing to
  photograph by the time you look. `hold[=secs]` parks it on each of the last
  two frames; two different colours in one boot is the whole point.
* The swapchain extent is 480x320 (`DEF_W`/`DEF_H`) because COSMIC configures
  the toplevel 0x0 and lets the client choose. If that ever changes, the harness
  reads the extent from the sentinel rather than assuming it.

Related: [[project-venus-vulkan-m4]], [[project-venus-vulkan-m2]],
[[memfd-shm-gaps]], [[console-authority]].
