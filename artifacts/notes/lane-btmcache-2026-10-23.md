# lane/btmcache 2026-10-23 - FIXED

Cause: build-all.sh passed `-C linker=$ROOT_DIR/scripts/linker-<arch>-musl.sh` in RUSTFLAGS (bottom, coreutils,
brush, disks-rs) and `CC_<triple>=$ROOT_DIR/scripts/cc-*.sh` / `AR_<triple>` env. $ROOT_DIR is the worktree, so
RUSTFLAGS differ per worktree -> different crate hashes in the shared sibling target dirs -> no reuse, stale piles
(du today: coreutils/target 177G, brush/target 130G, bottom-leandros/target 93G at peak, 7.7G after cleanup).
Fix: wrappers are copied (if changed) to a fixed dir `<parent>/.leandros-tools/` and referenced from there.
Evidence: bottom aarch64 built from worktree (48 s), then from the main repo path: 0.27 s, 0 new files in deps.
Left open: existing hashes in coreutils/brush targets are stale; delete those target dirs (or cargo-sweep) when no lane builds.
