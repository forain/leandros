# Staging `vkrender` in `scripts/mkfs-f2fs-populated.py`

**Described only — NOT applied.** The repo is read-only for this agent.

## The change

`vkrender` links exactly what `vktest` links (`libc.so` + `-ldl`, and it
`dlopen`s the ICD by absolute path), so there is **no new DT_NEEDED closure**
and nothing else on the image has to change. The design's "2 lines" is exact.

File: `/Users/forain/code/leandros/scripts/mkfs-f2fs-populated.py`
Location: immediately after the existing `vktest` block at lines **610-612**,
inside the `# ── M2 Venus ship set ──` section, which already has
`venus_root` in scope.

Existing text (lines 610-612):

```python
    _vktest = f"{venus_root}/usr/bin/vktest"
    if os.path.exists(_vktest):
        bin_files.append(("vktest", _vktest, 0o100755))
```

Insert directly below it:

```python
    _vkrender = f"{venus_root}/usr/bin/vkrender"
    if os.path.exists(_vkrender):
        bin_files.append(("vkrender", _vkrender, 0o100755))
```

That is the whole change: three lines in the same shape as the block above
(two if the `os.path.exists` guard is folded, but keep the guard — every other
optional artifact in this script has one, and a missing binary must not break
the image build for people who have not run the container).

## Where the binary must be

The snippet reads from `venus_root`, i.e.
`~/code/leandros-artifacts/venus-lane/stage-{arch}/usr/bin/vkrender`.

The build script in this directory writes to `/out/stage-$ARCH/usr/bin/vkrender`,
so **mount `venus-lane` as `/out`** and the file lands exactly where the mkfs
script already looks — no second lane directory, no second `os.path.expanduser`
line, and the staging change stays at three lines.

If you would rather keep the M3 artifacts in their own lane
(`~/code/leandros-artifacts/m3-vkrender/`), the change grows to five lines
because it needs its own root:

```python
    _m3_root = os.path.expanduser(f"~/code/leandros-artifacts/m3-vkrender/stage-{arch}")
    _vkrender = f"{_m3_root}/usr/bin/vkrender"
    if os.path.exists(_vkrender):
        bin_files.append(("vkrender", _vkrender, 0o100755))
```

Prefer the first form. The ICD it drives is a venus-lane artifact; keeping the
test next to it is the same reasoning the file's own comment gives for
`venus_root` existing at all.

## What NOT to change

- **Do not** add `vkrender` to the `bins = [...]` list at line 252. That list
  is Rust binaries from `userland/target/<triple>/release`; `vkrender` is a
  C binary from the Alpine container, exactly like `vktest`.
- **Do not** touch `scripts/build-userland.sh:38` (`RELIBC_LINKED=(...)`) for
  the same reason.
- **Do not** add anything to `usr_lib_files` / `m4_share_files`:
  `libvulkan_virtio.so` and `virtio_icd.<arch>.json` are already staged by the
  lines directly above, and `vkrender` needs nothing else.

## Suite safety

`vkrender` with no arguments is headless and touches no DRM ioctl, so it is
safe to add to a regression wave. `--present` takes CRTC 1 and must never be
in a default suite run — see the README.
