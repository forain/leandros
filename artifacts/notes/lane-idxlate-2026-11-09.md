# lane idxlate (design-idmap lane 5) - 2026-11-09

Status: DELIVERED. Ids arriving at set*id, chown family, setfs*id, setgroups and getgroups are translated; `ids_mapped` is deleted.

## What changed
- sched/src/ns.rs: new `arg_id(kind, id)` (user-space id -> kernel id via `to_k`, u32::MAX passes, unmapped = None); `ids_mapped` removed.
- kernel/src/syscall.rs: setuid/setgid/setresuid/setresgid/setreuid/setregid pass kuids on (EINVAL when unmapped); setfsuid/setfsgid translate in and answer the old id through `view_id`; chown/fchown/fchownat hand kuids to the VFS (-1 kept); setgroups translates each gid (EINVAL if unmapped; EPERM before gid_map stays); getgroups out-translates with `view_id`.
- idmaptest: new cases chown_unmapped_einval, setgroups_needs_gidmap (no knob), setuid_maps_down, chown_maps_down, setfsuid_ret_view, getgroups_view (knob). All 14 existing cases kept.
- idstat's `sigchld_si_uid_view` restored to its ORIGINAL expectation (handler sees 0): the body now does setresgid/setresuid(0) first (kuid 100000), so the sender is mapped; idcore's temporary 65534 expectation is gone.

## Evidence
- Guards fail on the pre-change kernel (base 1ad1d5a1, aarch64 Mac, idmap=test): 5 failures, 22 passed (chown_maps_down, getgroups_view, setfsuid_ret_view, setuid_maps_down, sigchld_si_uid_view).
- With the change: knob run 27 passed 0 failed (Mac aarch64, and box aarch64 /bin/idmaptest --cmdline idmap=test PASS). No-knob cases SKIP the non-identity ones.
- Box full regress after merging main (b9a910d8): x86_64 PASS (723 s), aarch64 PASS (1016 s). An earlier box x86_64 run showed a stale nstest cgns_in_userns failure; a gc'd rerun was green.
- chown_unmapped_einval and setgroups_needs_gidmap also pass on the old kernel (idcore already guarded those paths); they are kept as regression cases.

## Open
- setid_priv (ns!=0 uses caps bit) and fsuid cap fixup stay with idcaps. ACL/xattr/VFS euid==0 gates stay with idperm.
