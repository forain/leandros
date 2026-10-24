# lane/vaargs 2026-10-24 (L11.14): printf-family stack varargs - FIXED

Thunks (userland/libc/src/stdio.rs, both arches) now also pass argv[16] = caller's stack-argument
pointer and argv[17] = number of saved variadic int registers (printf 5/7, fprintf+sprintf 4/6,
snprintf 3/5 on x86_64/aarch64). do_format reads ints from the saved registers then the stack, doubles
from xmm0-7/d0-7 then the same shared stack cursor (SysV overflow_arg_area / AAPCS64 __stack, 8-byte
slots, Linux-style AAPCS64, not Apple). Previously ints beyond the registers read uninitialised thunk
frame (garbage) and doubles past 8 returned 0.0.
Also fixed: %d/%u/%x/%X without l/ll/z read the full 64-bit register ("-1" printed 4294967295).

Tests: floattest va_8ints, va_5uints, va_10doubles, va_mixed (>6 ints, >8 floats), va_longlong_ptr.
Before fix x86_64 TCG: 5 of the new cases FAIL (+floattest). After: PASS. aarch64 HVF regress PASS;
check-c-float-abi OK (floattest 131, DOOM 133); DOOM builds on both arches.
Open: 16-byte-aligned varargs (long double, __int128) not handled; no %hh/%h modifiers.
