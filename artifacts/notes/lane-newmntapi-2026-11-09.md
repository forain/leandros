# Lane newmntapi (M8 phase 1) - 2026-11-09 - FIXED

Syscalls 428-433 and 442 (open_tree, move_mount, fsopen, fsconfig, fsmount, fspick, mount_setattr) now have an explicit quiet `-ENOSYS` arm in `kernel/src/syscall.rs`; constants added to both the x86_64 and aarch64 `nr` modules (identical numbers).

Finding: main already returned -38 for these through the unknown-number fallthrough (`log_enosys`), so the new guard does not fail on main. The only difference is that main printed `[SYSCALL] ENOSYS` on the serial console for every probe. Nothing else claimed those numbers; seccomp (`sched/src/seccomp.rs`) filters on the raw number before dispatch, so it sees the right values.

Guard: `userland/mnttest/mnttest.c` (7 cases, registered in build-userland.sh, mkfs-f2fs-populated.py and the default regress list). aarch64: 7/7 PASS.

Default regress aarch64: all green except host:f2fsnidtest.py, which passed on an isolated rerun (flake). mnttest was only staged after the first run (RC=127 then); rerun alone: RC=0.

crun audit (upstream main linux.c): see "M8 verification" in design-mntns.md. Default create/run paths all fall back to mount(2) on ENOSYS. Hard failures only for a bind whose source is inside the rootfs, `dest-nofollow` symlink sources, and `libcrun_make_runtime_mounts`.

Open: x86_64 not run (lane protocol: aarch64 per lane).
