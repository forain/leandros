# lane buildguard 2026-11-11 (FIXED; Doom item FIXED in libc, not musl)

- mkfs-f2fs-populated.py: new `_image_missing()` makes a missing libseat.so.1/libudev.so.1 (neither fresh shim nor m4 blob), /bin/brush or /bin/coreutils a SystemExit; LEANDROS_ALLOW_PARTIAL_IMAGE=1 downgrades to warnings. /bin/btm absence is a warning, /bin/wl-globals a note (measurement-only).
- Fail-first: with LEANDROS_BRUSH_DIR=/nonexistent the old script exits 0 and writes a shell-less image; the new one exits 1 with "required image payload missing: /bin/brush".
- build-all.sh build_brush: explicit failure when the ../crossterm sibling is absent.
- .cargo/config.toml + userland/.cargo/config.toml: musl targets get relocation-model=static (+ the zig linker wrappers) for bare `cargo build`; build-userland.sh's RUSTFLAGS still overrides.
- userland/libc/src/mem.rs: bump allocator replaced by size-binned free lists (exact bins to 4 KiB, first fit above), spinlock, real free/realloc; Doom (links this libc) now reuses memory. Not musl.
- Artifact trees: documented in docs/development/conventions.md (repo copy canonical for tracked-source checks, ~/code/leandros-artifacts for blobs; 7 shared files differ). Neither deleted.
- Verified: full aarch64 + x86_64 build-all exit 0; aarch64 boot, /bin/memtest PASS.
