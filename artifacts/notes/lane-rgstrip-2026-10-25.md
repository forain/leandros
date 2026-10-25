# lane/rgstrip 2026-10-25 - FIXED

build_ripgrep now passes `--config 'profile.release.strip="symbols"'`.
rg size: aarch64 33191096 -> 4192472 B; x86_64 31884520 -> 4526528 B.
aarch64 QEMU regress: overall PASS, ripgrep case (rg --version, -n, -l, stdin, rc 1 on no match) RC=0.
