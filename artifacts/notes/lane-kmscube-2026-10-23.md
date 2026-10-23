# lane/kmscube 2026-10-23 (openbugs L11.8) - PARTIAL: relibc suspicion REFUTED, Pi fault stays open

kmscube + Mesa are built against musl (ports/gl-stack/NOTES.md, ports/mesa/cross-musl-aarch64.ini;
INTERP=/lib/ld-musl-*.so.1). relibc getenv/strtol are not in that process, so they cannot cause
the Pi fault (EL0, FAR=0x10, x0=sp+7, zero further ioctls). That fault is inside Mesa/musl userspace
past the whole kernel interface and stays Pi-only (no Pi here; kmscube not run in QEMU).

## Incidental relibc deviations found by the new `strtest` (userland/strtest, in regress)
First run 67 PASS / 20 FAIL; now 0 FAIL on aarch64. relibc commits 9f3390ab, aa2472fd:
- secure_getenv: unexported unimplemented!() stub (strtest did not even link) -> exported.
- strtod: u128 pow overflow for |exp| >= 39 (1e-310/1e-400 -> inf, 1e400 -> 0); "infinity" and
  "nan(0x1)" consumed 3 chars; "." consumed; no ERANGE; endptr not reset. Rewritten (Rust parse for decimal).
- strtol/strtoul: "0x"/"0xg"/"  -0x" consumed nothing (glibc/musl consume the "0"); LONG_MIN set ERANGE.
- setenv/unsetenv: no EINVAL for "" or names containing '='.
Coverage: strtol/ul/ll/ull edges (NULL endptr, base 0/8/16 prefixes, ws/sign, overflow, bad base),
atoi/atol/atof, strtod/strtof (inf/nan/hex/denormal/under/overflow), getenv unset/empty/prefix,
setenv/putenv/unsetenv, secure_getenv, getenv pointer stability, environ mutated while iterating.
No-conversion errno EINVAL is accepted (musl does it).
