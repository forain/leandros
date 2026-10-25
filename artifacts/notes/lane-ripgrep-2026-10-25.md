# lane/ripgrep 2026-10-25 -- openbugs #48: ship ripgrep (DELIVERED)

- Sibling checkout `../ripgrep` at release tag 15.2.0 (e89fff8), unmodified.
- `build_ripgrep` in scripts/build-all.sh (pinned nightly, musl linker wrapper, skipped when absent);
  `/bin/rg` staged by scripts/mkfs-f2fs-populated.py. No grep symlink: no similar tool uses that pattern.
- jemalloc: upstream's musl build links tikv-jemallocator, whose C configure/link failed under zig
  (undefined `_rjem_*`). Replaced via `cargo --config patch` with `ports/ripgrep/jemalloc-stub`
  (System allocator); Cargo.lock is restored after the build so the checkout stays clean.
- aarch64 QEMU (HVF): `rg --version` = 15.2.0; `rg -n root /etc/passwd` rc 0; `rg -l root /etc` walks dir;
  `echo foo | rg foo` rc 0; no match rc 1. Regress case added in runtests.py (shell one-liner).
- x86_64: build only (integ batch tests it).
- Binary is 33 MB (unstripped release); image grew accordingly. Strip is an open option.
