# lane/integfix 2026-10-23 - FIXED (L11.15, L11.16)

L11.16: blkfaulttest/blkhangtest searched `driver log` (trimmed serial.log) where the
"VIRTIO-BLK ... disk marked dead" line had rotated out; the driver still emits it.
Both tests now also search /tmp/leandros-<id>-serial-full.log. blkfaulttest also calls
drv("stop") before drv("start") (runtests leaves its VM up). Standalone PASS and
`runtests.py aarch64 bf --suite blkfault` PASS (RC=0, 49 s). Full aarch64 regress PASS.

L11.15: the [PAN] line (FAR=0x40010000) is the deliberate sys_secprobe probe (strict mode;
audit mode already suppresses it). Not a missed raw access, so not routed via uaccess; the
strict-mode line is now tagged "(expected: sys_secprobe probe)". Regress log has 0 untagged [PAN] lines.
