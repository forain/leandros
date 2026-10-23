# lane/pollnarrow (L12.15) - REFUTED as a kernel regression; test made robust

pollnarrow ppoll_ctl_churn compared probes/call with an epoll_ctl-churning thread against an idle baseline
(fail if churn > base*1.1+0.5). 5 runs on main (aarch64, QEMU lock): baseline 10.47-11.55, churn 11.95-11.97.
Churn saturates at ~12 (2 passes over nfd=6) - and so does a control thread that only spins on getppid()
(11.78-11.88, no epoll_ctl at all). So the figure measures a second runnable thread competing for a CPU, not
epoll_ctl cost; the failure appears whenever the baseline sample is <= ~10.9 (seen 9.85).
Fix: add a spin control phase, compare churn against max(baseline, control). 3 runs: base 10.11-11.28,
control 11.78-11.88, churn 11.62-11.92, all PASS. Full aarch64 regress PASS.
Not bisected against older mains (no evidence of a kernel change; an old 7.3 sample came from a different, slower build/host load).
