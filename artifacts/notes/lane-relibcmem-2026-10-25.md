# lane relibcmem 2026-10-25 — which mem* does each C library in the image use (openbugs #85)

Verdict: FIXED for zig-static musl memset (aarch64 byte loop); everything else already word/vector.

| library | used by | mem* impl (disassembled) | note |
|---|---|---|---|
| libleandros_libc.a | DOOM, C demos | rep movsq / ldp-stp (lane shmslow) | fixed 158d3880 |
| relibc libc.a (x86_64, aarch64) | relibc no_std Rust bins (drmsmoke, epolltest, ...) | Rust loop auto-vectorised by LLVM: SSE 32 B/iter, NEON ldp q | not a byte loop; not changed |
| musl-dynamic libc.so (x86_64, aarch64) | Firefox, Mesa, kmscube, dynamic musl bins | musl 1.2.5 asm (rep movsq / NEON) | fine |
| rustc self-contained musl libc.a | rust musl targets w/o zig | musl asm | fine |
| zig static musl (compiler_rt mem*) | zig-linked Rust bins (sshd, claude-run, blanktest, killmt, ripgrep, coreutils, bottom, ...) and zig C tests | memcpy word/vector; **aarch64 memset = byte loop** (strb; subs; b.ne); x86_64 memset byte/word mix | FIXED |

Fix: scripts/linker-<arch>-musl.sh now appends musl's asm memset (scripts/memset-<arch>.S, assembled by
scripts/musl-mem-obj.sh); a strong object overrides zig's weak compiler_rt symbol. build-all.sh copies the
helper files to ../.leandros-tools with the wrappers. New /bin/memcpybench-musl (zig static musl build of memcpybench).

Numbers (aarch64 QEMU HVF, 256000 B memset): zig musl before 65 us (3.9 GB/s) -> after 3 us (85 GB/s);
memcpy unchanged 5 us (45 GB/s). aarch64 regress PASS. x86_64 not run (object verified by disassembly only).
