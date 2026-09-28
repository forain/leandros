# ports/cosmic-comp: closed-window texture leak fix

This is the one approved exception to the no-COSMIC-patch rule. The user approved it on
2026-09-27, for this leak only.

## The leak
Each closed window left one GL texture alive in cosmic-comp: the import of the window's last
buffer. On every driver it showed up as one extra GEM handle in comp's DRM open. On virgl it
also showed up as one out-fence eventfd, the texture's `read_sync` GLsync. The Ctrl-T `[FDK]`
and `[DRMH]` census lines measure both (see `artifacts/notes/lane-compfdleak-2026-09-27.md`).

The cause is in cosmic-comp's `src/wayland/protocols/toplevel_info.rs`:
- Every `ZcosmicToplevelHandleV1` stored a **strong** clone of its window.
- `remove_toplevel()` never cleared that clone.
- The handle lives until its client destroys it, so the closed window stayed reachable:
  handle → Window → WlSurface → data_map → texture.

## The patch
`0001-toplevel-info-weak-window.patch` is a port of MartinKavik's `weak_window` fix to
cosmic-comp `dec1ee86`:
- The handle stores `Window::Weak` and upgrades it where it is used.
- `toplevel_management` requests on a handle whose window is gone are ignored. Before, they
  would panic on `unwrap()`.
- smithay stays unpatched at `efeb597`, which already has `WeakWindow`. cosmic-comp already
  has `WeakCosmicSurface`.

Upstream references:
- Smithay#1562, "closing windows causes VRAM leak": https://github.com/Smithay/smithay/issues/1562
  - cmeissl's diagnosis: https://github.com/Smithay/smithay/issues/1562#issuecomment-3864200389
- pop-os/cosmic-comp#2084: https://github.com/pop-os/cosmic-comp/pull/2084
- MartinKavik's fix: https://github.com/MartinKavik/cosmic-comp/tree/weak_window_upstream_smithay
  (write-up: https://github.com/MartinKavik/popos_fix_vram_leak)

**Remove this port when upstream merges the fix.** At that point, bump cosmic-comp instead.

## Build
```sh
ports/cosmic-comp/build.sh aarch64 x86_64
```
- It applies the patches to `leandros-artifacts/m6-session-bins/src/cosmic-comp`. Each patch
  is applied once, so it is safe to rerun.
- It builds with the m6-session-bins toolchain (`--no-default-features`), as the shipped
  binary was built.
- It stages the result at `leandros-artifacts/m6-session-bins/out/cosmic-comp-<arch>`.
- `scripts/mkfs-f2fs-populated.py` prefers that path over `m3-gl-stack/out/cosmic-comp-<arch>`,
  and `LEANDROS_COSMIC_COMP=<file>` overrides both.
- The mkfs log prints which binary it packed.
