# Lane smithayfix — 2026-09-27

Task (user request): apply Smithay draft PR #1921 to our cosmic-comp build to fix the closed-window VRAM leak (lane compfdleak, Smithay#1562).

## Verdict: REFUTED — #1921 does not fix our leak. Nothing shipped, nothing pushed.

## What was done
- Worktree `~/code/leandros-smithayfix`, branch `lane/smithayfix` from `ff7f5cd8`. It held `ports/cosmic-comp/{smithay-pr1921-dead-surface-hooks.patch,build.sh,README.md}` and the mkfs override, which made `scripts/mkfs-f2fs-populated.py` prefer `m6-session-bins/out/cosmic-comp-<arch>`. None of it was committed.
  - The worktree and branch are now removed. The parked copy is still at `scratchpad/cfl/cosmic-patch-parked/` from the compfdleak session.
- The patched cosmic-comp built rc=0 for aarch64 and x86_64 (Mac, m6 toolchain).
  - Same cosmic-comp `dec1ee86` and Cargo.lock as the shipped m3-gl-stack binary. Only smithay differed: efeb597 + #1921.
  - The mkfs log confirmed the image used the m6 binary.
- Cleanup:
  - deleted `m6-session-bins/out/cosmic-comp-{aarch64,x86_64}`, so mkfs falls back to the shipped `m3-gl-stack/out`;
  - restored the `m6-session-bins/src/cosmic-comp` Cargo.toml and Cargo.lock;
  - left `src/smithay-pr1921` in place (harmless).

## Numbers — aarch64/HVF virgl (Mac, `--virgl`), 16 cosmic-term open/close cycles, `scratchpad/cfl/cycle.py`
Figures are comp fence eventfds / comp GEM handles (`[DRMH]` open3). compfdleak's unpatched baseline is in brackets.

| N | 0 | 1 | 4 | 8 | 16 | idle30 |
|---|---|---|---|---|---|---|
| fence ev | 21 [21] | 22 [22] | 25 [25] | 28 [29] | 36 [37] | 36 |
| GEM handles | 28 [28] | 29 [29] | 32 [32] | 35 [36] | 43 [44] | 43 |

- The growth is still ~1 texture per closed window: 15 per 16 cycles, against 16 per 16 before.
- One extra open+close after N=16 also left +1.

Not run:
- The laptop and desktop were unreachable (LAN and Tailscale), so there was no x86_64 KVM virgl or Venus run.
- The x86_64 TCG fallback, the 5-minute regression pass, drmsmoke and the session smoke were skipped, because there was nothing to ship.

## Who holds the texture (source reading + two kill experiments)
- **Smithay is not the holder.**
  - `on_commit_buffer_handler` registers a destruction hook that calls `RendererSurfaceState::reset()` (`textures.clear()`).
  - GlesRenderer's `dmabuf_cache` is pruned in `cleanup()` on every `unbind()`.
  - The client's dmabuf fds do go away (compfdleak: `[FDK] dmabuf=` stays flat).
  - So the texture outlives the surface only through another strong reference.
  - #1921 only stops hooks being added to already-dead surfaces, which is not this path.
- **Upstream diagnosis.** cmeissl on Smithay#1562, and MartinKavik in pop-os/cosmic-comp#2084 and github.com/MartinKavik/popos_fix_vram_leak, identify a reference cycle in cosmic-comp:
  - `src/wayland/protocols/toplevel_info.rs`: `GetCosmicToplevel` initialises every `ZcosmicToplevelHandleV1` with `ToplevelHandleStateInner::from_window(window)`, i.e. a **strong** `window: Some(W)`.
  - `remove_toplevel()` resets the window's `ToplevelState` and sends `closed`, but never clears the per-handle `window`.
  - The handle lives until the client destroys it, so the chain is handle → CosmicSurface/Window → WlSurface → SurfaceUserData → data_map → MultiTexture/GlesTexture (with its read_sync fence eventfd and its GEM import).
  - Our tree (dec1ee86) has exactly this code; line ~204 is `from_window`, line ~672 is `window_from_handle`.
- **Kill experiment 1** (`sfx/a64-owner.log`, 6 cycles):
  - killing cosmic-applet-minimize, then cosmic-workspaces, then cosmic-panel, released nothing;
  - fence ev stayed at 27 and GEM handles at 34 throughout;
  - so none of those clients' handles is the (sole) holder.
- **Kill experiment 2** (`sfx/a64-owner2.log`): kills the pop-launcher `cosmic_toplevel` plugin, cosmic-applet-tiling, pop-launcher, cosmic-launcher and cosmic-app-library. ABORTED before its first measurement: the handback was forced and QEMU was stopped. The holder client is still unidentified. The pop-launcher `cosmic_toplevel` plugin (always running) is the leading suspect; a rerun of `sfx/owner.py` with `KILLS=plugins/cosmic_toplevel=...` settles it.

## Recommendation (needs the user's approval: a second COSMIC patch)
- Port MartinKavik's `weak_window` fix:
  - smithay gets `Window::downgrade()` / `WeakWindow`;
  - cosmic-comp gets `WeakCosmicSurface`, and `ToplevelHandleStateInner.window: Option<W::Weak>` is upgraded on use.
- This is preferable to the `window = None` variant, because `toplevel_management.rs` does `window_from_handle(toplevel).unwrap()` (9 sites) and would panic on a stale handle unless those are also made fallible.
- The same report lists sibling leaks worth taking together:
  - overview image-capture `ImageCaptureSourceKind::Toplevel`: our tree already uses `toplevel.downgrade()` there;
  - dead `MinimizedWindow` entries;
  - per-window shader cache;
  - `pending_activations`.
- Verify with the same census (`[FDK] ev_fence`, `[DRMH]`) over 16 cycles on both arches.
- Drop #1921 unless it lands upstream.
