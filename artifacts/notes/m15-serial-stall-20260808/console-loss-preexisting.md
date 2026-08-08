# Separate, pre-existing: x86_64 console output has no flow control at all

Found while checking that the `putc` deadline had not made console output
lossy. It had not. Something else already had.

## The measurement

Boot, log in, and print 300 numbered lines through a **continuously draining**
serial reader (`scripts/scmrun.py`, 240 s window, explicit end marker):

```
i=0; while [ $i -lt 300 ]; do echo CONSOLELINE$i; i=$((i+1)); done; echo CONSOLEDONE
```

| kernel | md5 | lines received | first missing |
|---|---|---|---|
| with the `putc` deadline (control) | `aaf1d14090a30ccb80bd32df3bd54327` | **19 / 300** | 19 |
| with the deadline removed (mutant) | `4b96738d695bd988a3ef1d45886a5720` | **19 / 300** | 19 |

**Byte-for-byte identical on both**, including the missing set: 0–18 arrive,
19–299 do not, and the trailing marker does. The `putc` change is not involved.

## Why

`drivers/src/serial.rs::write_byte` — the path userspace console writes take —
does not check `LSR.THRE` on x86_64. It is an unconditional `out dx, al` into
the transmit holding register:

```rust
pub fn write_byte(&self, b: u8) {
    #[cfg(target_arch = "x86_64")]
    { unsafe { core::arch::asm!("out dx, al", in("dx") self.base, in("al") b, ...); } }
    #[cfg(not(target_arch = "x86_64"))]
    unsafe { arch_serial_putc(b); }
}
```

The 16550 is initialised with its FIFO enabled, so the first ~16 bytes land and
everything written before the transmitter drains overwrites what is still in
flight. 19 lines is about what a burst into a 16-byte FIFO with no handshake
survives. Note the `#[cfg(not(x86_64))]` arm *does* go through
`arch_serial_putc`, which waits — so **this is x86_64-only**, and aarch64's
console does not have it.

## Why it matters beyond the console

This is very likely why the x86_64 side of `artifacts/m13_suite.py` has been
unreliable on this box, and it predates this lane: the recorded run of
2026-08-07 (`artifacts/notes/m13-cosmic-config/m13-control-suite-x86_64.log`)
already shows `drmsmoke  NO EXIT STATUS READ BACK` with 22 of 29 PASS lines,
and `m13-suite-x86_64-*.log` shows 7 of 12 tests with no `M13RC=` read back at
all. A run of the same suite today reproduces exactly that shape — vfstest's 36
subtests arriving 16 in its own window and 20 in the next, every later row then
reading the previous row's exit status. **Widening the read budgets does not
help** (700 s per test reproduces it identically), because nothing is timing
out: the bytes are never sent.

`waittest` reporting `wait_on_process_group: FAIL` (`failures = 1`) is the
documented pre-existing flake, not part of this.

## The fix, deliberately not made here

`write_byte` should wait for `LSR.THRE` the way `arch::putc` now does —
`arch::putc` is exactly the right primitive and, since it now carries a
cycle-counter deadline, calling it can no longer wedge anything. That is a
one-line change with a large blast radius (every console byte gains an LSR
read, i.e. a VM exit under emulation) and it needs its own suite run to land
honestly. It does not belong in an input-path commit.
