# lane mntiso-strict (E3, D1) 2026-11-11 - FIXED (core)
- In a mntns task, once a call's path was resolved in ns space (CALLCTX present), filesystems never follow a symlink: tmpfs `tmp_resolve_links` and f2fs `resolve_path_ex` fail with VFS_ELINKRACE (-4100) when they meet a link they would follow. `handle_as` redoes resolution at most twice, then ELOOP; the code never reaches userspace. Marker = per-task `strict_links` (sched), set by `rewrite_one`, restored by `handle_as`. ns 0 untouched.
- Host-side/kernel-internal reads (no CALLCTX: module autoload etc.) are NOT strict, so nothing relying on host lookups broke (this replaces the HostLookup guard of the design; inverse polarity).
- Test: `/proc/mntselftest` row `strict_tmp_link_refused` (mnttest strict_ns_suite). Red on lane/mntiso-strict-red (strict neutered): FAIL; green: PASS.
- Evidence: box aarch64 full regress 84/0 bad, x86_64 full 84/0 bad; podbridge + podpause pass; ffsandbox on (Mac aarch64) PASS.
- podpastatest: pasta_portfwd FAILS identically on unmodified main (box aarch64, wget timeout) - pre-existing, not this lane.
- Not done: race_dir_link_flip (needs a pivoted root; row is the deterministic gate); unix_bind_node/unix_resolve_node and f2fs-side sockets are not strict (entry lane resolves them); f2fs path untested by a row (covered by regress); caller_jail_* left (unreachable when strict).
