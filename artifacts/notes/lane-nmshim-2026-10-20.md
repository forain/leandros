# lane nmshim (openbugs L9.7), 2026-10-20

Four commits on lane/nmshim:
- virtio-net reports a nominal 1000 Mb/s while up (drivers/src/netdev.rs).
- netctl + NM shim take flock(LOCK_EX) on profiles.a around every profile
  read-modify-write (backend.rs ProfileStore::lock, netctl common.rs SlotLock).
- Device.StateReason keeps the failure reason (Iface.last_fail) past the FAILED
  hold until the next activation (model.rs). Baseline: the tightened nmtest case
  "AP that refuses association fails, no hang" was RED before
  ("connection failed: unknown"), green after.
- Engine stamps connection.timestamp (timestamp_s) in the slots on each successful
  Wi-Fi activation (engine.rs stamp_used); nmtest after-reboot checks timestamp > 0.

Test notes: `--suite wifisim` only runs wifisimtest.py; the shim's real-stack test
is the separate host:nmrealtest.py (run it with runtests host:nmrealtest.py).
