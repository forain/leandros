# lane/nettag2pend 2026-11-11: FIXED
connect_pending "no pipe parks on a bit of mask 0x2000 (320 probed)" in full regress, both arches (identical text).
Evidence: histogram on the failing run: union of the 320 probe bits = 0xffffffffffffdfff, zero unparked probes.
So every probe parked fine; the 320 consecutive pipe rings simply hashed to 63 of 64 bits, never bit 13.
Ring numbers come from a pool whose position depends on earlier suites in the boot, so the window is
state-dependent (not load). Not a kernel bug.
Fix: nettag2 alias_pipe window 320 -> 1500 candidates (soft RLIMIT_NOFILE raised to the hard limit); failure
now prints the alias union. The connect_pending assertion is unchanged.
Verified: box regress lane/nettag2pend aarch64 PASS (was FAIL with 320 on the same tree), x86_64 PASS.
