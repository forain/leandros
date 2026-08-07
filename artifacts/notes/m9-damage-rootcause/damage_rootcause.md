# M9 — why COSMIC damages 96.7 % of the output every present

**Lane K, 2026-08-06. Source-analysis only; no QEMU was run.**
Inputs: `notes/m9-fb-damage-clips/diag-report-20260806.md`, `run1-drmstat.txt`,
`shots/run1-quiet1.ppm`, TODO.md items 9 and 12, smithay `efeb597`
(`~/.cargo/git/checkouts/smithay-312425d48e59d8c8/efeb597`, pinned by
`cosmic-comp/Cargo.lock:4816`), `~/code/cosmic-epoch/cosmic-comp` @ `dec1ee86`.

---

## Verdict in one paragraph

**Buffer age is not the mechanism, and the swapchain does not reallocate per frame.**
Both halves of TODO item 12's unverified claim are refuted — the first from smithay's
source, the second from the diagnostic's own numbers, which contain the age-0 fallback's
exact fingerprint and show it firing **twice, at t≈4 s, and never again**. What actually
reaches the kernel is a genuine, element-driven damage set that spans nearly the whole
output, then gets rounded *up* by smithay's `DamageShaper`. The residual unknown is which
cosmic-comp element produces it; that is one cheap kernel-side print away, because
`PlaneDamageClips::from_damage` copies the shaper's output into the blob **1:1**, so the
blob we already decode *is* the damage tracker's verbatim output.

---

## 1. The age hypothesis is refuted — two independent lines

### 1a. Source: the swapchain cannot reallocate a populated slot

`allocator/swapchain.rs:154-181` (`Swapchain::acquire`) takes the first slot whose
`acquired` flag was false and calls `allocator.create_buffer` **only** inside
`if free_slot.buffer.is_none()`. There is no path that drops a buffer on the way out.
The only three ways a slot loses its buffer are `resize()` (`:206-215`),
`reset_buffers()` (`:217-221`) and `reset_buffer_ages()` (`:241-250`).

`submitted()` (`:188-204`) stores `age = 1` on the submitted slot and increments every
other slot that holds a buffer. `DrmCompositor` calls it from `queue_frame`
(`backend/drm/compositor/mod.rs:2459`) and `commit_frame` (`:2505`). cosmic-comp holds at
most two slots at a time (`QueueState::WaitingForVBlank` gates the next render), so the
steady state is a two-slot rotation with **age = 2 every frame**, and
`last_state.old_damage` sits at 2–3 entries against `MAX_AGE = 4`
(`renderer/damage/mod.rs:125`). The condition at `renderer/damage/mod.rs:741`
(`age > 0 && old_damage.len() >= age`) is satisfied.

The only caller-side resets are `cosmic-comp/src/backend/kms/surface/mod.rs:1399`
(`compositor.reset_buffers()` in the `Err` arm of `render_frame`, which `bail!`s — no
flip is produced) and smithay's own `compositor/mod.rs:2343` (same, on render error).
Neither can fire on a frame that reaches the kernel.

**`gbm_bo_create_with_modifiers2` is irrelevant to this question either way.** Whether
smithay's `backend_gbm_has_create_with_modifiers2` feature is on or off (it is decided by
`build.rs` probing `gbm` with `test_gbm_bo_create_with_modifiers2.c`),
`allocator/gbm.rs:200-238` only changes *how a buffer is allocated the first time*, and
its Invalid/Linear fallback means the allocation succeeds. A failure would surface as
`FrameError::Allocator` out of `acquire`, i.e. **no flip at all**, not a degraded one. The
"reallocates per frame" step in item 12's chain does not exist.

### 1b. Data: the fallback's fingerprint is present, dated, and rare

All three of smithay's "damage everything" branches push **exactly one rect equal to
`output_geo`**:

- `renderer/damage/mod.rs:668-685` — output geometry/transform/clear-colour changed
- `renderer/damage/mod.rs:747-759` — the `age`/`old_damage` fallback
- `backend/drm/compositor/mod.rs:2321-2331` — clearing a previous direct scan-out
  (this one leaves `damage_clips = None`, so our kernel counts it as `dmg_full`)

`shape_damage` passes a one-element input straight through (`shaper.rs:56-59`), and
`PlaneDamageClips::from_damage` (`backend/drm/surface/mod.rs:68-100`) is a 1:1 `map` — no
splitting, no merging, and with `src == dst` here the transform is the identity. So a
fallback frame **must** land in the blob as one rect of exactly 1 280 × 800 =
**1 024 000 px = 0xFA000**.

Re-reading `run1-drmstat.txt` per sample interval:

| window | presents | dmg_px / present | what it is |
|---|---|---|---|
| t = 2→4 s | 2 rect | **1 024 000 (100.00 %)** | the age-0 fallback, exactly as predicted |
| t = 12→98 s (idle) | 2 per 2 s | **40 960 (4.00 %)** | 1 280 × 32 — the panel bar, i.e. the clock |
| t = 100→168 s (motion) | 16–17 per 2 s | **992 000 / 981 760 (96.88 / 95.88 %)** | the thing we are chasing |

Three things fall out of this table and each is on its own sufficient:

1. The age-0 fallback happened **twice, during bring-up**, and produced exactly
   1 024 000 px. It never recurs in 176 s.
2. During the 86 s idle stretch, with the *same* swapchain and the *same* slot rotation,
   damage tracking works perfectly: one 1 280 × 32 rect per present. If age were
   structurally 0 this would be 1 024 000 too.
3. The burst value is **992 000, not 1 024 000**. No fallback branch can emit that
   number.

The diag report's line "That directly confirms the inference recorded in TODO.md item 9 —
we fail the third skip condition" is **wrong**, and its own raw data disproves it.
`dmg_skip = 0` means smithay never returned *empty* damage; it says nothing about `age`.

---

## 2. What actually forces the full-damage path

### 2a. The kernel sees the damage tracker's output verbatim

`compositor/mod.rs:2306-2315` builds the blob from `render_output_result.damage`, which is
`&self.damage` after `renderer/damage/mod.rs:774` `self.damage_shaper.shape_damage(...)`.
`from_damage` preserves both the rect count and each rect's area. **The blob is the
shaper's output, one rect for one rect.** That is what makes the runtime check in §4
decisive rather than suggestive.

### 2b. The shaper is an area-inflating quantiser, and the numbers are its arithmetic

`DamageShaper` (`renderer/damage/shaper.rs`) has two inflating modes:

- **bbox shortcut, `:81-88`** — if the largest input rect exceeds
  `MAX_DAMAGE_TO_DAMAGE_BBOX_RATIO = 0.9` of the current bounding box, emit the *bounding
  box*.
- **tiled path, `:158-174` + `:186-249`** — reached when the rects overlap in projection
  on both axes so no split point exists. The bbox is cut into `NUM_TILES = 4` by
  `NUM_TILES * 2 = 8` tiles; each tile is inflated to the bbox of the damage clipped to
  it; vertically adjacent tiles in a column are then merged to *their* bbox
  (`:229-241`).

For a full-output bbox on this display the tile grid is exactly **320 × 100, 32 tiles**
(1280/4, 800/8). The observed numbers are that grid:

- `992 000 = 31 × 32 000` — 31 of 32 tiles
- `981 760 = 992 000 − 10 240`, and `10 240 = 320 × 32` — one tile short by 32 rows at
  the tile column width

Equivalently (the two readings agree on the geometry): the damaged region is
**full width and 767–775 rows of 800**. 1 280 × 775 = 992 000 and 1 280 × 767 = 981 760.
Note the idle rect, 1 280 × 32, is a `len() == 1` passthrough — no shaping — which is why
idle reads clean.

### 2c. What is on the primary plane, and what is not

From `shots/run1-quiet1.ppm`: rows 0–31 are the panel bar (27, 27, 27); everything below
is the Orion wallpaper. **No windows.** So the damage tracker's element list is two
elements: the panel layer surface and the wallpaper layer surface.

The cursor is *not* one of them, and this is settled from source, not inferred:
`compositor/mod.rs:2036` puts a cursor-plane assignment into a separate
`cursor_plane_element` slot, and only `overlay_plane_elements` (`:2207-2232`) is fed back
into the damage tracker as fake elements. The only path that can push it back onto the
primary is the failed-`test_state_complete` reset at `:2082-2131` — and `atest = 0` for
the entire burst (6 for the whole 176 s run), so no test ever failed. `curs_up = 1` and
`curs_mv = 680 ≈ atomic = 684` confirm the plane is live and being repositioned once per
commit.

### 2d. What that leaves — stated honestly

With the cursor on hardware and two static elements on the primary, the damage tracker
*should* return empty and smithay should skip. It never does (`dmg_skip = 0`), and during
motion it returns a set spanning the output. Everything above narrows the cause to:

> one of the two primary-plane elements (overwhelmingly the wallpaper, the only
> near-output-sized one) reports damage on essentially every present **while and only
> while the pointer is moving**, and it reports it as **several rects forming a chain
> across the output**, not as one big rect.

The "several rects, not one" part is forced: a single rect covering the wallpaper would
trip the `> 0.9` bbox shortcut and emit exactly 983 040 or 1 024 000 px. We measure
neither.

I could not identify the *specific* cosmic-comp element or code path from source in this
lane, and I am not going to invent one. The candidates I checked and eliminated:

- **New `Id` per frame** — `NamespacedElement::new` (`element/mod.rs:717-732`) reuses the
  inner id via `Id::namespaced`; ids are `WaylandResource`-derived and stable
  (`element/mod.rs:167-215`). Also excluded by the idle behaviour: a churning id would
  full-damage the wallpaper when idle too.
- **z-index shift** from the cursor entering/leaving the primary list — excluded by
  `atest = 0` (§2c).
- **`instance_matches` mismatch on the wallpaper** — would push the element geometry plus
  the last instance geometry, two identical rects, `max/bbox = 1.0` → bbox shortcut →
  exactly the wallpaper area. Not what we measure.
- **cosmic-bg redrawing on frame callbacks** — would also fire during idle (the
  compositor sends callbacks after every `queue_frame`,
  `kms/surface/mod.rs:1370-1375`); idle is clean, so cosmic-bg is quiet.
- **`reset_buffers` / `reset_buffer_ages` in cosmic-comp** — only the error path
  (`kms/surface/mod.rs:1399`), which aborts the frame.

Two live candidates I could not eliminate, both of which produce *chains* of rects:

- **age-based accumulation feeding the tiled path.** `damage/mod.rs:745-746` adds the
  previous frame's damage to this frame's. A few full-width bands at different `y`,
  unioned across two frames, is precisely the "no split point on either axis" input that
  routes to the tiled shaper and comes out as ~31/32 tiles. This is upstream smithay
  behaviour, not a LeandrOS defect.
- **the opaque-region diff at `damage/mod.rs:657-670`.**
  `Rectangle::subtract_rects_many_in_place` emits band decompositions — many full-width
  slivers — and its result is added to the damage *and* sets `force_effect_redraw`. It
  fires whenever last frame's opaque regions are not a subset of this frame's. cosmic-comp
  passes `blur_strength` into every surface-tree walk (`render/mod.rs:730, 866, 915`) and
  smithay `efeb597` has the framebuffer-effect path that mutates `self.opaque_regions`
  in place (`damage/mod.rs:686-729`) before storing them into `last_state`.

---

## 3. Is it fixable from our side?

**No.** Every decision above is made inside `OutputDamageTracker` and `DamageShaper`,
entirely in the compositor's address space, before a single byte reaches the DRM
interface. There is no feedback path from the driver into the damage tracker. Nothing in
our kernel, our GBM shim, our Mesa build config, or the environment participates.

The `--no-default-features` lever is not applicable either: the shaper is unconditional
(`damage/mod.rs:774`), not feature-gated.

Fixing it would require changing COSMIC or smithay. Stating that plainly as a feasibility
finding, per the lane's constraint: **a real fix here is a COSMIC/smithay source change,
which the project's standing goal forbids.** The pragmatic consequence is that the
FB_DAMAGE_CLIPS work in item 9 has no perf headroom to recover and should be landed (or
not) purely on the merits of the kernel-side defect it fixes, never on flips/s.

The one thing genuinely worth knowing is whether the damage set is *legitimately* big
(the compositor really did change those pixels) or whether the shaper is inflating a few
hundred pixels into a million. §2b says inflation is happening; §4 measures how much. If
it turns out to be, say, six small rects inflated to 31 tiles, that is an upstream smithay
bug report with a reproducer, which is a real outcome even under a no-patch policy.

---

## 4. The runtime check that settles it

One counter plus a bounded dump, added to the *existing* item-9 diagnostic patch. Prepared
as `damage_rect_dump.patch` in this directory — it applies **on top of**
`notes/m9-fb-damage-clips/fb_damage_worktree_20260806.patch`, touches only
`drivers/src/drm_device_interface.rs`, and adds nothing outside the `DRM_STATS` gate.
Verified: `git apply --check` clean against `a0f2c46` + the base patch, and
`cargo build --release -p drivers` succeeds for **both** `aarch64-unknown-none` and
`x86_64-unknown-none` with no new warnings. Unlike the base patch, this one is **built**.
The worktree it was produced in was restored clean; nothing is left in `drivers/`.

**What it prints.** A new `dmg_nrects=` field on the `[DRMSTAT]` line (total rects across
all rect-path presents), and for the first 12 presents whose decoded area exceeds half the
surface, one line:

```
[DMGRECTS] n=0x1F r=0x0,0x0,0x140,0x64 r=... [/DMGRECTS]
```

**What confirms what.**

| observation | conclusion |
|---|---|
| `n = 1`, rect = `(0,0,1280,800)` | I am wrong: it *is* a full-output fallback after all. Reopen the age question — but note this contradicts the t≈4 s samples. |
| `n = 1`, rect full-width, height 767–775 | shaper bbox shortcut, `shaper.rs:81-88`. The pre-shaper set is ≥2 rects, one of them ≥ 892 800 px. |
| `n` in 4…32, rects 320 px wide or column strips | tiled path, `shaper.rs:186-249`. Read the rect list directly to see which regions the compositor believes changed. |
| `dmg_nrects / dmg_rect` ≈ 1 during idle **and** ≫ 1 during motion | the inflation is motion-specific; hand the rect list to the next lane as an upstream reproducer. |

**How it fails loudly rather than plausibly** — three lanes were burned by silent
instruments today, so:

1. **`n` is printed before the rects and a `[/DMGRECTS]` sentinel after them.** The
   analyser must assert that the number of parsed `r=` tuples equals `n` **and** that the
   sentinel is present, and abort on mismatch. A truncated or interleaved serial line then
   errors instead of under-reporting.
2. **Built-in positive control with a known-good answer.** The dump threshold is
   `area > W*H/2`, so it will not fire on idle presents — therefore the analyser must
   *also* check the idle invariant we already know from this run: over any 20 s window
   with `evpush` delta 0, `dmg_nrects − dmg_rect` must be **0** (one rect per present) and
   `dmg_px / dmg_rect` must be exactly **40 960**. If idle does not reproduce, the
   instrument is wrong and the motion numbers must be thrown away. This is the leg the
   "guard test that could not fail" was missing.
3. **Cross-foot against the existing counters.** `dmg_nrects ≥ dmg_rect` must hold
   identically. `dmg_nrects == 0` while `dmg_rect > 0` means the counter was never wired —
   hard error, not a zero.
4. `MAX_DAMAGE_RECTS = 64` already caps the decode, and `dmg_full` already counts blobs
   that exceed it, so a blob with more than 64 rects cannot masquerade as a small one.

**Separately, a one-line check that kills the age hypothesis by observation** rather than
by inference, for anyone who wants it belt-and-braces: count `ADDFB2` calls
(`fbs_added=`). Per-frame swapchain reallocation is impossible without a new framebuffer
object per frame.

- `fbs_added` total **≤ ~8 for the whole run** ⇒ four swapchain slots plus cursor, buffers
  are reused, age > 0. Confirms §1.
- `fbs_added` climbing at **≈ flips/s (8/s)** ⇒ reallocation is real and §1 is wrong.

Prediction: ≤ 8. I would be surprised to be wrong, but this is the number that would make
me wrong, and it costs one `AtomicU64`.

---

## 5. What remains unknown

- **The specific cosmic-comp element and code path** that emits per-present damage during
  pointer motion. Narrowed to "the wallpaper or panel element, as several rects", with two
  surviving mechanisms (§2d). Not resolvable from source at reasonable cost; the §4 rect
  dump resolves it in one run.
- **Which of the two shaper inflation modes is active** (bbox shortcut vs tiled). Both fit
  the aggregate area; only the rect count separates them. Same single run.
- **Why the damaged height wobbles between 767 and 775 rows.** Consistent with a small
  second rect near one edge moving by a few pixels, which is suggestive of pointer
  tracking, but I have no evidence for that beyond the arithmetic.
- **Whether the pre-shaper damage is genuinely large or a handful of rects inflated.** This
  is the question that decides whether there is an upstream bug worth reporting. The rect
  dump gives the shaper's *output*; the *input* is not visible from the kernel at all, so
  even after this check the input remains inferred. That is a hard ceiling —
  `release_max_level_info` in `cosmic-comp/Cargo.toml:61-62` compiles out the `trace!` at
  `damage/mod.rs:742, 748, 2293, 2318` that would show it.
- The diag run is still a **single run**, and `drmsmoke` / `idletest` were never completed
  (diag report, Caveats). Nothing here changes that.

---

## Corrections to existing documents

- **TODO.md item 9**, the paragraph beginning "**Inferred, well-supported:** we fail that
  third condition" — that inference is now **refuted**. Suggested replacement: "the
  `age`/`old_damage` condition is satisfied; the damage is element-driven and inflated by
  `DamageShaper`."
- **TODO.md item 12**, the "Mesa modifier support" bullet — the per-frame-reallocation
  claim can be **deleted as refuted**, on the strength of `swapchain.rs:154-181` plus the
  idle counters. (The separately-observed 128-dmabuf-fd burn and the `MAX_FDS` 64→128
  raise are untouched by this and still stand.)
- **`notes/m9-fb-damage-clips/diag-report-20260806.md`**, the Verdict paragraph "That
  directly confirms the inference recorded in TODO.md item 9" — this does not follow;
  `dmg_skip = 0` says the tracker never returned *empty* damage, which is independent of
  `age`. The report's own `dmg_px` column contradicts it.
