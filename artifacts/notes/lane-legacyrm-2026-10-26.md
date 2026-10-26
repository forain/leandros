# lane/legacyrm (openbugs #46, last part) -- PARTIAL (slice 1 of 2 delivered)

User decision 2026-10-26: lx (`LeandrOS lxnode2`) is the only f2fs format; the legacy LeandrOS-private layout goes away.

## Slice 1 (this lane): nothing writes legacy any more, nothing reads it
- f2fs server (`aa79f134`): a superblock whose init_version is neither `LeandrOS lxnode` nor `Linux version` is refused
  before anything else is read: log `[F2FS] unsupported legacy LeandrOS f2fs format (no lxnode marker): reformat the volume; not mounting`, mount fails, no write.
- Guard (`fe8a7b37`): f2fscptest `legacy` case (fresh volume, init_version blanked in both sb copies): refused + logged + alive + image sha unchanged.
  On the unfixed kernel: `legacy: refused FAIL`, `legacy: logged FAIL` (the old kernel mounted it as legacy). Fixed: 4/4 PASS.
- Scripts (`961e4bb1`, `ed695b31`): mkfs-f2fs-home.py IS the lx writer now (old mkfs-f2fs-home-lx.py renamed over the legacy
  writer; `--legacy`/LEANDROS_F2FS_LEGACY gone). `ensure` moves a legacy-format home image to `<img>.legacy` and creates a fresh one.
  mkfs-f2fs-populated.py: legacy tail + helpers + the unused full-size `image` bytearray removed (the lx output is unchanged: old vs new
  script on the same inputs differ in 32 bytes = the random sb UUID, the same 32 bytes two runs of the new script differ in).
  fsck-f2fs.py and f2fs-lx-ssa-rebuild.py refuse legacy volumes; F2FS_TEST_LEGACY removed from f2fscrash/f2fsfull tests.
- Lines: 17 files, +211 / -1039 (net -828).

## Evidence (aarch64 HVF unless noted)
- regress PASS except host:pollnarrowtest ppoll_ctl_churn (ran in parallel with f2fscptest); alone: PASS.
- f2fscptest PASS (one earlier run lost its first boot under parallel load; rerun PASS).
- `scripts/f2fslx/interop.sh all`: 124 PASS, 0 EXPECTED-FAIL, 0 FAIL (last recorded count in notes: 15 PASS + 6 EXPECTED-FAIL).
- blkmanytest PASS, stdevtest PASS. x86_64 build OK, scripts/check-rpi5.sh OK.

## Open (slice 2, handoff $SP/handoff/legacyrm.md)
- servers/f2fs/src/{lib.rs,durable.rs}: `Fmt { lx }` is now always true after the mount guard; ~60 `fmt.lx` branches and
  legacy-only helpers (legacy dir_add_entry path, legacy xattr arena, NAT version bump, LSB-first SIT bits) are dead code to remove.
- `durable::cp_is_legacy` is the pre-f2fscrc CHECKPOINT CRC format, not the node format: decide separately (keep for now).

## For the orchestrator / user: main checkout
- `/Users/forain/code/leandros/f2fs-data0-*.img` are rewritten as lx by the next `build-all.sh`.
- `f2fs-data1-{aarch64,x86_64}.img` (legacy) get moved to `f2fs-data1-<arch>.img.legacy` and recreated as lx on the next
  build or run-qemu (`mkfs-f2fs-home.py ensure`); delete the `.legacy` files afterwards. The `f2fs-data1-*.run-*.img` clones
  are legacy and will not mount any more: delete them (`rm /Users/forain/code/leandros/f2fs-data1-*.run-*.img`).
- KVM box: no persistent images; `/tmp/f2fsinterop-1185/f.img` there is legacy scratch, delete.
