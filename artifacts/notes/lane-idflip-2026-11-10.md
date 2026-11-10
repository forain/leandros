# lane idflip (2026-11-10)

Flip: non-identity uid/gid maps are on by default. Branch lane/idflip, commit c4d9ac8a on main 8985de69. NOT merged (two security reviews pending).

Kernel diff (sched/src/ns.rs, kernel/src/main.rs, ~10 lines): `IDMAP_REAL = true`; new cmdline opt-out `idmap=off`
(`set_idmap_off()`, `idmap_real() = IDMAP_REAL && !IDMAP_OFF`); `idmap=test` is now an ignored token. The only consumer of
`idmap_real()` is write_map rule 9 (`upper != lower_kuid`); no permission logic touched. kuid-0 refusal (rule 6) unchanged.
Opt-out is for one release, then delete the knob.

Tests: idmaptest probe refused without `idmap=off` is now a FAIL (`probe_nonidentity_accepted`, review S8); newuidmap_e2e/e2e_subuid
run by default (skip only on idmap=off); nstest `map_firefox_seq` split into `ff_seq_identity` + `ff_seq_nsroot` (S9; nsroot skips on
idmap=off); runtests `podbridge` no longer forces `idmap=test`; comment fixes in mnttest/podbridgetest.

Results (no knob): ffsandbox --mode on (Mac aarch64): RESULT {'on': True}, all 8 children Seccomp 2 (only parents 740/835 are 0), no Sandbox errors.
Rootless `podman run --rm --network=none alpine:3.21 echo hi` as leandro (Mac aarch64, default cmdline): `hi`, RC=0.
Regress (no knob): x86_64 box (KVM) 81 suites 0 bad, overall PASS. aarch64: box disk was full (ENOSPC from other lanes, twice), so the full default regress ran on the Mac under lock.sh qemu: overall PASS, idmaptest 0 fails, nstest 0 fails (ff_seq_identity + ff_seq_nsroot green).

Not done (review B leftovers, not flip-blocking per B): delete `kid_from_view` (N1), S10 grep gate reword (sched/src/lib.rs:3059,3273 apply to already-translated kuids).
