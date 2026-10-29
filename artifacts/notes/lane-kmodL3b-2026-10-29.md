# lane kmodL3b (2026-10-29): kmod-abi + kmod-elf

DELIVERED. Branch lane/kmodL3b. New: lib/kmod-abi, lib/kmod-elf; workspace members added. No kernel file touched.

- Host: `cargo test -p kmod-abi -p kmod-elf --target aarch64-apple-darwin` (5 tests in kmod-elf: layout/sizes, round-trip both machines, tailcall bytes, one negative per ElfErr variant (17 reachable + SlotNotInWindow/BuildId via Expect), 20k-iteration truncation/bit-flip fuzz).
- Builds for x86_64/aarch64-unknown-none and the two kernel target specs with -Z build-std=core,alloc.
- Frozen sizes (const-asserted with field offsets): KmodHeader 128, KmodCtx 24, KSym 16, KmodId 2.
- Window constants only in kmod-abi `layout`.
- Deviations: `sys` module is empty (L3c list not yet defined); KmodHeader got from_bytes/write_bytes helpers; synth's first PT_LOAD (R) holds header+rodata, then RX text, then RW data; file offset = 4096 + (vaddr - slot_base); synth init = text start. Added NSLOTS, EM_*, FLAG_* consts. Header-malformed cases (size, slot_base != first vaddr) map to NoHeader; unaligned/zero slot geometry maps to SlotNotInWindow.
