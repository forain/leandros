# lane/blkshort (2026-10-23) — FIXED (L10.1)

Cause: `servers/vfs/src/block.rs` read_at/write_at capped each call at MAX_XFER (256 KiB) and returned that as a short count.
Fix: read_at/write_at now loop read_chunk/write_chunk until len is satisfied, device end, or error (bytes so far win over the error).
Guard: vfstest `blockdev_full_read` (one 8 MiB read(2) from the raw disk node, default aarch64 virtio-blk), in regress.
Evidence (aarch64 release): guard FAIL on unfixed code (vfstest 0/1); with fix full regress overall PASS, guard PASS.
Not covered: write path has no guard (writing the raw root disk is unsafe in the suite); x86_64 not run per lane rules.
