# lane/vforkflt 2026-10-23 -- L10.6 FIXED

Bug: page faults of a vfork (CLONE_VM, no CLONE_THREAD) child before exec were added to the
shared AddressSpace min_flt/maj_flt, i.e. charged to the parent (/proc/<pid>/stat 10/12, getrusage).

Fix: Task/Slot `mm_borrowed` (set in clone for CLONE_VM without CLONE_THREAD, cleared in
replace_address_space). While set, the fault path only bumps the thread's slot counters;
`process_faults` and `group_acct` (wait/RUSAGE_CHILDREN) read the slot counters for such a task;
at exec the child's pre-exec counts seed the new address space.

Guard: pstest `test_vfork_faults` (raw clone(CLONE_VM|CLONE_VFORK) child touches 64 pages of a
parent-mapped region). Unfixed: parent minflt 131 -> 195; fixed: 131 -> 131 (aarch64).
