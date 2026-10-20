# lane/imgslim: smaller lane worktrees

- scripts/sparseimg.py: sparse image writer (truncate, write non-zero 4 KiB runs) + `inject` (sparse dd conv=notrunc). Used by mkfs-f2fs-populated.py, mkfs-f2fs-minimal.py, and build-all.sh's limine image (truncate -s 512M + inject). mkfs-f2fs-home.py was already sparse.
- Root sizing unchanged (max(32, segs*2)); the free half is now holes.
- .claude/skills/run-leandros/imgclone.py: cp -c / reflink / cp --sparse=always instead of shutil.copyfile in the per-run image copies.
- ports/portal and ports/busd cargo targets moved to ${LEANDROS_PORTS_CACHE:-~/Library/Caches/leandros-ports}/{portal,busd}-target, shared by all worktrees. Note: portal's CC_* wrapper paths are worktree-specific, so a first build in a new worktree may rebuild C-dependent crates; still no per-worktree copy.
- Evidence: fresh old-script image vs new-script image: cmp IDENTICAL; fsck-f2fs PASS; aarch64 regress PASS.
