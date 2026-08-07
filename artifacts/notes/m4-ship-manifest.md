# M4 (anvil) input-stack ship manifest

Staged at `~/code/leandros-artifacts/m4-input-ship/{x86_64,aarch64}/`, mirroring
on-image paths under each arch dir (i.e. `x86_64/usr/lib/foo.so` ships to the
x86_64 image's `/usr/lib/foo.so`). Ready for the orchestrator to fold into
`scripts/mkfs-f2fs-populated.py` once M3 (Mesa userspace crash) unblocks.

## 1. anvil's full runtime dependency closure (derived + verified)

anvil direct `DT_NEEDED` (8, unchanged both arches):
`libxkbcommon.so.0 libdisplay-info.so.3 libgbm.so.1 libseat.so.1 libudev.so.1
libinput.so.10 libpixman-1.so.0 libc.so`

Transitive (pulled in by libinput): `libmtdev.so.1 libevdev.so.2`

`libgbm.so.1` + `libc.so` are M3's problem (GL ship set / base musl), already
staged by that lane — NOT duplicated here. Everything else in the closure
above IS staged in this lane. anvil's `backend_egl` additionally `dlopen`s the
whole GL stack (libEGL -> libgallium -> libwayland-* -> libffi -> libdrm ->
GLESv2 -> dri_gbm.so) at runtime — that's the M3 GL ship set's contract, not
this one; it is exercised in this closure check only as pre-existing sysroot
content, not re-staged.

## 2. On-image file manifest, per arch (identical file set both arches; content differs only by ELF machine)

```
usr/lib/libxkbcommon.so.0.8.0        (real; ELF; SONAME libxkbcommon.so.0)
usr/lib/libxkbcommon.so.0            -> libxkbcommon.so.0.8.0
usr/lib/libxkbcommon.so              -> libxkbcommon.so.0
usr/lib/libdisplay-info.so.0.3.0     (real; ELF; SONAME libdisplay-info.so.3)
usr/lib/libdisplay-info.so.3         -> libdisplay-info.so.0.3.0
usr/lib/libdisplay-info.so           -> libdisplay-info.so.3
usr/lib/libseat.so.1.0.0             (real; ELF; SONAME libseat.so.1)
usr/lib/libseat.so.1                 -> libseat.so.1.0.0
usr/lib/libseat.so                   -> libseat.so.1
usr/lib/libudev.so.1.0.0             (real; ELF; SONAME libudev.so.1 — FLIPPED, see §3)
usr/lib/libudev.so.1                 -> libudev.so.1.0.0
usr/lib/libudev.so                   -> libudev.so.1
usr/lib/libinput.so.10.13.0          (real; ELF; SONAME libinput.so.10)
usr/lib/libinput.so.10               -> libinput.so.10.13.0
usr/lib/libinput.so                  -> libinput.so.10
usr/lib/libpixman-1.so.0.44.2        (real; ELF; SONAME libpixman-1.so.0)
usr/lib/libpixman-1.so.0             -> libpixman-1.so.0.44.2
usr/lib/libpixman-1.so                -> libpixman-1.so.0
usr/lib/libmtdev.so.1.0.0            (real; ELF; SONAME libmtdev.so.1)
usr/lib/libmtdev.so.1                -> libmtdev.so.1.0.0
usr/lib/libmtdev.so                  -> libmtdev.so.1
usr/lib/libevdev.so.2.3.0            (real; ELF; SONAME libevdev.so.2)
usr/lib/libevdev.so.2                -> libevdev.so.2.3.0
usr/lib/libevdev.so                  -> libevdev.so.2

usr/share/X11/xkb/{rules,compat,types,symbols,geometry,keycodes}/...
    full xkeyboard-config 2.44 data tree (3.8 MB) — shipped whole rather than
    pruned to "rules/evdev + symbols/us" because xkbcommon's symbols files
    `include` each other transitively (us -> pc, latin, etc.) and pruning
    risks a broken include chain for a few hundred KB of savings.

usr/share/libinput/*.quirks (50 files, 216 KB)
    libinput's runtime quirks DB (vendor + system + generic quirks, incl.
    30-vendor-qemu.quirks — directly relevant, has a QEMU/KVM mouse-
    integration stanza). See §4 for the aarch64 backfill note.
```

Not shipped here (excluded, with reason):
- `usr/libexec/libinput/*` (18 CLI tools: libinput-list-devices, -record,
  -replay, -measure-*, -analyze-*, -debug-*, -quirks, -test) — these are
  developer/debug tools, not part of anvil's runtime dependency closure.
  Also: per m3-gl-stack/NOTES.md landmine (b), the aarch64 build of these
  specific tools FAILS at link time (zig/lld `R_AARCH64_LDST64_ABS_LO12_NC`
  alignment bug hitting `.rodata.str1.1` help-text strings) — the library
  itself is unaffected. Not shipping them sidesteps that landmine entirely
  since anvil never calls them.
- `etc/libinput/` — empty (0 B) in the built sysroot; nothing to stage.
- GL stack (libEGL/libGLESv2/libgbm/libgallium/libdrm/libwayland-*/libffi/
  libexpat/libz) and libc/musl loader — M3's ship set, out of this lane's
  scope (m3-gl-stack/out/ is explicitly off-limits to write into).

## 3. libudev shim flip (task 2)

**Mission-stated premise did not match the source.** The task described the
needed fix as "shim currently models the pointer device as
ID_INPUT_TOUCHSCREEN on event1 → flip to ID_INPUT_MOUSE." Direct read of
`ports/input-stack/shims/libudev/libudev.c` (only version that has ever
existed — confirmed via `git log -p`, single commit `3a3120a`) plus `strings`
on the already-built `libudev.so.1.0.0` in `m3-gl-stack/sysroot-*` both show
event1 was **already** `props_mouse` / `ID_INPUT_MOUSE`; the entry actually
tagged `ID_INPUT_TOUCHSCREEN` was **event2**, not event1.

Cross-checking kernel truth (Facts + `userland/evtest2/src/main.rs` docstring):
the real device set is `event0=keyboard, event1=virtio-tablet` — **no third
input node exists**. So the actual staleness was: the shim's synthetic device
table still modeled a **phantom `event2` touchscreen** (+ its `input2` parent)
that no longer corresponds to any device the kernel exposes. A libinput udev
enumeration walking that table would surface a nonexistent
`/dev/input/event2` and fail to open it.

**Fix applied** (in the staged copy, NOT the repo):
- Removed `props_touch[]`, the `event2` `dev_desc` row, and the `input2`
  parent row from `g_devices[]`.
- Rewrote the header doc comment to state the real 2-node device set and
  document event1's classification rationale (absolute ABS 0..32767 +
  BTN_LEFT, no `INPUT_PROP_DIRECT` → pointer/mouse, not touchscreen).
- `libinput` reads `ID_INPUT`/`ID_INPUT_MOUSE`/`ID_INPUT_TOUCHSCREEN`/etc as
  udev tags in `src/evdev.c`'s `evdev_udev_tag_match[]` — confirmed against
  the libinput 1.27.1 source tree, so the property choice matters for real,
  it just was already correct on event1.

Files:
- Patched source: `~/code/leandros-artifacts/m4-input-ship/src/libudev-shim/{libudev.c,libudev.h,libudev.map}`
- Diff (git-apply-ready, `a/`+`b/` paths, verified with `git apply --check`
  against the live repo — **not applied**): `~/code/leandros-artifacts/notes/m4-libudev-flip.patch`
- Built both arches: `~/code/leandros-artifacts/m4-input-ship/{x86_64,aarch64}/usr/lib/libudev.so.1.0.0`
  (+ `.so.1` / `.so` symlinks), using `m3-gl-stack/toolchain/<arch>-linux-musl-cc`
  with the same invocation as `ports/input-stack/build-shims.sh`
  (`-shared -fPIC -O2 -std=c11 -D_GNU_SOURCE -Wl,-soname,libudev.so.1
  -Wl,--version-script libudev.map`).
- Verified per arch: ET_DYN, correct `EM_X86_64`/`EM_AARCH64`, SONAME
  `libudev.so.1`, `NEEDED` = `libc.so` only, `ID_INPUT_MOUSE`/
  `ID_INPUT_KEYBOARD` strings present, `ID_INPUT_TOUCHSCREEN`/`event2` strings
  **absent** from the built binary (confirmed via `strings`).

## 4. Closure verification

Custom verifier (read-only against both source trees, does not touch
m3-gl-stack/out/): `~/code/leandros-artifacts/m4-input-ship/verify-closure.sh
<arch> <elf>`. Resolves anvil's full transitive `DT_NEEDED` against the union
of `{m4-input-ship/<arch>/usr/lib}` (this lane, libudev flipped) and
`{m3-gl-stack/sysroot-<arch>/usr/lib}` (M3 base+GL set), M4 taking precedence.

**x86_64: CLOSED, zero missing.**
```
libxkbcommon.so.0     [m4-input-ship]  -> libxkbcommon.so.0.8.0
libdisplay-info.so.3  [m4-input-ship]  -> libdisplay-info.so.0.3.0
libgbm.so.1           [m3-gl-stack]    -> libgbm.so.1.0.0
libseat.so.1          [m4-input-ship]  -> libseat.so.1.0.0
libudev.so.1          [m4-input-ship]  -> libudev.so.1.0.0      (flipped)
libinput.so.10        [m4-input-ship]  -> libinput.so.10.13.0
libpixman-1.so.0      [m4-input-ship]  -> libpixman-1.so.0.44.2
libc.so               [m3-gl-stack]
libdrm.so.2           [m3-gl-stack]    -> libdrm.so.2.134.0     (via libgbm)
libmtdev.so.1         [m4-input-ship]  -> libmtdev.so.1.0.0     (via libinput)
libevdev.so.2         [m4-input-ship]  -> libevdev.so.2.3.0     (via libinput)
```

**aarch64: CLOSED, zero missing.** Identical closure shape, all libs resolve,
all `Machine: AArch64`.

Additional per-arch ELF sanity sweep (every real, non-symlink `.so` staged):
all 8 files both arches are `Type: DYN (Shared object file)`, correct
`Machine:` (`Advanced Micro Devices X86-64` / `AArch64`). No host (macOS)
libraries anywhere in the closure.

## 5. Size estimate (image margin)

| arch    | usr/lib | usr/share/X11/xkb | usr/share/libinput | **total added** |
|---------|---------|--------------------|--------------------|--------------|
| x86_64  | 6.8 MB  | 3.8 MB             | 216 KB             | **~11 MB**   |
| aarch64 | 6.4 MB  | 3.8 MB             | 216 KB             | **~10 MB**   |

350 regular files + 16 symlinks staged per arch (the file count is dominated
by the xkeyboard-config data tree — hundreds of small per-layout symbol
files). f2fs inode/metadata overhead on that many small files is a minor but
nonzero addition on top of the raw byte totals above; worth a quick check
against remaining image margin when packing (per K4 gradient-test headroom
notes).

## 6. Proposed `scripts/mkfs-f2fs-populated.py` additions (description + snippet, NOT applied)

Description: add an M4_INPUT_SHIP file list analogous to however M3's GL ship
set is already expressed in that script (presumably a list of
`(src_path, image_dest_path)` tuples or a directory-copy call keyed by arch).
Source root is `~/code/leandros-artifacts/m4-input-ship/<arch>/`; every path
under it already mirrors the on-image path 1:1 (i.e. `<arch>/usr/lib/foo.so`
-> image `/usr/lib/foo.so`), so the addition should be a recursive copy of
that per-arch staging root rather than an enumerated file list, mirroring
however the M3 lane's own staged-root copy is expressed. Example shape
(adapt identifiers/helper names to match the script's existing style — this
lane did not read mkfs-f2fs-populated.py's internals, being repo-read-only,
so treat this as illustrative, not copy-paste-exact):

```python
# M4 (anvil) input-stack ship set — libudev(flipped)/libseat/libinput/
# libevdev/libmtdev/libxkbcommon/libpixman/libdisplay-info + XKB data +
# libinput quirks DB. Staged 1:1 per arch; see
# ~/code/leandros-artifacts/notes/m4-ship-manifest.md for provenance.
M4_INPUT_SHIP_ROOT = Path.home() / "code/leandros-artifacts/m4-input-ship"

def add_m4_input_ship(image_root: Path, arch: str) -> None:
    src_root = M4_INPUT_SHIP_ROOT / arch
    for src in src_root.rglob("*"):
        rel = src.relative_to(src_root)
        dst = image_root / rel
        if src.is_symlink():
            dst.parent.mkdir(parents=True, exist_ok=True)
            os.symlink(os.readlink(src), dst)
        elif src.is_file():
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)
            # preserve +x on the .so real files (cp2 keeps mode; sonames'
            # symlinks handled above)
```

Call `add_m4_input_ship(image_root, arch)` for the given arch alongside
wherever the M3 GL ship set is currently folded in, before/after order
shouldn't matter (disjoint path sets — the only potential overlap is
`libudev.so.1*`, which m3's sysroot ALSO has a stock copy of; if M3's packing
step stages sysroot's `usr/lib` wholesale, **M4's copy must run after M3's**
so the flipped libudev wins).

## Anvil binary itself

`anvil-{x86_64,aarch64}` binaries at `m3-gl-stack/out/` were read (for `readelf`
NEEDED extraction and closure verification) but **not copied** into
`m4-input-ship/` — this lane stages anvil's *dependencies*, not anvil itself;
the mission scope is the input/XKB ship set.
