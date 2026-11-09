# lane/idpermx (design-idmap row 7, H3)

Filesystem permission gates now ask "does this task hold cap C over THIS inode" instead of `euid == 0`.
`xattr::Cred` gained `host_root` (euid 0 in the host ns, the old `euid == 0`), `caps` (the task's cap bitmask in its own userns) and `view` (IdView).
`Cred::cap_inode(cap, uid, gid)` = host_root, or (task in a non-host userns, holds `cap`, and the inode's uid and gid both map in that ns). `Cred::cap_any(cap)` = host_root or the cap held in a non-host userns.

## Converted sites (vfs tmpfs + xattr; f2fs still on the old gates, lane idperm-f2fs)
| site | cap | ns / over | reason |
|---|---|---|---|
| tmpfs chmod/fchmod owner check | CAP_FOWNER | cap_inode (inode ids mapped) | Linux `inode_owner_or_capable` |
| chmod S_ISGID strip (`chmod_filter_mode_c`) | CAP_FSETID | cap_inode | Linux `in_group_or_capable` -> `capable_wrt_inode_uidgid`; the setid-kill gate over the inode is the correct Linux rule |
| setid kill on write/truncate/chown (`setid_kill_bits_c`, `tmp_kill_setid`) | CAP_FSETID | cap_inode | Linux `should_remove_suid`/`setattr_should_drop_sgid` |
| chown/fchown owner/group change | CAP_CHOWN | cap_inode | Linux `chown_ok`/`chgrp_ok` via `capable_wrt_inode_uidgid` |
| utimensat explicit times | CAP_FOWNER | cap_inode | Linux `inode_owner_or_capable` |
| sticky dir delete/rename (`sticky_denies_c`) | CAP_FOWNER | cap_inode on the victim | Linux `check_sticky` |
| hardlink restrictions (`hardlink_denied_c`) | CAP_FOWNER | cap_inode on source | Linux `may_linkat` |
| DAC override / read-search in `tmp_may` | CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH | cap_inode | Linux `generic_permission` |
| xattr `user.*` / ACL set/remove owner checks | CAP_FOWNER | cap_inode | Linux `xattr_permission`, `posix_acl_xattr_set` |
| `trusted.*` get/set/list/remove | CAP_SYS_ADMIN | host only (host_root) | Linux requires CAP_SYS_ADMIN in init_user_ns |
| `security.capability` and other `security.*` set | CAP_SETFCAP / host | host only for set in a userns without a mapping to init ns | file caps would otherwise grant caps across namespaces (EPERM in a userns) |
| tmpfs mount-root chmod/chown (no inode ids) | host only | host_root | mount root has no namespaced owner |
| mkdir-with-search path walk shortcut | cap_any / host_root | host_root | skip DAC only for host root |
| sysclass `write_allowed` (/sys/class writes) | CAP_SYS_ADMIN | host_root | sysfs attributes are init-ns objects |
| create/mknod/symlink/mkdir/open(O_CREAT) | n/a | caller userns | Linux `fsuidgid_has_mapping`: fsuid or fsgid unmapped in the caller's ns -> EOVERFLOW |

## Test changes (existing behaviour that relied on over-permission)
Only idmaptest cases added by this lane (10 new) needed edits, because a kuid-0 task in an own ns whose map is `0 100000 10` has fsuid/fsgid kuid 0 UNMAPPED; Linux returns EOVERFLOW on create in that state:
- `chown_maps_down`: the parent now creates the file and chowns it to kuid 100000; the child opens it (no O_CREAT).
- `setfsuid_ret_view`: the child also does `setfsgid(6)` before creating.
No other test changed. nstest/captest/vfstest/chroottest are green unmodified (aarch64 Mac).

## Evidence
- Fail-first: on main + the new idmaptest (branch lane/idpermx-guard, box aarch64, idmap=test) 7 cases FAIL (trusted_xattr_host_only, fcap_xattr_eperm_in_ns, create_unmapped_eov, ...); on the lane 0 fails.
- Mac aarch64: idmaptest (knob) pass; idmaptest/nstest/captest/vfstest/chroottest pass without knob.
- fsbench statwalk /tmp + statlat /tmp, Mac aarch64 HVF, main vs lane x2: statwalk p50 3us both, p99 7/6us, passes ~308k vs ~307k; statlat p50 17us both, p99 34-39us. Neutral.
- Box full regress: x86_64 75 suites 0 bad; aarch64 75 suites, only host:nettag2test (known wake-count flake) failed, rerun alone green.
