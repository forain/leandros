# lane memfast 2026-10-25 (openbugs #71)

## Verdict: FIXED (premise REFUTED) — x86_64 rep-string mem* landed

The "e1000e never gets a DHCP lease with rep movsq/stosq mem*" regression was a
test-harness artifact, not a kernel bug. `linkmontest.py` chose its arch from argv
only (`"x86_64" in argv else "aarch64"`); runtests passes host tests the arch via
`LEANDROS_TEST_ARCH`, so in an x86_64 `--suite net-e2e` run it booted **aarch64**.
In a worktree without a current aarch64 image QEMU exits at once
(`Could not open .../leandros-limine-aarch64.img`), the serial log stays 0 bytes, and
the test reports `setup: FAIL no e1000e lease: ''` (the empty string is the tell:
`/proc/net/nic` was never read). inloadx86's 3/3 failures have that exact signature
(empty nic, 135.6 s), and so did this lane's first suite run.

Evidence that the mem* change itself is fine on e1000e (x86_64/TCG):
- f6a9a6a3 re-applied on main 3cafd0d2, `LEANDROS_NIC=e1000e LEANDROS_NET=user`:
  `/proc/net/nic` `dhcp: bound`, 10.0.2.15/24 after 45 s.
- standalone `linkmontest.py x86_64`: PASS (tx_bulk 413.7 Mbit/s).
- after the harness fix, x86_64 `--suite net-e2e`: overall PASS, 12/12, linkmontest
  PASS (432 Mbit/s), inloadtest serial PASS 56/56.
- Code audit: e1000e descriptors use volatile field accesses, packet buffers are RAM,
  MMIO is volatile u32; every interrupt/exception entry stub executes `cld`
  (the kernel already contains ~1450 compiler-inlined `rep movs/stos`).

## Commits (lane/memfast)
- d814b762 x86_64 mem*: bodies in kernel/src/mem_x86.rs (rep movsq/stosq + byte tail,
  backward memmove with std/cld); host test kernel/memtest (all lengths 0..=80, all
  src/dst misalignments, memmove overlap at every distance both directions, return
  value, DF clear, 4 KiB/64 KiB copies). Run:
  `cargo test --manifest-path kernel/memtest/Cargo.toml --target x86_64-apple-darwin`
  (Rosetta; needs `rustup target add x86_64-apple-darwin`). Guard shown failing on an
  injected off-by-one tail.
- 530fe7e0 linkmontest honours LEANDROS_TEST_ARCH and prints driver start/login output
  on setup failure.

## Tests
- x86_64 net-e2e PASS (above); aarch64 regress PASS (overall PASS, 0 boots failed).
- lines/s not re-measured: inloadtest prints no rate; inloadx86 measured 130 lines/s
  with this exact mem.rs variant.

## Open
- aarch64 mem* are still byte loops. Not done: an ldp/stp version must not issue
  unaligned accesses to Device-nGnRE mappings (framebuffer/MMIO fault), so it needs an
  alignment-aware head/tail — a lane of its own.
- runtests suites made only of host tests can hit `Failed to get "write" lock` in the
  first host test (suite VM not yet gone when the host test starts its own); seen once
  with `runtests x86_64 tag host:inloadtest.py host:linkmontest.py`.
