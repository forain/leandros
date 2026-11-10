# Lane mmuserns (2026-11-10)

Status: FIXED (security parity), merged from lane/mmuserns.

- `mm_userns` tracked on tasks; shared `ptrace_gate` in sched/src/cred.rs applies the non-dumpable
  CAP_SYS_PTRACE rule against the target's mm->user_ns (Linux __ptrace_may_access).
- nstest: `userns_cap_nondumpable_denied` (failed on main, now passes), `userns_cap_exec_in_ns_allowed`.
- `nobody_opens_child_ns` encoded a wrong premise (root pasta). On real Linux (Arch,
  `sudo pasta --config-net -- true`) it fails with EACCES: setuid(65534) clears dumpable and
  mm->user_ns stays init. Replaced by `nobody_drop_child_ns_denied` (asserts EACCES) and
  `user_opens_child_ns` (no id change, dumpable, open succeeds). No PR_SET_DUMPABLE workaround.
- Decision record: orch/designs/dumpable-decision.md.

Rule for pasta lanes: test ROOTLESS pasta (uid 1000, standalone) and the podman `--netns <path>`
path, not root pasta. Root pasta is EACCES on Linux too; use `pasta --runas 0:0` for a root case.
