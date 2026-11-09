# lane/ovlmknod 2026-11-09 (O0)

Status: FIXED. Device nodes (mknod of char/block nodes, rdev preserved) work on f2fs and tmpfs.

## Commits
- b4873b4d WIP ovlmknod: device nodes in kernel/f2fs/tmpfs, ovltest
- 936f7811 keep mode leaves nodes in place for external fsck
- 5a92973b merge of main (lane ovlreal) into the lane

## Test placement
Lane ovlreal also created userland/ovltest/ovltest.c. Both lanes wanted that path, so this lane's
test lives in userland/ovlmknodtest/ovlmknodtest.c (binary /bin/ovlmknodtest, group "ovlmknod"),
registered in build-userland.sh, mkfs-f2fs-populated.py and runtests.py. ovlreal's ovltest is untouched.
(The coordinator asked for both in one ovltest.c; a separate binary was the only conflict-free way.)

## Evidence
- Fail-first: reverse-applying kernel+servers made the tests fail (O1, O10, O11, O13-O16 ...) on both fs; re-applied.
- Before the rename: 58 PASS / 0 FAIL on aarch64 (f2fs /home + tmpfs).
- Box regress lane/ovlmknod 5a92973b after the main merge: aarch64 74 suites, 0 bad (939s); x86_64 74 suites, 0 bad (686s).
- Box fsck.f2fs on the x86_64 guest image after the run: all [Ok].

## Not done: Linux stat of kept device nodes
Skipped (over the 6-call budget). `box/regress.sh ... /bin/ovlmknodtest keep` treats extra args as separate
runtests commands (RC=127 for "keep"); quoting it as one command still gave "2 suites, 1 bad", cause not
investigated. The image fsck.f2fs after the normal run is clean. Open: stat the kept nodes on Linux
(sudo loop mount works on the box, passwordless) once runtests can pass the argument through.
