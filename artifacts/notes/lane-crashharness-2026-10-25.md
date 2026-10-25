# lane/crashharness 2026-10-25: DELIVERED

Cherry-picked (-x) two f2fscrashtest.py harness fixes from lane/f2fscrc; no kernel or f2fs changes.
- f66683cf: serial capture started after the workload was typed, losing early W lines (gens wrongly "never written there").
- 71f0ff99: setup-incomplete crashes count as errors.

Evidence (aarch64, main's f2fs): seed 1792903575 PASS; seed 462812096 PASS.
