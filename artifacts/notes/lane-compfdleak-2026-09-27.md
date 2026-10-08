# Lane compfdleak — 2026-09-27

Symptom: on virgl, cosmic-comp keeps about 1 eventfd per closed client (multiterm: 43→50 after 8 terminals). Venus showed no eventfd growth.

Branch `lane/compfdleak` (pushed), head `cacc00ba`: diagnostics only.

## Verdict: REFUTED as a kernel bug. cosmic-comp/smithay keeps one GL texture per closed window.

- **What the leaked eventfds are.** All of them are our VIRTGPU_EXECBUFFER out-fence eventfds, and every one is signalled (c=1). Each is the `read_sync` GLsync of a smithay `GlesTexture`. It is created in `GlesFrame::render_texture_from_to`, then goes through Mesa's virgl flush with FENCE_FD_OUT. The eventfd2 fds are flat (20).
- **What the texture is.** It is the closed window's last-buffer import: `GlesRenderer::import_dmabuf` ← `WaylandSurfaceRenderElement` ← `CosmicWindow::push_render_elements`. This comes from the PRIME_FD_TO_HANDLE backtraces, symbolised at PIE base 0x200000.
  - While a terminal is open, comp holds 2–3 imports of its buffers. After close, exactly one of them stays live and the rest are GEM_CLOSEd.
- **Venus behaves the same.** On laptop x86_64/KVM, comp's DRM open grows by one import per closed window: 34→37→40→41 handles at N=0/1/4/8. While a terminal is open it has 41f7/41f8/41f9; after close, 41f9 is kept.
  - Venus looked clean only because Zink's GL syncs don't mint an fd.
- **The disconnect is delivered.** After each close, these return to baseline:
  - comp's sockets: 21
  - the client's dmabuf fds in comp: `[FDK] dmabuf=` stays flat
  - smithay's Dmabuf, which is dropped (so smithay's destruction reset ran)
- **Nothing releases the texture.** I tried idle 30/90 s, the overview (Super+W), a workspace switch, the launcher, mouse motion, and opening and closing another window. Opening the overview adds its own +2.
- **Linux comparison.** Upstream Smithay#1562, "closing windows causes VRAM leak", has been open on Linux since 2024-10. The candidate fix is Smithay PR #1921 (draft, head 2f5337b6), and cosmic-comp#2084 is related. Nothing in our kernel semantics is missing.
- **GEM handles on our side.** Comp's handle is its own PRIME import. It correctly outlives the dead exporter, as on Linux. It is released only by comp's GEM_CLOSE, which never comes because the texture is alive.

## Numbers (comp fence eventfds / comp DRM GEM handles; pool = global eventfd pool fill)

| run | N=0 | 1 | 4 | 8 | 16 |
|---|---|---|---|---|---|
| aarch64/HVF virgl, fence ev | 21 | 22 | 25 | 29 | 37 |
| aarch64/HVF virgl, GEM handles | 28 | 29 | 32 | 36 | 44 |
| aarch64/HVF virgl, pool | 235 | 236 | 240 | 243 | 250 |
| x86_64/KVM virgl (laptop), fence ev | 15 | 17 | 21 | 25 | — |
| x86_64/KVM virgl (laptop), GEM handles | 23 | 23 | 26 | 30 | — |
| x86_64/KVM Venus (laptop), fence ev | 1 | 1 | 1 | 1 | — |
| x86_64/KVM Venus (laptop), GEM handles | 34 | 37 | 40 | 41 | — |

- The x86_64 virgl diag8 run is 16→33 fences at N=16.
- On x86_64 Venus the pool stayed flat at 192–193.
- The growth is linear with no bound, and nothing releases it later.

## Committed (cacc00ba): Ctrl-T census
- `[FDK] pid= ev= ev_fence= tmp= dmabuf= dev= pipe= other=` for tables with ≥20 eventfds. This needed eventfd provenance in `EVENTFD_META`: origin, tgid, serial and fence id.
- `[DRMH] blob_objs= openN=handles` for live GEM handles per DRM open.
- `FD_PROVENANCE_BT` (kernel const, default off) records user frame-pointer backtraces for eventfd2, the out-fence and PRIME imports, and prints `[EVFD]`/`[PRIME]` lines (vfs `EVFD_DUMP_DETAIL` for the per-fd lines).
- Files: kernel/src/syscall.rs, servers/vfs/src/lib.rs, drivers/src/drm_device_interface.rs.

## Suites (release, --no-gpu): polltest, epolltest, wakepolltest, drmsmoke, sigtest
- aarch64/HVF: 5/5 RC=0.
- x86_64/KVM (laptop): 5/5 RC=0.

## Not done: the COSMIC-side fix
- The orchestrator relayed that the user approved an exception: apply Smithay PR #1921 to our cosmic-comp build.
- I adapted the PR to efeb597 without changing the public API, so cosmic-comp would compile unmodified. I then started the rebuild, and the permission system denied continuing it. An agent-relayed approval is not the user's own consent, so I stopped the build.
- The work is parked, uncommitted, at `/private/tmp/claude-501/-Users-forain-code-leandros/1eba02a4-a12c-41fb-a6b6-03086905d613/scratchpad/cfl/cosmic-patch-parked/`. It contains `cosmic-comp/{README.md,build.sh,smithay-pr1921-dead-surface-hooks.patch}` and an mkfs override diff.
- Build trees are at `leandros-artifacts/m6-session-bins/src/{smithay-pr1921,cosmic-comp}`.
- It is not verified that #1921 fixes this particular retention.

## Open
- The user should confirm the COSMIC patch directly if they want it. Then run `build.sh aarch64 x86_64` and verify with `[FDK] ev_fence` and `[DRMH]` over 16 cycles.
- Shared files touched: kernel/src/syscall.rs (ioctl/eventfd2 signatures, epoll dump), servers/vfs/src/lib.rs (dump only), drivers drm (census fns).
