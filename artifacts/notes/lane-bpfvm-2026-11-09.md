# lane/bpfvm 2026-11-09 - DELIVERED

New no_std crate `lib/ebpf-cgdev` (workspace member, no dependencies, no kernel wiring):
Insn decode/encode, `verify(&[Insn]) -> Result<Program, Error>` (`Error::errno()` gives EINVAL/EACCES),
`Program::load(bytes)`, `run(&Program, &DevCtx) -> u32`.

Verifier: forward-only jumps, 1..=4096 insns, no unreachable insns, no fall-off, r10/CALL/LD_IMM64/ST/STX
rejected, per-reg Uninit/Ctx/Scalar(const) state merged at joins, ctx reads only at access_type (W at 0,
B/H inside 0..4), major (W at 4), minor (W at 8), r0 const 0/1 at every EXIT (constants differing on
joined paths fail EINVAL, so emit one EXIT per verdict as crun/runc do).
Tests: 22 host tests (`cargo test -p ebpf-cgdev --target aarch64-apple-darwin`): a Rust port of crun's
generator (default OCI set, privileged, c 10:* rw, overrides, ro block, empty) checked against an
independent model on a dense grid; runc-style ALU32/JMP32/narrow-load program; negative cases.
cargo check --release OK on aarch64-unknown-none and x86_64-unknown-none. Kernel does not depend on the
crate yet, so no guest regress. Next: lane/bpfsys.
