# lane/greetdfix (L11.15, L11.10a) 2026-10-23

FIXED L11.15. The shipped greetd (leandros-artifacts/greetd-lane, 2026-08-08) embedded the old pam-leandros that only
verified $sha256$; /etc/shadow holds $6$rounds= since secmisc, so greeter login failed with pam_authenticate AUTH_ERR.
- Guard: scripts/mkfs-f2fs-populated.py `_check_greetd_fresh` refuses a greetd lacking the "rounds=" marker or older
  than ports/greetd/pam-leandros/src. Shown failing on the August binary.
- Rebuilt both arches with ports/greetd/build.sh (rounds= count 0 -> 1); old binaries kept as greetd.old-0808.
- Probe: greeter login on aarch64 --virgl succeeds, 0 AUTH_ERR in greetd.log, /run/user/1000/bus present.
- L11.10a REFUTED: bench portal dialog first=1.13 s (settled 1.69), wallpaper2 dialog 1.42 s; no 60 s stall.
- Regress aarch64: PASS (sshsmoke and netctltest passed this run too).
