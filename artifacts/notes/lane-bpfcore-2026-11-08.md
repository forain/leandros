# lane/bpfcore 2026-11-08 - DELIVERED

New pure crate `lib/seccomp-bpf` (no_std + alloc, no deps), workspace member. No kernel integration.
API: SeccompData, SockFilter (= Insn), Program, Error, check_filter(&[SockFilter]) -> Result<Program, Error>,
run(&Program, &SeccompData) -> u32, action_only, precedence_min(best, cur), run_stack(newest_first, &data), SECCOMP_RET_* consts.
Notes: BPF_MOD rejected (not in Linux's seccomp whitelist). Shifts by X mask to 5 bits; DIV by X==0 returns 0.
Precedence uses signed (s32) compare so KILL_PROCESS wins; equal actions keep the first-traversed (newest) filter.
check_load_and_stores replicates Linux (no memvalid reset after RET).
Tests: cargo test -p seccomp-bpf --target aarch64-apple-darwin (6 tests green).
