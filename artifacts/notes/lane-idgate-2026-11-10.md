# lane idgate 2026-11-10
DELIVERED: idmaptest e2e_subuid + nested_map_composed; nsroot_opens_ns_of_mapped_user test fixed (Linux denies non-dumpable target w/o mm_userns cap).
Results and verdict: leandros-artifacts/orch/designs/idflip-gate.md. Full regress under idmap=test: a64 81/1 bad, x86 81/1 bad (the fixed case).
