# lane/futexvma (L10.11)

Non-private futex ops key through `futex_backing`, whose `AddressSpace::find` scanned every `regions` slot (twice for phys-keyed pages).
Fix: `AddressSpace::find` tries the last-hit slot (`find_hint`, Relaxed atomic) first, validated against the live region.
Never trusted, nothing to invalidate. Page faults and `virt_to_phys` get it too. relibc untouched.

Tests: futextest `wake_cost` (1001 VMAs, 100k uncontended wakes, shared vs FUTEX_PRIVATE, fails above 3x) and `hint_remap_shared`.
Measured on the desktop under TCG aarch64 (syscall cost dominates, so the effect is modest):
- before: shared=26945ns private=15003ns (1.80x)
- after:  shared=16021ns private=14399ns (1.11x)
Both cases PASS before and after (the 3x bound only catches a return to an O(N) scan; TCG did not show a 3x gap before).
