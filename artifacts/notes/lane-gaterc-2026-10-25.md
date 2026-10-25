# lane/gaterc 2026-10-25 (openbugs #88): FIXED

- mamesession.py already exits 1 on a failed gate; added MAMESESSION_INJECT_SETMODE_FAIL=1 (fake "SET_MODE failed" line) to prove it.
  Under runtests: `host:mamesession.py aarch64 gaterc --vt --secs 20` -> RC=1, overall FAIL (mame-vt-setmode pass:false). Normal runs unchanged.
- Audit: doomsession, doom-music-check.sh, blkmanytest, espresettest, vmnethealth etc. already exit nonzero. Fixed the two that did not:
  coreutils-test.py (always exit 0) and udevhottest.py (printed RESULT FAIL, exit 0).
- runtests.py: a host: script whose output has a RESULT line with "pass": false / FAIL is a failure even at rc 0.
- servers/drm/src/lib.rs drm_errno: InvalidParameter -> EINVAL on all paths (was EPERM -1). Regress aarch64: 96 RC=0, overall PASS (incl. drmsmoke).
