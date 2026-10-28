# lane/greetdcache 2026-10-28 - FIXED

greetd is staged by pam-leandros source hash: greetd-lane/<arch>/<srchash>/{greetd,fakegreet,greetd.srchash}.
- ports/greetd/build.sh: stages into the hash dir, skips cargo when cached (FORCE=1 rebuilds, ARCHES=aarch64 limits), still refreshes <arch>/greetd as a latest-build copy for old tooling.
- scripts/mkfs-f2fs-populated.py: _greetd_stage_dir() uses the hash dir of this checkout, never the flat copy; runs build.sh for the arch when missing (~20 s warm, GREETD_NO_AUTOBUILD=1 disables). _check_greetd_fresh (AUTH_ERR guard) unchanged.
- Race proof: scratch copy with edited pam-leandros built into the cache (flat greetd.srchash became 0ff09297..); worktree image still used cacde361.. with no stale error. aarch64 build + regress PASS.
