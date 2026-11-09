# lane/fusedev 2026-11-09 -- FUSE in the kernel VFS (PARTIAL: core delivered)

New crate servers/fuse (fuse-server): /dev/fuse (char 10:229, notify-open device), mount(2) fstype "fuse" / "fuse.<sub>"
with data "fd=N,rootmode=,user_id=,group_id=[,allow_other]", umount2 (DESTROY, EBUSY with open files unless MNT_FORCE/DETACH).
Protocol 7.31 INIT; LOOKUP/FORGET/GETATTR/SETATTR/OPEN/READ/WRITE/RELEASE/OPENDIR/READDIR/RELEASEDIR/MKDIR/CREATE/
UNLINK/RMDIR/RENAME(2)/LINK/SYMLINK/READLINK/STATFS/FSYNC/FLUSH/xattr/ACCESS/FALLOCATE/INTERRUPT(on signal)/DESTROY.
Callers park until the daemon replies; last close of the device fd aborts the connection (ENOTCONN); poll on /dev/fuse.
The VFS is path based: each op walks with LOOKUP and FORGETs afterwards; open files hold one lookup until RELEASE.
Without allow_other only the mounting uid may access (EACCES), as Linux.

VFS fix: a mount whose mount point is inside a tmpfs root (/tmp/x) was ignored by open/getdents (is_tmp_path won);
now paths under such a mount belong to the mount (servers/vfs is_tmp_path + TMP_NESTED_MOUNTS).

Test: userland/fusetest (raw-protocol daemon, hello fs, mounted on /tmp/fusemnt; ls/cat/stat/write/create/unlink/ENOTCONN/umount),
registered in the regress suite and the image bin list. Before the VFS fix it failed 4 cases (ls/cat/write/kill) -- the
daemon never saw OPEN/READDIR; after: 0 FAIL on aarch64 (HVF).

Open / not done: mknod, mmap of FUSE files, READDIRPLUS, BATCH_FORGET, FUSE_DEV_IOC_CLONE, default_permissions mode checks,
absolute symlink targets (ENOENT), pread/pwrite offsets rely on the VFS lseek path, MAX_MOUNTS still 8, fusermount3 and
libfuse not tried (fuse3 not in the image), x86_64 not run by this lane (box).
