# lane shmslow 2026-10-25 — wl_shm copy slow on the laptop (openbugs #58)

## Verdict: FIXED (root cause = libc byte-loop memcpy); frame-callback 28 ms REFUTED as a bug

### Root cause
DOOM (and every C program linked against `libleandros_libc.a`) copies its frame
with the LeandrOS libc `memcpy`, which was `for i in 0..n { dst[i] = src[i] }`:
one `movb`/`ldrb`+`strb` per byte (LLVM never vectorises or idiom-replaces a loop
inside a function named memcpy). 256000 bytes = 256000 store iterations; the
laptop's 430 us is that loop at the i5-8350U's clocks, HVF's 62 us the same loop on
an M-series core. Nothing memory-type related.

Hypotheses checked in order:
1. PAT/MTRR — REFUTED by code: anonymous/shm/memfd PTEs never set PCD/PWT/PAT
   (`arch/x86_64/src/paging.rs translate_flags`), PA0 = WB under Limine; WC/UC only
   for device BARs and DRM `MMAP_HINT_UNCACHED`. memcpybench also shows the
   memfd and a private anon mapping copy at the same speed.
2. first-touch/fault per copy — REFUTED: memcpybench touch_ns_pg ~20 ns (HVF), the
   timed loop copies into already-faulted pages; shm == anon throughput.
3. userspace memcpy byte loop — CONFIRMED (disassembly of target/doom/doom-*).
4. cb_ms ~28 — REFUTED as a compositor/i915 latency: identical 28 ms on QEMU HVF
   virgl and the laptop, and = 1000/35 ms: DOOM only dispatches Wayland events
   from `wl_present`, once per 35 Hz game frame, so the callback is timestamped
   up to one DOOM frame late. Measurement artefact of the client, not a bug.

### Fix
`userland/libc/src/mem.rs`: x86_64 `rep movsq`/`rep stosq` + byte tail; aarch64
64-byte ldp/stp loop in inline asm (the target is +strict-align, so Rust
`read_unaligned` words were split back into bytes); memmove backward path keeps a
volatile loop. New `/bin/memcpybench` (C, linked like DOOM) times the 256000-byte
copy into a MAP_SHARED memfd / private anon mapping and checks memcpy/memset/memmove
for every length 0..300 x 16x16 misalignments, overlap both ways.

### Numbers (memcpybench, 256000 B, 200 iters)
| machine | old libc shm_us | new libc shm_us | byte-loop ref |
|---|---|---|---|
| QEMU aarch64 HVF | 67 (byte ref) | 5 (45 GB/s) | 66 |
| QEMU x86_64 TCG | 319 (byte ref) | 104 (2.5 GB/s) | 319 |
| laptop i5-8350U (idle console, 3 runs) | 143 / 143 / 161 (1.6-1.8 GB/s) | 7 / 9 / 9 (28-32 GB/s) | 80-93 |

Laptop memset 143-161 us -> 5-6 us. (DOOM's in-session copy_us was ~430 us with the
desktop running; the bench ran at an idle console. The old-libc binary's printf
garbled its later fields; shm_us/shm_MBps are self-consistent.) DOOM is rebuilt
against the new libc (`memcpy` = rep movsq) but its in-session copy_us was not
re-measured on the laptop (no desktop session driven, budget).

Regress (default suite) PASS on both arches; memchk_err=0 both arches.

### Open
- relibc/musl programs are unaffected (own memcpy); cosmic-comp's upload path not measured.
