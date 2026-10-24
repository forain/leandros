# lane/floatabi 2026-10-24 (L11.12): C float ABI of leandros-libc on x86_64 - FIXED (class)

Inventory: libleandros_libc.a is linked by exactly two C programs, DOOM (Makefile.leandros) and the
new floattest; everything else is Rust (rlib, same soft-float ABI on both sides: no mismatch), relibc
or musl. Exported f64 functions: only pow/exp/log/log10/tan/sqrt (math.rs). No strtod/atof/ldexp/...
exist. printf/snprintf had NO %f/%e/%g at all (printed "%f" literally) on both arches. The larger hidden
part of the class: every compiler-rt float builtin in the archive (__powidf2, __floattidf, __fixdfdi,
__adddf3, __eqdf2, __truncsfhf2 ... 37-42 symbols in DOOM/floattest) was soft-float on x86_64.

Fix:
- build-userland.sh builds a second libleandros_libc.a for x86_64-unknown-leandros.json (+sse,+sse2,
  hard-float, build-std); DOOM and floattest link that one. Kernel already saves user SSE state
  (relibc/musl programs use it). math.rs trampolines now apply only to the soft-float build
  (cfg not(target_feature="sse2")); the hard-float build exports plain libm functions.
- printf/fprintf/sprintf/snprintf thunks (both arches) save xmm0-7 / d0-d7 into argv[8..16); do_format
  reads doubles from there; %f %e %g %F %E %G with width/precision/-/0 flags via core::fmt.
- scripts/check-c-float-abi.py: checks EVERY float-ABI symbol (libm names + f variants, strtod/atof,
  compiler-rt float builtins) in a linked ELF; each must touch xmm itself or in a callee (depth 3,
  address-keyed: aliases), soft-float codegen never can. Runs on doom-x86_64 and floattest-x86_64.
- userland/floattest-c/floattest.c -> /bin/floattest (in regress): libm calls, powi, i128<->double,
  printf %f/%e/%g/width.

Evidence:
- Checker: floattest linked against main's soft-float archive FAIL 37/45; fixed: OK 131/131; DOOM OK 133.
- x86_64 guest, floattest on the soft-float archive: 13 failures (pow..sqrt, powidf2, floattidf, all
  printf cases; main's own userland archive there was a stale pre-kvmaudio build). Fixed: see regress.
