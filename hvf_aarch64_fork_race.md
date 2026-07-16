# AArch64 `fork()` child freezes under HVF acceleration (Apple Silicon)

**Status: RESOLVED 2026-07-15.** Root cause found and fixed the same session the
one-shot fault-capture diagnostic (§4) was added. Not a race, not a TLB/barrier
bug, not an HVF erratum: a **kernel SP alignment fault** (`SCTLR_EL1.SA`),
deterministic on real hardware and silently forgiven by QEMU TCG.

---

## 1. Root cause

`fork_current` and `clone_thread` (`sched/src/clone.rs`) placed the child's
`UserFrame` — which becomes the child's initial kernel SP — at
`stack_top - UserFrame::SIZE`, where `UserFrame::SIZE` = **280** on AArch64
(35 × u64). 280 is not a multiple of 16, so the child's very first SP-relative
instruction, `str x0, [sp, #0]` in `ret_to_user_fork`, executed with
SP ≡ 8 (mod 16).

With `SCTLR_EL1.SA = 1` (set on this kernel, as on Linux), AArch64 takes an
**SP alignment fault** (ESR EC = 0x26) on *any* load/store that uses a
misaligned SP as base register — regardless of the access's own alignment.

The freeze signature followed mechanically:

- The fault vectors to the EL1-SPx sync handler, whose prologue is
  `sub sp, sp, #288` + `stp` saves. 288 ≡ 0 (mod 16), so **the prologue
  preserves the misalignment** and its own first `stp` re-faults — infinite
  recursion, never reaching Rust code, no serial output, SP marching downward
  by 288 per iteration (hence the ever-changing `fffe...` garbage SP values;
  2.7 **billion** re-faults were counted in one frozen capture).
- TCG never enforces `SCTLR_EL1.SA`, so the identical binary ran fine under
  software emulation for months. HVF executes guest code natively on the
  M-series core, which enforces it. x86-64 has no equivalent check.
- All 4 earlier barrier/TLBI fix attempts were doomed: the TTBR0-switch block
  in `ret_to_user` was never the trigger (the fault fired one instruction into
  `ret_to_user_fork`, before it).

## 2. The fix (three parts, all committed)

1. **`sched/src/clone.rs`** — round the frame base down to 16 bytes:
   `const FRAME_SIZE: usize = (UserFrame::SIZE + 15) & !15;` in both
   `fork_current` and `clone_thread`. This also matches `exception_asm.s`'s
   288-byte frame convention and `CpuContext::new_user_task_with_pt`, which
   already used 288 for exactly this reason.

2. **`../relibc/src/platform/linux/mod.rs`** — the *user-side* twin bug, found
   minutes later when `pthreadtest` died with the same EC=0x26 at EL0
   (`SCTLR_EL1.SA0` covers user mode): `pthread::create` pushes 8 words
   (64 B, SP stays 16-aligned at `clone`), but the aarch64 child-side unwind
   in `rlct_clone` popped only 40 B (16+16+8), entering `new_thread_shim`
   with SP ≡ 8 (mod 16). Fixed by making the last pop `ldr x3, [sp], #16`,
   consuming one of the padding words (total 48 B).

3. **`userland/*/build.rs`** — `cargo:rerun-if-changed` on `librelibc.a`.
   Cargo does not track external native libraries, so userland binaries were
   silently keeping the libc they were first linked against (pthreadtest's
   last real link predated the fix by days). This is why fix #2 initially
   appeared not to work.

## 3. Verification (2026-07-15)

- aarch64 **HVF**: `mame -rompath / captcomm` runs to the Captain Commando
  info screen (framebuffer screenshot confirmed) — previously froze forever at
  the first fork. `forktest`, `pthreadtest`, `racetest`, `polltest` all PASS,
  repeated twice in one boot.
- x86_64 (TCG): same test suite PASS (the FRAME_SIZE rounding shifts its fork
  frame from -168 to -176; `fork_ret_to_user` pops relative to the frame base,
  so position is immaterial).

## 4. The diagnostic that cracked it (kept in-tree)

`arch/aarch64/src/exception_asm.s` now routes the EL1-SPx sync vector through
`exc_el1_sync_capture`: a stack-free stub (register stashes in `tpidrro_el0` /
`par_el1`, per-CPU state in `.bss` `EL1_SYNC_CAPTURE`, 128 B/cpu) that records
ELR/ESR/FAR/SP/x30/SPSR of the last fault whose ELR lies *outside*
`exc_el1_sync`'s save prologue — i.e. it filters out the recursive re-faults
that had been destroying the evidence, preserving the *first* fault of a
recursion storm. Read it post-mortem via the QEMU monitor:

```
nm -n target/final-aarch64/kernel | grep EL1_SYNC_CAPTURE
# monitor: x /14gx <addr + cpu*128>
# +0 elr  +8 esr  +16 far  +24 sp  +32 x30  +40 spsr   (first/non-recursive)
# +48 elr +56 esr +64 sp   +72 count                    (latest, any)
```

One capture was sufficient to identify the bug after four failed
fix-by-hypothesis attempts. Cost: ~20 instructions per EL1 sync fault
(demand-paging faults only — not on syscall/IRQ paths).

## 5. Post-mortem notes for future sessions

- The four failed attempts all assumed the fault was a translation/TLB
  problem because "HVF-only" pattern-matched to hypervisor TLB quirks. The
  actual lesson: **HVF-only ≡ "anything TCG doesn't model"**, and TCG skips
  most alignment checking (`SA`, `SA0`). When a bug is
  accelerator-dependent, enumerate the architectural checks the emulator
  elides before theorizing about the hypervisor.
- The §7 "debug prints seemed to help once" lead from the original writeup
  was a red herring (UART-contention garbling, as suspected).
- The earlier verification that `child_ctx.sp` was "valid" checked that it
  pointed into the right stack — nobody checked its low 4 bits. The captured
  SP (`0xFFFF0000BFAAFEE8`) was byte-identical to the "known-good" value from
  that print.
