# lane/bkperm 2026-10-30 - brightness keys: login1 write EACCES

FIXED (kernel VFS only). login1 runs as the session user (uid 1000, groups incl. 44), VFS allowed sysclass writes only for euid 0.

- servers/vfs/src/sysclass.rs: `VIDEO_GID = 44`, `is_video_writable` (backlight/*/brightness only), pure `write_allowed(euid, egid, groups, writable, video_attr)` + unit test (host-run via rustc --test: pass).
- servers/vfs/src/lib.rs: open-for-write uses `sysclass_write_ok` (root, or egid/supplementary 44 for backlight brightness only); stat of that attribute is 0664 root:44 via write_stat_owned. All other attributes unchanged (root-only, 0644/0444). access() falls back to kernel stat-based check, so it follows the stat mode/owner. No leds class exists in sysclass.
- userland/permtest: cases backlight_nogroup / backlight_group (EACCES without 44, open OK + 0664 + gid 44 with 44, EACCES for max_brightness even with 44). SKIPs when no intel_backlight (aarch64).
- Evidence: aarch64 default regress PASS; aarch64 `/bin/permtest` PASS (positive part SKIP).
- permtest is not part of the default regress; run `/bin/permtest` explicitly.
