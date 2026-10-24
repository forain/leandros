# lane/drmtoken 2026-10-23 (L1.14, mmap token scope): FIXED

Commits: b5939b83 (test), 3576811f (fix).

## Red
`drmsmoke --tokenscope` on the unfixed kernel (default GPU, aarch64): a forked child that opens renderD128 afresh
and mmaps parent A's dumb-BO token got A's secret (`TOKENSCOPE_FOREIGN_OPEN_REFUSED` FAIL). The PRIME-import
case (`TOKENSCOPE_PRIME_IMPORT_MAPS`) passed.

## Fix
`drivers/src/drm_device_interface.rs` `handle_ioctl_mmap(arg, open_id)`: dumb/virgl/scattered records require
`owner == open_id` (aliases from `prime_import_dumb` carry the importer as owner); blob/hostvis tokens need a
`BLOB_BUFFERS` handle owned by `open_id`; `open_id == 0` (kernel legacy) stays unscoped. The 0x1007 dispatch no
longer takes the DRM device lock. Backend `mmap` receives `open_id`; i915 and NVIDIA check
`STATE.opens[open_id].handles -> bos[obj].phys == token` (None -> EINVAL).
i915/NVIDIA are type-checked only (`cargo check x86_64 --features nvidia`), not run.

## Evidence (aarch64, HVF)
- `--virgl`: drmsmoke --tokenscope, --maplife, plain, --scatter, --force-scatter, gpuprobe gl: all RC=0.
- default GPU: drmsmoke --tokenscope RC=0 (was red).
- COSMIC greeter login under --virgl (probe.py) reaches a session: /run/user/1000 has wayland-1, wayland-1-renderD128, CosmicSettings bus, pipewire.
- Full aarch64 regress: overall PASS, 0 boots failed.

## Open
- Tokens are still raw physical addresses / window offsets (info leak, no longer an access hole).
- Token 0 (firmware framebuffer) maps for any opener of the node (legacy DOOM/fbdev path).
- No test covers the blob/hostvis path.
