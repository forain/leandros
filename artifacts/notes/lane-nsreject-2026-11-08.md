# lane/nsreject (L0) 2026-11-08 - FIXED (clone/clone3 part)

- `do_clone` (kernel/src/syscall.rs) now returns EINVAL for any CLONE_NEW{NS,CGROUP,UTS,IPC,USER,PID,NET}; shared by clone and clone3, same set unshare already rejects. Closes the "silently unconfined" hole.
- New guest suite `userland/nstest` (`/bin/nstest`, runtests alias `ns`, in default regress): clone/clone3/unshare x 7 flags -> EINVAL, plus a plain clone sanity case. LeandrOS-interim: stock Linux would create the namespace for root; set NSTEST_HOST=1 to skip on a host. Later lanes move flags out as they implement them.
- Before fix: 14 FAIL (all clone_*/clone3_*), unshare 7 PASS. After: 22 passed, 0 failed.
- Default aarch64 regress (HVF): overall PASS.
- NOT done (per instruction): prctl untouched (seccomp lanes own it).
