# lane/mkfsguard 2026-10-23 - FIXED
mkfs-f2fs-populated.py `_check_greetd_fresh` no longer compares mtimes (fresh worktrees always misfired).
Now: `rounds=` marker check plus sha256 of ports/greetd/pam-leandros/src compared with sidecar greetd.srchash
(written by ports/greetd/build.sh; missing sidecar = warning, marker only). Sidecars generated for aarch64+x86_64.
Evidence: fresh worktree image build OK untouched; greetd.old-0808 rejected (no marker).
