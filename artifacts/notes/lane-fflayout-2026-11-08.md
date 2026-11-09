# lane/fflayout 2026-11-08
FIXED C: /etc/ld.so.conf RamEntry (/lib, /usr/lib, /usr/local/lib) + /usr/local/lib dir in image (mkfs-f2fs-populated.py); musl search path unchanged.
FIXED D: /etc/localtime RamEntry = minimal v1 TZif UTC (no tzdata in world).
Evidence: ffsandbox aarch64 on: 0 libsqlite3/localtime lines; regress aarch64 PASS.
Open: /sys/devices/system/cpu/present denial (lane syscpu).
