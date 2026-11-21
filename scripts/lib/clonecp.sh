# Portable helpers (POSIX sh / bash). Source this file; do not execute it.
#   clone_cp SRC DST   copy-on-write clone where the filesystem supports it
#                      (APFS clonefile on Darwin, reflink on Linux XFS/btrfs),
#                      falling back to a plain copy.
#   file_size FILE     size in bytes.
if [ "$(uname -s)" = "Darwin" ]; then
    clone_cp() { cp -c "$@"; }
    file_size() { stat -f %z "$1"; }
else
    clone_cp() { cp --reflink=auto -a "$@"; }
    file_size() { stat -c %s "$1"; }
fi
