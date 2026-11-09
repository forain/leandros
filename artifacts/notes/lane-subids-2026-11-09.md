# lane subids (2026-11-09)

Status: DELIVERED (design-idmap lane 4).

- /etc/subuid, /etc/subgid = `leandro:100000:65536`, 0644 root, staged in scripts/mkfs-f2fs-populated.py; 100000-165535 reserved (comment next to passwd).
- vfs procsys (RAMFS): /proc/sys/kernel/overflowuid, overflowgid = 65534; /proc/sys/user/max_user_namespaces = 255 (new dir /proc/sys/user). Read-only (open for write fails).
- Installer: leandros-install creates no users (copies the running root tree), so subuid/subgid carry over for both normal and dual-boot installs; no code change needed.
- pi_sync.py: the manifest is built from the build image, so both files sync as ordinary files; no change needed.
- userland/idmaptest/idmaptest.c: minimal file with `subuid_present` and `procsys_overflow` (idcore had not created it; conflict to be resolved at merge, keep idcore's file and append these two functions). Wired in build-userland.sh, mkfs list, runtests default list and `idmap` group.
- Open: podtrace may add subuid entries; keep this as the single source.
