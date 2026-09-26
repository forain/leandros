/* LeandrOS musl libc.so exports __stack_chk_fail but NOT __stack_chk_guard.
 * Alpine's static libstdc++.a / libgcc.a (pulled in via -static-libstdc++
 * -static-libgcc) are built with -fstack-protector and reference
 * __stack_chk_guard, so the Alpine-built libgallium fails to load on LeandrOS
 * ("__stack_chk_guard: symbol not found"). Provide the guard locally with a
 * fixed nonzero canary. Overflow detection stays functional (prologue stores
 * this value, epilogue compares it); it is not RNG-seeded, which is acceptable
 * for this software-GL bring-up. Compiled itself with -fno-stack-protector so
 * it does not self-reference. */
unsigned long __stack_chk_guard = 0x2b7e151628aed2a6UL;
