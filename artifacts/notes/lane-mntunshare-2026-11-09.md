# lane/mntunshare (M1, CLONE_NEWNS) - 2026-11-09

Spec: orch/designs/design-mntns.md, M1. Branch lane/mntunshare. NOT merged.

## Behaviour
- unshare(CLONE_NEWNS) / clone / clone3 with CLONE_NEWNS create a private mount namespace (copy of the caller's tree, locks per Linux semantics).
- NEWNS needs CAP_SYS_ADMIN, unless combined with CLONE_NEWUSER (then checked in the new userns). Otherwise EPERM.
- NEWNS with CLONE_FS, CLONE_THREAD or CLONE_VM -> EINVAL. unshare accepts NEWNS|CLONE_FS as Linux does.
- Inside a non-initial mntns mount()/umount2() are handled by sys_mount_ns/sys_umount_ns, permission = ns_capable_in(CAP_SYS_ADMIN, owner userns of the mntns).
- mount: bind and rbind (file and directory), tmpfs. Remount, move, propagation flags -> EINVAL (M2). Other fstypes -> EINVAL (M5).
- umount2: plain, MNT_FORCE, MNT_DETACH, UMOUNT_NOFOLLOW; MNT_EXPIRE -> EINVAL. Not a mountpoint / namespace root / locked -> EINVAL; busy (submounts) without DETACH -> EBUSY.
- bind of a tree with locked children (non-recursive) -> EINVAL; rbind clones the subtree and the top loses LOCK_MNT; unbindable -> EINVAL.
- exec, #! and PT_INTERP resolve through the namespace; relative paths and cwd survive unshare; getcwd reverse-translates.
- Dead mount namespaces are reaped when a new one is created (leak check via /proc/mntselftest ns_live/sb_count).
- Code: sched/src/caps.rs ns_capable_in, sched/src/ns.rs swap_mntns, kernel/src/syscall.rs, servers/vfs/src/mnt.rs (bind/umount/walk_idx/owner_of).

## Security-relevant decisions
1. Permission is evaluated against the owning userns of the target mntns (Linux cap_capable walk, owner rule), not the caller's current userns only.
2. Mounts created by a less-privileged userns inherit LOCK_MNT on copy when the owner changed, so an unprivileged user cannot umount or unbind the locked inherited mounts (tests umount_locked_einval, bind_locked_children_einval, rbind_child_locked).
3. INTERIM tmpfs: a tmpfs mount in a mntns is backed by a hidden pool directory /tmp/.leandros-sb/tmpfs/<sbid>. Parents are root 0711, the instance dir is 0700 owned by the creator. It is destroyed by the sb destroy hook (maps /.leandros-sb/tmpfs/N). M5 must replace it with real tmpfs superblocks.
   Review items: ns0 can see the directory NAME under /tmp/.leandros-sb (0711 permits traverse by name, not listing); the pool uses real disk/tmp space and counts against /tmp; an owner-0700 dir means root without DAC override cannot enter; /.leandros-sb prefix is hidden from ns0 lookups (sb_prefix_* tests).
4. Unsupported flags (remount, move, propagation, other fstypes) fail closed with EINVAL, not silently ignored.
5. Stale tests changed: mnttest newns_still_rejected removed (root NEWNS now works), mnttest child resets inherited fail count; nstest NEWNS dropped from the always-EINVAL tables (NEWCGROUP kept).

## Verification (aarch64, Mac HVF)
- Fail-first: kernel files from 58fd6c9d with the new tests -> mnttest fails=7 (newns_unpriv_unshare, newns_bind_invisible_out, newns_tmpfs_invisible_out, newns_child_clean, newns_perm, clone3_newns, newns_root_umount_ns_ino).
- Fixed tree: mnttest 0 fails, nstest 0, chroottest 0, ffsandbox RESULT on=True off=False.
- Full aarch64 regress on the Mac (HVF, runtests.py, all suites): overall PASS, 0 failures. Box regress not run (queued 30+ min; box boots currently fail with "login: exec failed" per BOX_STATUS). x86_64 NOT run.
