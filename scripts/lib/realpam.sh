# realpam_stub <arch> <dir>: put Alpine's real Linux-PAM libs (libpam.so, libpam_misc.so
# and their sonames, taken from the locked linux-pam apk) in <dir> so a dynamic
# Rust/C build links against the real libpam, not a stub.
realpam_stub() {
  local arch="$1" dir="$2" repo
  repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  mkdir -p "$dir"
  python3 - "$repo" "$arch" "$dir" <<'PY'
import sys, importlib.util, tarfile, os
repo, arch, dir_ = sys.argv[1:4]
spec = importlib.util.spec_from_file_location("lk", repo + "/scripts/alpine-lock.py")
lk = importlib.util.module_from_spec(spec); spec.loader.exec_module(lk)
lock, _, _ = lk.read_lock(arch)
p = lock["linux-pam"]
tree = lk.extract_one(arch, p)
for so in ("libpam.so.0", "libpam_misc.so.0"):
    src = os.path.realpath(os.path.join(tree, "usr/lib", so))
    dst = os.path.join(dir_, so)
    if os.path.lexists(dst): os.remove(dst)
    os.symlink(src, dst)
    base = so.rsplit(".so.", 1)[0] + ".so"
    if os.path.lexists(os.path.join(dir_, base)): os.remove(os.path.join(dir_, base))
    os.symlink(so, os.path.join(dir_, base))
PY
}
