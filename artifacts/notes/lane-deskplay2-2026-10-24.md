# lane/deskplay2 - 2026-10-24 - laptop YouTube playback census (measurement only, no code changes)

Kernel: slot good, kid a0ad7cd53e99, cmdline with iwlwifi.mgr=1 (untouched, no deploy). Scripted login (session already up),
`scripts/deskplay/phase1.sh`: 3 pinned launches, snapshots +60 s then 2x30 s windows; 11.5-min SSH heartbeat. Panel lit
(`blanktest --status` On) in all playback windows. Video plays in all three: flips 52.7-52.8/s, exec_vcs 60.0/s, wifi RX 249-266 KB/s.

| item | verdict | numbers |
|---|---|---|
| L3.1 TASKS | STILL-BAD | r1/r2/r3: acq 31.1k/30.6k/30.4k per s, contended 4413/4348/4121 per s, held 95 ms/s, spin 31.7/30.9/28.5 ms/s |
| L3.6 total CPU | OK (about half of old 310 %) | 153/150/148 % (sys ~75). firefox parent 61, Web Content 37-39, cosmic-comp 20, RDD 15, netd 4, busd 0.6 (old ~30), pipewire-pulse 2.2 |
| L3.7 RDD | PARTIAL | RDD 15 % total, 9.2 % sys (old sys 5.7 %). Per s: nr7 (poll) 2600, nr3 (close) 1570 (about 26 per frame), nr228 1460, nr32 (dup) 1330, nr202 680, nr16 (ioctl) 483 (2.8 % sys). Close/dup churn persists at ~26/frame |
| L6.2 SSH blackout | OK | 686 heartbeat lines over 690 s of playback: max gap 3 s; ping 686/687 (0.1 % loss, max RTT 1.48 s) |
| L11.3 blank spin | OK | panel Off (blanktest) ~230 s, stayed Off; 30 s window: Web Content 21.9 % (12.3 sys), firefox 14 %, RDD 8.2 %, total 56 %, flips 0, exec_vcs 30/s. No 3-core spin. Panel restored On afterwards |

Notes: first blank attempt (single 200 s sleep) showed status On at measurement time (output re-enabled); the retry polled status every 20 s and it stayed Off.
Raw snapshots: scratchpad deskplay/out (r1-r3, c). Tooling unchanged.
