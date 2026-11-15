# lane/enallq (EC2 design lane L5) - 2026-11-15

DELIVERED: ENA LLQ (TX device placement), mock-verified.
- `ena_hw.rs`: `LlqCaps` parse (GET_FEATURE LLQ), `llq_negotiate` (inline header only; 128 B preferred, then 256, 192; 2 descs before header preferred, then 1/4/8; multi-desc stride preferred), SET_FEATURE LLQ with the enabled words, CREATE_SQ placement 3 (no host SQ address), entry write via `EnaBus::llq_write` + `EnaBus::wmb` before the doorbell. Fallbacks to host placement: no LLQ caps, no usable layout, no BAR2, `ena.llq=0`, CREATE_SQ refused (bring-up redone on host memory).
- `ena.rs`: BAR2 mapped `CacheMode::WriteCombining` (existing kva mode: x86 PAT WC, a64 Normal-NC), 8-byte volatile stores, `sfence` / `dsb st`, `ena.llq=0`, log + /proc/net/nic lines.
- Mock (`drivers/ena-tests`): BAR2, LLQ caps, entry decoder, fence check; 36 tests (15 new: negotiation matrix, entry layout, 10k loopback across ring wrap in LLQ mode, fallback x5, 256 B entries, reset, depth clamp, fencing).
- Not in the public headers as far as remembered: a 64 B entry size, so 64 is not offered.

UNPROVEN (needs real EC2, gate G1): LLQ feature body offsets, SET_FEATURE LLQ body, meta descriptor bits, `header_length` position (word3 bits 31:24), packet descriptor `length` meaning (total frame, host buffer = frame after the pushed header), whether a fully pushed frame still needs a data descriptor. Layouts are from memory of the public Linux/DPDK ena headers and encoded identically in driver and mock.
