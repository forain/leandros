# lane idquota2 (2026-11-10): FIXED

Namespace quota is now charged up the userns tree (Linux inc_ucount): a new namespace counts against the creator kuid and the owner kuid of every ancestor userns, each checked against 32 user / 64 total. Slot counts are derived by scanning live slots, so release paths need no decrement. Code: sched/src/ns.rs (quota_chain_ok, quota_one, chain_has).

Test: nstest quota_hier (needs idmap=test; skips otherwise). Map `0 1000 1;1 100000 16`, 9 subuids.
- main + test only: FAIL, total 254 (table full), other user cannot unshare.
- fix: PASS (total <= 32, uid 2000 can unshare).
Full aarch64 box regress: 81 suites, 0 bad. ffsandbox --mode on (Mac aarch64): on=True, all children Seccomp 2.
Open: 8b NPROC and 8d inotify limits remain per-kuid (LOW).
