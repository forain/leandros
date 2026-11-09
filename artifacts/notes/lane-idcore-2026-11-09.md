# lane idcore (design-idmap lane 1) - 2026-11-09

Status: DELIVERED (aarch64). Maps are stored composed and every write rule of design-idmap section 2 is in; main stays identity-only (`IDMAP_REAL=false`).

## What changed
- sched/src/ns.rs: map storage is `(upper_first, lower_kuid, count)`; `to_k`, `from_k`, `from_k_munged`, `root_k`, `IdView`, `view_of`, `view_of_ns`, `map_text_for` (opener-relative read: reader in the map's own ns sees the parent's names, unmapped = 4294967295). `view_id` body is `from_k_munged(kind, current_userns(), kid)`; `ids_mapped`/`mapped` stay as wrappers. `write_map` implements rules 1-9; `new_userns` does rule 8 (EPERM if creator euid/egid unmapped) and rule 7 (setgroups=deny inherited). `gid_map_written()` backs the setgroups(2) check.
- `IDMAP_REAL = false` plus cmdline `idmap=test` (kernel/src/main.rs -> `ns::set_idmap_test`); the knob never lifts the kuid-0 refusal (rule 6).
- sched/src/caps.rs: `capable_snap` (Linux cap_capable walk on a cred snapshot) and `ns_capable_in`.
- servers/vfs: map reads translate for the opener (`gen_proc_self(reader, target, path)`).
- kernel/src/syscall.rs `sys_setgroups`: EPERM in ns != 0 until its gid_map is written (rule 7).
- userland/idmaptest/idmaptest.c (14 cases) wired in build-userland.sh, mkfs-f2fs-populated.py, runtests.py (default list + suite `idmap`).
- nstest: `root_einval` skips when the kuid-0 map is refused; `depth_limit`/`leak` now map uid/gid 1000 at each nesting level (rule 8 makes unmapped nested unshare EPERM).

## Evidence (aarch64, Mac)
- No knob: idmaptest 9 passed, 0 failed, 5 skipped (unpriv_translated_lower, priv_multi_extent, nested_view, podman_map_text, unpriv_root_map); nstest 75 passed, 0 failed.
- `--cmdline idmap=test`: idmaptest 14 passed, 0 failed, 0 skipped.
- Default regress (merged main b7c9ca7b base): overall PASS, 0 boots failed. ffsandbox.py aarch64: exit 0.

## Open / for later lanes
- `ids_mapped` still exists (idxlate removes it); set*id/chown still take raw ids, so keep the knob out of production until idxlate/idcaps/idperm land.
- setgroups rule 7 is partly in syscall.rs (`sys_setgroups`); idxlate also edits that area.
- kuid-0 maps stay refused (idroot0).
