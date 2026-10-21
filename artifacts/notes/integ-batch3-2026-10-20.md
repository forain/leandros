# integ batch 3 (2026-10-20)

Branch integ/batch3 from main 69c9b863 (vfsabi, pools, x86tlb merged since integ-2). Release build of both arches OK.

| arch | regress | net-e2e |
|------|---------|---------|
| aarch64 | PASS after 1 flake re-run | PASS |
| x86_64 | PASS | PASS |

## Flakes
- aarch64 regress: host:nettag2test.py failed first run (fails=2: tcp_recv, nettag2test) under parallel host load (x86 regress running). Re-run alone under the qemu lock: RC=0, fails=0. Wake-count flake, not a regression.

## Real failures
None.
