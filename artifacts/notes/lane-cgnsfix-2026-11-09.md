# lane/cgnsfix 2026-11-09 - FIXED

cgns_in_userns failed after idcore rule 8 (userns creation needs the creator's euid/egid mapped).
Linux does the same (create_user_ns: EPERM when !kuid_has_mapping), so the TEST was wrong: it cloned
NEWUSER from a userns whose uid_map was never written. Fix: map_self_1000() before the nested clone.
Evidence: Mac aarch64 hvf /bin/nstest PASS (fails=0, 34.8s). Box runs failed at boot (login exec failed,
farm issue, 0 cases run); x86_64 and cgtest/idmaptest not run.
