# Guard falsification — `m15_serial_stall`

The guard is only worth what it fails on, so it was run against a kernel built
with **only** its guard removed. The mutation deletes the deadline and the
`TX_WEDGED` latch from `arch/x86_64/src/lib.rs::putc` and nothing else — the
counter, the const and `rdtsc_raw` all stay referenced so the mutant differs in
behaviour, not in surface.

x86_64/KVM, Linux box, QEMU 11.0.1, three phases per run, one boot per kernel.

| kernel | md5 (`target/final-x86_64/kernel`) | PARKED | DRAINED | ABSENT | verdict |
|---|---|---|---|---|---|
| control | `aaf1d14090a30ccb80bd32df3bd54327` | **100.0%** | 100.0% | 100.0% | `failures = 0` |
| mutant  | `4b96738d695bd988a3ef1d45886a5720` | **9.8%**   | 100.0% | 100.0% | `failures = 1` |
| restored| `aaf1d14090a30ccb80bd32df3bd54327` | — | — | — | byte-identical to control |

The restored kernel is byte-identical to the control, so the mutant differs from
the control in the guard and in nothing else.

`DRAINED` and `ABSENT` are 100% on **both** kernels. That is the point of
carrying them: they show the mutant is not simply a broken kernel, and that what
the guard discriminates is specifically a back-pressured console, not input
delivery in general.

The positive control (`nosuchbinary_xyz42` as the first command of every boot)
reported `command not found` on every run, control and mutant alike.
