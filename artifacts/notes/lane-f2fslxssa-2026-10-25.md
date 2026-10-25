# Lane f2fslxssa: Linux curseg summaries + rw for Linux-made volumes (f2fslxrw S4), 2026-10-25

Status: FIXED.
- linux_import copies the six curseg summaries (compact or normal format, node sums on umount packs) from the cp pack
  into the SSA area (device writes, rw only). NATIVE_IMPORT_SAFE = true: Linux-made volumes mount rw (S0 policy still
  forces ro for the unsupported cases; inline mutation keeps EIO; inode_checksum remains ro until f2fslxcsum merges).
- Root cause of the remaining fsck failures: nat_set bumped the NAT version byte to 1 on every rewrite; Linux summaries
  carry version 0, so every rewritten Linux node/dentry block showed "Invalid data seg summary" and cascaded counter/
  unreachable-NID errors. On lx volumes the version byte is now left untouched.
- `ino 3 i_links 5, real 4`: not reproducible after the fix (Linux mkdir ldir, then our rm -rf ldir/mkdir/rmdir: fsck rc=0);
  it was a side effect of the invalid-summary cascade.
Evidence: lxnoi and lxdef (pristine Linux images): our rw workload (12 MB file, 10 files, rm, mkdir/rmdir) -> box fsck rc=0,
Linux mounts, md5 of our files OK, Linux writes, fsck rc=0; second round trip (our rm -rf of Linux's ldir, mkdir) rc=0.
interop.sh all: known-failure entries removed (all UNEXPECTED-PASS before removal). aarch64 regress PASS, f2fscptest run.
