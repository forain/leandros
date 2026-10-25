# lane memfast64 2026-10-25 (follow-up of lane memfast / openbugs #71)

## Verdict: FIXED — aarch64 kernel mem* are no longer byte loops

`kernel/src/mem_a64.rs` (wired from `kernel/src/mem.rs`) gives aarch64
memcpy/memset/memmove an 8-byte bulk with byte head/tail:

- destination aligned first; when src and dst agree mod 8, 4x-unrolled
  `ldr`/`str x`; when they disagree, the source is read as aligned
  doublewords and merged with `lsr`/`lsl`/`orr` (2x unrolled). The extra
  bytes read are in the same aligned doubleword as wanted bytes (never another
  page).
- every access naturally aligned (framebuffer/Device/Normal-NC safe); only
  single-register offset-addressed `ldr`/`str` — no `ldp`/`stp`, no writeback
  (those give ISV=0, which HVF cannot emulate if an access ever traps); no
  FP/SIMD registers (kernel is softfloat, EL0 vector state is not saved).
- memmove: forward when `dst - src >= n` (wrapping), else top-down: tail bytes
  until the destination end is aligned, doublewords downward, head bytes.
- head/tail bytes via volatile accesses so LLVM cannot re-idiomise the loops
  into recursive memcpy/memset calls.
- memcmp unchanged (byte loop).

MMIO audit: no kernel memcpy targets trapped MMIO; register access is
volatile. The framebuffer is written by memcpy in `drivers/src/framebuffer.rs`
(`copy_px_wide` on aarch64 = `copy_nonoverlapping` shadow -> base, `clear` via
`write_bytes`); shadow and base have the same offset mod 8, so the fast
same-alignment path is taken and all its accesses are aligned.

## Evidence

Host test `kernel/memtest` now also runs `mem_a64.rs` natively on the Mac:
`cargo test --manifest-path kernel/memtest/Cargo.toml --target aarch64-apple-darwin`
→ 4/4 (lengths 0..=80, all 64 src/dst misalignments, memmove overlap at every
distance both directions, 4 KiB/64 KiB). Guard shown failing (large_copies,
memmove_overlap) on an injected bug in the backward shift-merge.

New opt-in boot benchmark `ktest=memperf` (not in `ktest=all`; set with
`scripts/set-image-cmdline.sh leandros-limine-aarch64.img ktest=memperf`),
RAM (HHDM) MB/s, byte loop (main ac339cd9) -> this lane:

| case               | HVF old | HVF new | TCG old | TCG new |
|--------------------|--------:|--------:|--------:|--------:|
| memcpy 4K aligned  |   3795  |  57186  |    614  |   7269  |
| memcpy 1M aligned  |   3838  |  49254  |    613  |   7612  |
| memcpy 1M src+3    |   3859  |  31338  |    602  |   3667  |
| memset 1M          |   3893  |  61394  |    832  |  10571  |
| memmove 1M up+8    |   1295  |  49106  |    408  |   7438  |
| memmove 1M down+5  |   3951  |  31155  |    628  |   3663  |

Mismatched alignment: shift-merge is 6-8x the byte loop, so the "fall back to
bytes" option was not kept.

## Tests
- aarch64 regress (HVF): overall PASS, 0 boots failed.
- aarch64 `--suite net-e2e`: overall PASS.
- coldstart (CS_STEPS=term, aarch64 HVF): launcher 0.91 s, window 1.05 s
  (integ16 0.92/1.06) — not regressed.
- x86_64 release build compiles (not booted; x86 path unchanged).

## Open
- memcmp is still a byte loop on both arches.
- x86_64 untouched (only built to confirm it compiles).
