# lane/f2fsinterop (2026-10-25) - DELIVERED

`scripts/f2fslx/interop.sh <lxdef|lxnoi|lxext|home|all>` (python in `interop.py`, usage in its header).
Promoted from the f2fslxrw/f2fsfsck prototypes. Direction A (lxdef, lxnoi, lxext): box mkfs + populate +
MANIFEST + fsck0, gzip transfer, aarch64 guest mount, `md5sum -c`, write attempt, ro image bit-identical.
Direction B (home): mkfs-f2fs-home-lx -> LeandrOS workload -> box fsck -> Linux rw verify+write -> fsck ->
LeandrOS re-mount + verify. Locks: `F2FSINTEROP_LOCK=$SP/lock.sh` takes `box` and `qemu` slots internally.
`F2FSINTEROP_VERBOSE=1` dumps guest output. Exit code = FAIL + UNEXPECTED-PASS count.

Known gaps: `scripts/f2fslx/interop-known-failures.txt` (EXPECTED-FAIL). Lanes delete entries as they fix them.

Today: 15 PASS, 6 EXPECTED-FAIL (A.write x3: ro volume; B.fsck1/fsck2: SSA/summary errors; B.back-verify:
"linux_import gave up at short-pack" after Linux wrote), 0 FAIL.
Findings: a "rw" mount of a Linux volume succeeds but writes give EROFS; the default QEMU config already
has a `leandros-home` disk, so B mounts /dev/vdd, not LABEL=. Not wired into runtests (needs box + ssh).
Not covered yet: lx2g/lx64g, compression/quota/casefold RO set, second round trip, stat/xattr comparison.
