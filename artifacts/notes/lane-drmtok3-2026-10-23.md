# lane/drmtok3 2026-10-23 (L1.14 remainder: opaque mmap offsets, token 0, hostvis test): FIXED (hostvis guard SKIPs on this Mac)

Commits: 3fc1edbd (tests), 6f15aeb8 (fix).

## (1) Opaque offsets
New `drivers/src/drm_offset.rs` (Linux `drm_vma_offset_manager` style): per-device map real token -> page-aligned
fake span in [1<<44, 1<<45), plus a 32-bit window [0xE000_0000, 0xF000_0000) for the legacy 0x1002 CREATE_FB reply
(DOOM reads it as u32). Hand-out sites: MAP_DUMB, VIRTGPU_MAP (dumb/guest blob/HOST3D window), V3D_MMAP_BO,
i915 MMAP_OFFSET, nouveau GEM_NEW/GEM_INFO map_handle, CREATE_FB. `handle_ioctl_mmap` (0x1007) resolves fake ->
real token (keeping the in-span offset, so partial hostvis maps work) BEFORE the per-open check from lane/drmtoken;
a non-fake non-zero offset from a user open is refused (open_id 0 kernel callers still pass real tokens).
Spans are released in `free_dumb`, `blob_unref`, i915 and NVIDIA BO free; allocation is next-fit with a guard page,
so a freed offset is not reused at once (drmsmoke MAPLIFE_SURVIVES_GEM_CLOSE asserts no token reuse — first-fit
reuse turned it red, next-fit fixed it). A missed release is a bounded leak, not a hole: the real token still has to
pass the ownership check, and a same-token object of another size replaces the stale span.

## (2) Token 0 (firmware framebuffer)
Consumers: none in-tree besides drmsmoke (DOOM uses CREATE_FB's offset, /dev/fb0 is a separate device). Kept, but
now `master_gate(open_id)` on card0 (implicit grant to the first open when no master exists, as Linux's open-time
grant) and refused (EACCES) on renderD128 in servers/drm.

## (3) Tests (userland/drmsmoke)
`--fakeoff` (FAKEOFF_DUMB_OPAQUE/MAPS/STABLE/DISTINCT/STALE_REFUSED), `--fbtoken` (FBTOKEN_RENDER_REFUSED,
FBTOKEN_NONMASTER_REFUSED, FBTOKEN_MASTER_MAPS), `--hostvis` (Mesa-virgl-style HOST3D blob: PIPE_RESOURCE_CREATE
cmd + CREATE_BLOB, MAP opaque, write/munmap/remap readback, partial map at +4 KiB, stale offset refused after
GEM_CLOSE). `--hostvis` SKIPs under `--virgl`: runtests' virgl device has no blob/hostmem
(RESOURCE_BLOB/HOST_VISIBLE getparams are 0); it runs under `--venus` (blob=on,hostmem=4G).

## Red on main (aeaf5193 kernel + new drmsmoke, aarch64 HVF)
- `--fakeoff` failed=2 (DUMB_OPAQUE, DISTINCT) on GPU none and --virgl.
- `--fbtoken` failed=2 (RENDER_REFUSED, NONMASTER_REFUSED): a fresh non-master card0 open and a render-node open both
  mapped the firmware framebuffer.

## Green (fixed, aarch64 release, HVF)
- GPU none: --fakeoff, --fbtoken, --tokenscope, --maplife, --scatter RC=0.
- --virgl: --fakeoff, --fbtoken, --tokenscope, --maplife, plain, --scatter, --force-scatter, gpuprobe gl RC=0
  (re-run after merging main 7689d859+: same, except one plain-drmsmoke run failed
  V3D_SUBMIT_CL_FENCE_NOT_RETIRED_AT_SUBMIT, which passed in the two pre-merge runs; timing case, not offset code).
- Full aarch64 regress (fixed kernel, pre-merge): overall PASS, 0 boots failed.
- `cargo check -p kernel --features nvidia` x86_64: clean.

## Open
- i915/NVIDIA paths type-checked only (`cargo check x86_64 --features nvidia`), not run.
- `--hostvis` never ran a real case: `--virgl` has no blob/hostmem (SKIP), and runtests `--venus` on aarch64 failed to
  boot ("Image type X64 can't be loaded on AARCH64 UEFI system" — the venus path picks the x86 image). Needs a venus
  run on x86_64/KVM, or blob=on,hostmem= added to the virgl device.
- Plain `drmsmoke` with GPU none dies RC=139 after FLIP_TS_SUBTICK: "[PAN] raw kernel access to user memory",
  FAR=user stack, in the next step (GPU_IRQ_STATS 0x1008 / park-spin area). Not shown pre-existing (not run on main);
  the offset code does not touch that ioctl. Repro: `runtests.py aarch64 t "/bin/drmsmoke"` (no --virgl).
- V3D_SUBMIT_CL_FENCE_NOT_RETIRED_AT_SUBMIT failed once under --virgl after the merge (see above).
