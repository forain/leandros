# lane/playlap - 2026-10-24 - L3.1 confirmation of playcost4 keyed-wake index on the laptop (measurement only)

Kernel: trial f43a0abaa589, `uname -v` "#1 playlap", built from main b5cfb4ea (contains playcost4 16ddedbe). Cmdline of good
(iwlwifi.mgr=1) copied to trial. ffcache's trial was built from 809b2376 (no b5cfb4ea), so a fresh build was needed.
NOT promoted (see verdict). Good slot stays d7cb59dfcfaf.

Measurement caveat: `phase1.sh` measures 60 s after launch. In this session YouTube had not started streaming by then in
the first full pass (RX 1 KB/s, flips 1/s, exec_vcs 0 in all three runs - invalid) and in runs 1-2 of the repeat pass
(run 2 was ramping: 79-84 % CPU, exec_vcs 0). Run 3 of the repeat pass is the only fully playing window
(flips 52.7-52.8/s, exec_vcs 59.8-60.1/s, wifi RX 156-248 KB/s). A manual probe between the passes showed the stream
playing normally (exec_vcs about 60/s) 40 s after launch, so the late start is a YouTube/network start-up effect, not the kernel.

## Playing run (r3, two 30 s windows)

| metric | baseline (deskplay2/playcost2) | playlap r3 w1 / w2 |
|---|---|---|
| TASKS acq/s | ~28-31k | 27.7k / 27.9k |
| TASKS held ms/s | ~91-95 | 67.8 / 68.2 |
| TASKS spin ms/s | ~25-31 | 15.7 / 15.7 |
| TASKS contended/s | ~4.1-4.4k | 3.23k / 3.23k |
| RDD CPU total/sys % | 15 / 8.8-9.2 | 15.1 / 9.2 |
| total desktop CPU % | ~150 | 154 / 154 (sys 78/76) |
| firefox / Web Content / cosmic-comp % | 61 / 37-39 / 20 | 62-64 / 38-40 / 18.5 |

Held and spin down roughly 25 % and 37 % against deskplay2 (acq/s unchanged: the work is the same, each acquisition is
cheaper). Total CPU and RDD sys unchanged: the keyed-wake index did not move them.

## Cumulative counters at end of the repeat pass (r1 / r2 / r3)

| row | r1 | r2 | r3 |
|---|---|---|---|
| dlscan pass / slots | 798783 / 9380959 | 888359 / 14250982 | 1144251 / 19299363 (full 0) |
| w4scan calls / slots / fallback | 54032 / 127580 / 46 | 77927 / 160600 / 53 | 116652 / 211606 / 65 |
| kwake calls / slots / deref | 1712249 / 5402439 / 5039593 | 2203672 / 6904900 / 6541954 | 3091272 / 9756448 / 9393285 (full 0) |
| widen calls / slots | 322120 / 543611 | 414090 / 693503 | 543125 / 885578 (full 0) |

r3 playback delta (r2 end to r3 end, about 90 s incl. start): kwake 887k calls / 2.85M slots = 3.2 slots/call; widen 129k calls /
192k slots = 1.5 slots/call; no `full` fallbacks. dlscan is now the largest scan: ~17 slots/pass (19.3M/1.14M cumulative; 20 slots/pass
over r2->r3) - the next candidate.

## Top /proc/rqhold sites at end of r3 (cumulative: site, holds, ns_held, cont, ns_spun)

| site | holds | held ms | cont | spun ms |
|---|---|---|---|---|
| sched/src/futex.rs:379 | 1442882 | 1115 | 139754 | 585 |
| sched/src/futex.rs:595 | 1342569 | 3256 | 183513 | 480 |
| sched/src/lib.rs:4026 | 1184809 | 872 | 137098 | 646 |
| sched/src/lib.rs:4422 | 1144250 | 5588 | 500083 | 0 |
| sched/src/lib.rs:4044 | 942429 | 414 | 124307 | 543 |

(sorted by hold count; lib.rs:4422 is the dlscan site and has the largest held time and most contention.)

## Heartbeat

686 ssh lines over the playback pass, max gap 2.53 s, one gap above 2 s; ping 692 lines.

## Verdict

PARTIAL / not promoted. The playcost4 index is a real improvement in lock hold and spin (about -25 % / -37 %) with total CPU
unchanged at ~154 %. Promotion rule (0 heartbeat gaps > 2 s) was missed by a single 2.5 s gap, and only one of three runs
had the video playing at the 60 s snapshot, so it was left as trial. Open: phase1.sh should wait for exec_vcs to rise before the
first snapshot; next scan to index is lib.rs:4422 (dlscan, 20 slots/pass, 5.6 s held per 1.14M passes).
