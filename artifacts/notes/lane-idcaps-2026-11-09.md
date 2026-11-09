# lane/idcaps 2026-11-09 (design-idmap lane 6, hazards H2 + H14)

Status: see bottom (results appended after the box runs).

## Change (sched/src/caps.rs only)
`on_exec` and `on_uid_change` compare kernel uids with `ns::root_k(task's userns)` instead of literal 0.
`root_k` is `to_k(0, ns, 0)`: lock-free, safe under RUN_QUEUE. `None` (id 0 unmapped) means nobody is root.
prctlcaps semantics are kept: ambient set, securebits (NOROOT, NO_SETUID_FIXUP, KEEP_CAPS), P' = (root ? I|bnd : 0) | A',
E' = root-euid ? P' : A', set-id exec clears A, NNP clamp `p &= old permitted`, KEEP_CAPS cleared on exec.
H14: comment block in caps.rs (fsuid DAC-cap fixup not modelled).

## Behaviour changes (complete list)
1. Namespace 0 (every task today without a userns): root_k = 0, so identical to before. No change.
2. Task in a userns, euid == root_k(ns) (e.g. kuid 1000 with map `0 1000 1`): exec now gives P' = I|bnd,
   E' = P' (was: A' only, caps lost). Needed for podman re-exec / crun exec as ns-root.
3. Task in a userns with a non-root euid/uid: exec gives P' = A' only, as before; but now "root" is not kuid 0.
4. kuid 0 inside a userns whose root_k != 0 (e.g. host root that unshared, map `0 1000 1` written): no longer
   root for exec / setuid fixup (was: full caps on exec). Closes the "host root is ns-root" escalation direction.
5. `on_uid_change`: the drop-all-caps rule (all of uid/euid/suid leave root), the E-clear on euid leaving root and the
   E=P restore on euid entering root now use root_k. setresuid(5,5,5) from ns-root (kuid 1000) now drops caps
   (was: kept, since euid was never literal 0).
6. Userns whose uid map is not yet written (root_k None): `on_uid_change` is a no-op (returns early, no fixup),
   `on_exec` gives root nobody: P' = A', E' = A'. Previously kuid 0 tasks kept root semantics there.
   Linux behaves the same (uid_eq with an invalid kuid is false).
7. Unchanged: fsuid cap fixup (H14), caller code in lib.rs/cred.rs (the Task already carries userns), capable()/ns_capable().
   euid-0 gates elsewhere (VFS, setid_priv, etc.) still test literal euid 0: other lanes (idperm-*, idroot, idxlate).

## Tests (userland/idmaptest, probe = re-exec of idmaptest --capprobe reading CapEff/CapPrm)
- exec_drops_caps_host (no knob): host root, KEEPCAPS + setresuid(1000), exec -> caps empty. Passes before and after.
- Knob (idmap=test), userns map `0 1000 1` + `5 5 1`: exec_nsroot_keeps_caps, setuid_drops_caps, exec_nonroot_nocaps
  (KEEPCAPS + setresuid(5), exec -> empty; a no-regression/over-grant check, also passes on main),
  kuid0_not_nsroot (task stays kuid 0, map root=1000, exec -> empty).
- Guard on main (tests only, box aarch64 `--cmdline idmap=test /bin/idmaptest`): FAIL exec_nsroot_keeps_caps,
  setuid_drops_caps, kuid0_not_nsroot; pass exec_drops_caps_host, exec_nonroot_nocaps.

## x86_64 box regress (2026-11-09)
lane/idcaps ecbf2592: 75 suites run, 0 bad, boots failed 0/1, PASS (1014s).
