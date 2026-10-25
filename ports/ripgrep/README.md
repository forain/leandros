# ripgrep

`rg` is built UNMODIFIED from the sibling checkout `../ripgrep` at release tag
15.2.0 (e89fff8) by `build_ripgrep` in scripts/build-all.sh and staged as
/bin/rg by scripts/mkfs-f2fs-populated.py. The guest image ships no grep.

`jemalloc-stub` replaces tikv-jemallocator (via `cargo --config patch`) because
jemalloc's C build does not cross-compile here; the result uses musl's malloc.
The patch rewrites ../ripgrep/Cargo.lock, which build_ripgrep restores.
