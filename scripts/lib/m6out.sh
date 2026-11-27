# Per-tree COSMIC/m6 session-binary staging dir (sourced; needs no args).
# Default: <this worktree>/target/m6-out. Override: LEANDROS_M6_OUT=<dir>.
# cosmicbin: trivial comment (cache hit check)
# M6_OUT_SHARED is the old shared dir, read as a fallback baseline only
# (mkfs-f2fs-populated.py prefers M6_OUT, then falls back to it per file).
M6_OUT="${LEANDROS_M6_OUT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/target/m6-out}"
M6_OUT_SHARED="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}/m6-session-bins/out"
mkdir -p "$M6_OUT"
# cache check
