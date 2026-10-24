# lane/laptop3 — 2026-10-24 (netdwake A/B on the laptop; L8.10 x2APIC)

Status: netdwake PROMOTED (kernel tag d148515a30, kid edb99041acea); L8.10 x2APIC FIXED/VERIFIED (no code needed).

## 1. netdwake trial (d148515a) vs good (c703e3a80dfb), same AP, same ~35 min window
Quiet-session sampler (guest background script, ssh disconnected), 50 MiB scp, 3 rounds each.

| metric | good c703e3a80dfb | trial edb99041acea |
|---|---|---|
| idle netd passes/s (2 windows x 2 samples) | 10, 11, 10, 12 | 5, 4 |
| scp pull Mbit/s | 8.12 8.56 8.98 10.19 (median 8.77) | 8.75 7.87 8.27 9.25 7.67 8.63 (median 8.45) |
| scp push Mbit/s | 25.4 25.7 22.9 18.1 | 23.9 21.9 23.8 23.9 |
| heartbeat 300 | 300/300, max gap 1.13 s, 0 > 2 s | 300/300, max 1.15 s, 0 > 2 s |
| idle CPU (cosmic-comp+greeter) | 4.1-4.2 % | 4.0-4.2 % |

Pull median trial vs good: -3.6 % (within the -5 % gate; link noise between samples is +-10 %).
Promoted. (Note: the first trial push rounds 2/3 were skipped by remote.py's identical-file check and
were discarded; the table uses fresh random data per round.)

## 2. L8.10 x2APIC on the real laptop
`apic.x2apic=1` already exists (lane/kexecx2apic, arch/x86_64/src/apic.rs; QEMU TCG/KVM verified there),
so no code change. Trial boot of the same kernel with `iwlwifi.mgr=1 apic.x2apic=1`:
- `[CPUS] x2APIC mode (apic.x2apic=1)`, `[SMP] 8 CPUs online (x2APIC) cpu:apic 0:0 1:2 2:4 3:6 4:1 5:3 6:5 7:7`
- Wi-Fi up, heartbeat 120/120, max gap 1.04 s, 0 > 2 s; idle CPU 2.7 % comsmic-comp + 1.2 % greeter (as xAPIC).
- No panic/stall lines in the klog. Rebooted back to good; knob not promoted (no reason to: xAPIC works).
