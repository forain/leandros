# lane/capvfs 2026-11-11: FIXED
VFS inode checks (tmpfs + f2fs chown/chmod/open DAC/xattr) no longer treat euid 0 in the initial userns as all-powerful.
- vfs `cred_with_ns` now fills `Cred.caps` for ns 0 too (task's effective set; `real` flavour for access()). Unknown pid keeps `Cred::new` (euid 0 => all caps, boot path).
- xattr crate: `cap_inode` has no host_root bypass (HOST view maps every id); access_check evaluates CAP_DAC_OVERRIDE/READ_SEARCH in every ns; trusted.*/security.* need CAP_SYS_ADMIN in ns 0 (`host_sysadmin`); `host_root` is now only a fast path = euid 0, ns 0 and all six inode caps held; mount-root gates still use it (stricter, never looser).
- Never more than before: non-root ns-0 tasks have caps 0 unless Linux gives them (exec/ambient rules already in caps.rs).
Evidence: captest +8 cases (tmpfs and f2fs chown/chmod/DAC/trusted with one cap dropped from root) FAIL 9 checks on the unfixed kernel (x86_64 box), PASS after (aarch64). Podman root bridge PASS, rootless pasta PASS except pasta_portfwd (known, lane pastabisect). Full box regress both arches: see status line.
Open: security.capability should need CAP_SETFCAP (still SYS_ADMIN); userns-root cases unchanged.
