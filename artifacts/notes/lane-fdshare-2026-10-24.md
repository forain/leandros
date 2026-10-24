# lane/fdshare — openbugs L5.6 "background child loses its fd after `sh -c 'X &'` exits" — REFUTED (2026-10-24)

## Verdict
REFUTED. No fd is removed from a background child's table. The udevhot4 "lost reader" (pid 155, `evtest2 cap
/dev/input/event2 4 0`, result `(0,0)`) ran its full 4 s capture window and closed its fd itself; the key 'a' was
injected ~15 s later, after the second reader (pid 162) had finally been launched. Harness timing, not the kernel.

## Evidence
1. Minimal repro, new `exectest bgfd` (4 cases, now part of plain `/bin/exectest`, already in regress): launcher forks
   (or `clone(CLONE_VFORK|SIGCHLD)`, or `/bin/sh -c 'exectest --bgfd P > so 2>&1 &'`, or `sh -c 'true; ... >> so &'`)
   and exits; the background child waits until it is reparented, then writes to its own fd and to its redirected stdout
   every 50 ms for 3 s, checks every write + F_GETFD + final `lseek(SEEK_END) == 60`. aarch64 HVF: 3 boots x 4 cases =
   12/12 PASS (tests-fdshare1). Not red on main -> it is a regression guard, not a fix guard.
2. Census in /tmp/leandros-udevhot4-aarch64-serial-full.log (EVSTAT every 0xC8 = 2 s, t in 10 ms):
   - dev2 reads attributed to rpid 0x9b (=155): 0x176->0x187->0x19b->0x19e = 17+20+3 = 40 reads = `secs*10` = exactly
     the 40 poll iterations of a 4 s `cap`. A lost fd would make poll return POLLNVAL at once and the read count stop
     short; it did not. Client gone between t=0x1518 and 0x15e0 = normal end of life (~4 s after register).
   - dev2 `push=0x10` constant for the whole life of 155; the second reader's `sh -c` appears after t=0x1b58 and the
     injected key lands at ~t=0x1c20 (push 0x10->0x14), ~11 s after 155 exited. 162 then got open id 6 = legit reuse.
3. Code check: fork/vfork-style clone copy the table (`copy_fd_table`, refs incremented incl. dynamic-device open ids);
   exit closes only the exiting pid's / private key's table (`vfs_close_all_for`, `handle_close_all(pid)`); no shared
   (CLONE_FILES) table exists for a non-thread child, so there is nothing for exec to unshare.

## For lane/udevhot (cap_events)
`hold = secs + 3` starts per reader at launch, but readers are launched by sequential `sh()` round trips (here ~15 s
apart). Launch all readers in ONE `sh -c 'evtest2 cap A .. > /tmp/ev0.out 2>&1 & evtest2 cap B .. > /tmp/ev1.out 2>&1 &'`,
or make hold cover the launch latency (e.g. cap waits for /tmp/ev.go before its window). Open aside: why one `sh()`
round trip took ~15 s there (console/LRC sync) is not investigated.
