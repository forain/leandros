# lane/vncport 2026-10-25 (openbugs L12 #60)

Bug: every session script did `setdefault("LEANDROS_VNC_PORT", "59xx")`, so two concurrent
sessions of the same script (or doomsession + another lane) both bound the same port; the second
QEMU died with `-vnc ...: Failed to find an available port: Address already in use` -> "boot failed".

Fix (driver.py + doomsession/ffsession/nmsession/gpu-session-loop): the scripts now set
`LEANDROS_VNC_BASE` (a starting point) instead of the port. An explicit `LEANDROS_VNC_PORT` is still
honoured verbatim. Otherwise `cmd_start` picks the first bindable port >= base under a host-wide
flock (/tmp/leandros-vnc-pick.lock) that is held until QEMU has bound it, so two simultaneous starts
cannot choose the same port. The chosen port is written to /tmp/leandros-<RUN_ID>-vnc-port, which
screenshot (and every other consumer) already reads.

Evidence: two doomsession.py aarch64 runs started together (distinct RUN_IDs, no VNC port, qemu lock):
main -> run 1 "Address already in use", boot failed; run 2 fine. Branch -> both booted and reached the DOOM menu screenshot, VNC ports 5938 and 5939.
Note: concurrent runs also need their own LEANDROS_DISK_IMAGE / LEANDROS_ROOT_IMAGE clones (shared repo images take a write lock).
Not covered: scripts/vnclat.py and zinkbench.py still take LEANDROS_VNC_PORT/5909 by default (read the state file if needed).
