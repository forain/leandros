# lane mntnosuid (2026-11-10) - REFUTED (no gap on main; guard test added)

Review item 5d claimed `exec_setid` ignores nosuid for mounts in a non-initial mntns. It does not:
`exec_setid` calls `backing_walk(kp.ns_bytes(), &mut crossed)`; in a mntns != 0 that goes through `backing_walk_m` -> `mnt_xlate`,
which does `*crossed |= x.flags & MNT_NOSUID` from `mnt::Mnt.flags`, and `exec_setid` returns (None, None) when `crossed`.
`path_on_nosuid_mount` is only called from `exec_setid` (kernel/src/syscall.rs); file capabilities are not implemented at exec (5b).
- Test `mntnosuid_bind_ignores_setuid` (mnttest): root in a private mntns (no userns), setuid-root copy on a tmpfs, two binds, one remounted nosuid;
  uid 1000 exec: suid bind euid 0 (control), nosuid bind euid stays 1000, sibling still suid. PASSES on unmodified main (box aarch64 mnttest 1/1).
- No kernel change. Host (ns 0) nosuid path untouched.
