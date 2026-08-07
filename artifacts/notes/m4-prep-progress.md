# M4 input-ship prep — progress checkpoint

Lane: HOST-ONLY, repo-read-only. Writes only under ~/code/leandros-artifacts/m4-input-ship/
and ~/code/leandros-artifacts/notes/.

## STATUS

### Step 1 — anvil runtime manifest derivation: DONE
Read m3-gl-stack/NOTES.md. anvil direct NEEDED (8): libxkbcommon.so.0
libdisplay-info.so.3 libgbm.so.1 libseat.so.1 libudev.so.1 libinput.so.10
libpixman-1.so.0 libc.so. Transitively via libinput: libmtdev.so.1 libevdev.so.2.
libgbm.so.1 itself is part of the GL ship set (M3, not M4 — anvil links it
directly though, so it must already be on-image from M3 packing). backend_egl
dlopens the whole GL stack (already M3's problem, not re-staged here).

### Step 2 — libudev shim flip: DONE, both arches built + verified
SURPRISE (see report): the mission's stated premise — "shim currently models
the pointer device as ID_INPUT_TOUCHSCREEN on event1" — did NOT match the
source. Read ports/input-stack/shims/libudev/libudev.c (only version that has
ever existed, single commit 3a3120a, confirmed via git log -p): event1 was
ALREADY props_mouse/ID_INPUT_MOUSE. event2 was the one modeled as
ID_INPUT_TOUCHSCREEN. Cross-checked against the built .so in
m3-gl-stack/sysroot-x86_64 via strings — matches source, not stale-binary.
Cross-checked kernel truth: userland/evtest2/src/main.rs docstring + the
Facts in this task both say the real device set is event0=keyboard,
event1=virtio-tablet ONLY — no third input node exists on the image.
ACTUAL bug: the shim's g_devices table still modeled a phantom event2
touchscreen (+ input2 parent) that no real kernel device backs. Fixed by
DELETING that phantom entry (props_touch, the event2 dev_desc row, the
input2 parent row) and documenting event1's classification rationale
(ABS+BTN_LEFT, no INPUT_PROP_DIRECT -> mouse not touchscreen) in the header
comment. This is the real, meaningful "flip" for M4: previously libinput's
udev_enumerate_scan_devices() would have surfaced a nonexistent
/dev/input/event2 and libinput would try (fail) to open it.
Files:
  - patched source: ~/code/leandros-artifacts/m4-input-ship/src/libudev-shim/{libudev.c,libudev.h,libudev.map}
  - diff: ~/code/leandros-artifacts/notes/m4-libudev-flip.patch (git apply --check verified clean against ports/input-stack/shims/libudev/libudev.c, NOT applied to the repo)
  - built both arches: ~/code/leandros-artifacts/m4-input-ship/{x86_64,aarch64}/usr/lib/libudev.so.1.0.0 (+ .so.1, .so symlinks)
  - verified: ET_DYN, correct machine (X86-64 / AArch64), SONAME=libudev.so.1,
    NEEDED=libc.so only, ID_INPUT_MOUSE/ID_INPUT_KEYBOARD strings present,
    ID_INPUT_TOUCHSCREEN / event2 strings ABSENT from the built binary.

### Step 3 — stage per-arch ship set: DONE
Staged both arches: libseat, libinput, libevdev, libmtdev, libxkbcommon,
libpixman-1, libdisplay-info (real .so + soname symlink + devlink each) from
m3-gl-stack/sysroot-<arch>/usr/lib; libudev.so.1* from the flipped build
(step 2). Full xkeyboard-config 2.44 data tree copied from
~/.claude-forain/jobs/afde2e74/tmp/d3-input-stack/ship-set/usr/share/X11/xkb
into both arch dirs (3.8MB each, shipped whole not pruned — symbols files
`include` each other transitively). BONUS FIND: libinput's quirks DB
(usr/share/libinput/*.quirks, 50 files/216KB, incl. 30-vendor-qemu.quirks)
existed in the x86_64 sysroot but was MISSING from aarch64's (asymmetric
build — aarch64 only hand-installed lib+header+pc per the CLI-tool landmine,
skipping the data-file install step x86_64 got via full meson install).
Backfilled aarch64 from the x86_64 copy since it's plain-text data, not ELF.

### Step 4 — closure verification: DONE
Wrote ~/code/leandros-artifacts/m4-input-ship/verify-closure.sh (read-only
against both source trees). anvil's full transitive DT_NEEDED CLOSED both
arches, zero missing, against union of {m4-input-ship staged set} ∪
{m3-gl-stack sysroot for libgbm/libdrm/libc}. Every staged real .so
(8 per arch) verified ET_DYN + correct machine (X86-64/AArch64).

### Step 5 — write m4-ship-manifest.md: DONE
~/code/leandros-artifacts/notes/m4-ship-manifest.md — full file manifest,
exclusions + reasons, closure verification output, size estimate
(x86_64 ~11MB / aarch64 ~10MB added, 350 files + 16 symlinks each), and the
proposed mkfs-f2fs-populated.py addition (illustrative snippet, script not
read/touched — repo-read-only).

## ALL TASKS COMPLETE. Nothing left to resume.

## Landmines / notes for resume
- m3-gl-stack toolchain cc wrappers at ~/code/leandros-artifacts/m3-gl-stack/toolchain/<arch>-linux-musl-cc work fine standalone (just zig cc -target wrapper, no dependency on the old tmp job path).
- closure.sh / verify-elf.sh hardcode D=/Users/forain/.claude-forain/jobs/afde2e74/tmp/m3-gl-stack — that directory STILL EXISTS on disk (live working copy, separate from the ~/code/leandros-artifacts/m3-gl-stack copy) and has matching sysroot-<arch> content, so those scripts run as-is without modification. Do not edit them (read-only lane discipline extends to not needing changes here anyway).
- XKB data is NOT in the merged sysroots (sysroot-<arch> only has xkbcommon.h headers + libxkbcommon.so, no /usr/share/X11/xkb). It only exists at the d3-input-stack job tmp dir's ship-set/. This is the source to copy from.
- anvil binary itself lives at m3-gl-stack/out/anvil-<arch> (DO NOT COPY INTO m4-input-ship — mission says stage only the input/XKB dependency set, anvil binary itself is M3's/orchestrator's to pack).
