# lane/idpermf2fs (design-idmap row 8, H3)

f2fs (servers/f2fs/src/lib.rs, the /root and /home volume) now uses the idpermx rules: `Cred::cap_inode(cap, uid, gid)` = host root, or the cap held in the caller's non-host userns AND the inode's uid and gid mapped there. Never `euid == 0` in any ns.

## Converted sites
| site | cap | ns / over | reason |
|---|---|---|---|
| `kill_setid` (write/truncate setid strip) | CAP_FSETID | cap_inode on the inode (host root short-circuits) | Linux should_remove_suid |
| `may_access_ino` root shortcut | none (skip xattr read) | host_root only | ns root takes the normal `may_access` path, which applies CAP_DAC_OVERRIDE/READ_SEARCH via cap_inode |
| `may_delete_in` sticky dir | CAP_FOWNER | cap_inode on victim (`sticky_denies_c`) | Linux check_sticky |
| `create_gate`, open(O_CREAT) | n/a | caller userns `owner_mapped` | fsuidgid_has_mapping: EOVERFLOW |
| `handle_open` DAC bypass (`euid != 0`) | DAC_OVERRIDE / READ_SEARCH | `!host_root` then `access_check` (cap_inode inside) | generic_permission |
| `handle_link` protected_hardlinks | CAP_FOWNER | cap_inode on source (`hardlink_denied_c`) | Linux may_linkat |
| `chmod_inode` | CAP_FOWNER | cap_inode; S_ISGID strip uses CAP_FSETID via `chmod_filter_mode_c` | inode_owner_or_capable |
| `chown_inode` | CAP_CHOWN | cap_inode; owner may restate uid / move gid to own group | chown_ok/chgrp_ok |
| utimens explicit times | CAP_FOWNER | cap_inode | inode_owner_or_capable |
| `xattr_list` trusted.* visibility | CAP_SYS_ADMIN | host_root only | trusted.* is init-ns |
| xattr get/set/remove | via xattr crate (already cap_inode / host_root from idpermx) | | |
Not changed: f2fs mknod has no device privilege gate of its own (the VFS layer gates device nodes); no quota gate exists in f2fs apart from the kill_setid site above.

## Tests
- idmaptest: six f2fs variants (`*_f2fs`) on `/idpx-f2fs` (root volume, statfs checked f2fs): create_unmapped_eoverflow, nsroot_chown_mapped, nsroot_chmod_subuid_file, nsroot_write_in_subuid_0700_dir, nsroot_cannot_touch_unmapped, host_user_cannot_touch_subuid. Existing cases kept.
- nstest `map_firefox_seq` is knob-aware: with `idmap=test` the non-identity `0 1000 1` map is accepted and the rest of the sequence uses ns ids 0.
- Fail-first: main + new tests (branch lane/idpermf2fs-guard, box aarch64, idmap=test): 3 f2fs cases FAIL (create_unmapped_eoverflow_f2fs, nsroot_chown_mapped_f2fs, nsroot_chmod_subuid_file_f2fs). Lane: 0 fails.
- Box aarch64 `--cmdline idmap=test /bin/idmaptest /bin/nstest`: 2 suites, 0 bad.
- Full regress: see below.
