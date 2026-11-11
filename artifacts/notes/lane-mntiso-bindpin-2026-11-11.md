# lane mntiso-bindpin (E5, D5) 2026-11-11 - FIXED
- `Mnt.pin=(dev,ino)` set after a bind (`vfs::bindpin_set`, host-backed roots only); global `mnt::NAME_GEN` bumped at the top of handle_rename/unlink/rmdir/symlink. `ns_resolve_links` final step calls `bindpin_verify`: if `vgen < NAME_GEN`, one stat of the backing root; mismatch/failure = ESTALE (fail closed, deviation from Linux dentry pinning), match = vgen updated.
- Tests (mnttest): bindpin_dir_swap (replacement dir must be ESTALE, not NEW), bindpin_link_swap (host sentinel never read; ESTALE/ELOOP), bindpin_unrelated_ok (no false ESTALE). Red (verify neutered): dir_swap + link_swap FAIL; green: PASS.
- Cost (aarch64 HVF, `mnttest bindbench`): quiet stat through a bind 6.5 us (no extra stat); rename+stat via bind 15.3 us vs 14.9 us without a bind = about +0.4 us (~3%) per op, exactly one stat per op after a name change (60002 checks / 60000 renames).
- Evidence: see merge message; podbridge + podpause pass; ffsandbox on PASS; podpasta pasta_portfwd known (pastafix).
- Not done: rbind children are pinned only via the top mount of the bind (children keep their source's pin); only the final mount of a path is verified.
