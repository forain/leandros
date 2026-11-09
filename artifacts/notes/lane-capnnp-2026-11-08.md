# lane/capnnp 2026-11-08 (ffsbreview1 F4, F7)

F4 FIXED: sched/src/caps.rs on_exec caps new permitted by old permitted when no_new_privs (Linux cap_bprm_creds_from_file). Set-id bits were already ignored under NNP.
captest: exec_nnp_keeps_dropped (root, capset drop all, NNP, exec -> empty), exec_no_nnp_regains (same without NNP -> full). Both PASS aarch64.

F7 PARTIAL: premise differs. All syscalls run with IRQs masked by design (no kernel preemption; long loops use irq_window), so there is no "IRQs enabled" point to move the hook after. Instead SeccompSlot::evaluate opens an irq_window before each filter in the chain. Decision is still taken before dispatch; no migration possible, so the per-CPU slot stays valid. A single filter (up to 32768 insns) still runs masked in one piece; splitting seccomp_bpf::run for that is open.

Tests: captest/seccomptest/sigtest aarch64 PASS; x86_64 on box PASS (3 suites); default regress aarch64: see merge report.
