# lane/kexecblk 2026-11-13 - FIXED (test-side; not a kernel regression)
- Repro: `box/regress.sh main x86_64 --suite kexecblk` fails 3 cases (gen0 virtio_blk is a module, gen1 loaded virtio_blk, gen1 lists virtio_blk).
- Cause: regress.sh builds the stock kernel (VIRTIO_BLK=y), so /proc/modules has no virtio_blk. The suite is only meaningful on the =m kernel built by scripts/kmod/kexecblktest.sh. Named suites (kexecblk/kexecfdt/kexecacpi) are not in the default regress.
- Evidence: `scripts/kmod/kexecblktest.sh x86_64` and `aarch64` (Mac TCG) both PASS on main e653ea48. No kernel bug, no bisect needed.
- Fix: kexecblktest.py prints `RESULT: SKIP` (rc 0) when virtio_blk is not a module; the wrapper script still requires `RESULT: PASS`.
- Verified: box x86_64 kexecblk (SKIP, green), kexecfdt PASS, kexecacpi PASS.
