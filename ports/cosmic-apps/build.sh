#!/usr/bin/env bash
# Build the COSMIC apps shipped in the Applications menu: cosmic-files (file
# manager), cosmic-edit (text editor) and cosmic-screenshot (the program the
# Print key runs via System(Screenshot) in cosmic-comp's default keybindings).
# All three are unmodified upstream epoch-1.3.0 with the m6 recipe:
#   m6-session-bins/build-rust.sh src/<app> <arch>
# Sources live in m6-session-bins/src/<app>; cosmic-edit and cosmic-screenshot
# share cosmic-files' cargo target dir (symlinked) to save disk.
# cosmic-monitor and cosmic-initial-setup follow the same recipe; the latter is
# staged with its .desktop but is NOT run at first login.
#
# Output: ~/code/leandros-artifacts/m6-session-bins/out/<app>-<arch>
# mkfs stages each as /bin/<app> together with its .desktop (this directory)
# and hicolor icons, or none of them.
#
# Usage: build.sh [aarch64|x86_64|both] [app...]   (default: both, all apps)
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/stage-lib.sh"

M6="$HOME/code/leandros-artifacts/m6-session-bins"
SRC="$M6/src"

case "${1:-both}" in
  aarch64)  ARCHES=(aarch64) ;;
  x86_64)   ARCHES=(x86_64) ;;
  both|"")  ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both] [app...]" >&2; exit 2 ;;
esac
shift || true
APPS=("$@")
[[ ${#APPS[@]} -gt 0 ]] || APPS=(cosmic-files cosmic-edit cosmic-screenshot cosmic-store cosmic-player cosmic-monitor cosmic-initial-setup)

if [[ ! -x "$M6/build-rust.sh" ]]; then
  echo "cosmic-apps: no m6 toolchain at $M6 -- not built" >&2
  exit 1
fi

# No gvfs (needs a GLib/GIO SDK the sysroot lacks) and no io-uring (LeandrOS
# has no io_uring; compio falls back to its poll driver).
feat_for() {
  case "$1" in
    cosmic-files) echo "--no-default-features --features bzip2,dbus-config,desktop,lzma-rust2,notify,wayland,wgpu" ;;
    cosmic-edit)  echo "--no-default-features --features dbus-config,wayland,wgpu" ;;
    # No libflatpak (GLib/GIO) and no packagekit: appstream is pure Rust, so the
    # store starts with no backend. Upstream feature flags only, no source patch.
    cosmic-store) echo "--no-default-features --features dbus-config,desktop,logind,notify,packagekit,single-instance,wayland,wgpu,xdg-portal" ;;
    # Upstream default features (mpris-server, xdg-portal, wgpu, wayland); its
    # GStreamer stack comes from ports/gstreamer (merged into the sysroot).
    cosmic-player) echo "" ;;
    cosmic-monitor) echo "--no-default-features --features dbus-config,wayland,wgpu" ;;   # no nvml
    cosmic-initial-setup) echo "--no-default-features --features wgpu,wayland" ;;
    *)            echo "" ;;
  esac
}

# cosmic-store's reqwest pulls aws-lc-sys, whose C objects reference
# __stack_chk_guard; the sysroot libc.so does not export it (same gap as
# ports/mesa/ssp_guard.c), so link the same shim object in.
extra_for() {
  [[ "$1" == cosmic-store ]] || return 0
  local o="$M6/out/ssp_guard-$2.o"
  "$M6/toolchain/$2-linux-musl-cc" -fno-stack-protector -c "$(dirname "$0")/../mesa/ssp_guard.c" -o "$o" || return 1
  echo "--config=target.$2-unknown-linux-musl.rustflags=[\"-C\",\"link-arg=$o\"]"
}

for app in "${APPS[@]}"; do
  if [[ ! -f "$SRC/$app/Cargo.toml" ]]; then
    echo "cosmic-apps: no source at $SRC/$app -- not built (git clone https://github.com/pop-os/$app, tag epoch-1.3.0)" >&2
    exit 1
  fi
  # one shared cargo target dir (cosmic-files'), if the app has none of its own
  if [[ "$app" != cosmic-files && ! -e "$SRC/$app/target" ]]; then
    ln -s ../cosmic-files/target "$SRC/$app/target"
  fi
  [[ -f "$SRC/$app/.cargo/config.toml" ]] || sh "$M6/gen-cargo-config.sh" "$SRC/$app" > /dev/null
  for arch in "${ARCHES[@]}"; do
    if ! "$M6/build-rust.sh" "$SRC/$app" "$arch" $(feat_for "$app") $(extra_for "$app" "$arch") > /dev/null; then
      echo "cosmic-apps: $app build failed for $arch (log: $M6/logs/$app-$arch.log)" >&2
      exit 1
    fi
    stage_file 0755 "$SRC/$app/target/$arch-unknown-linux-musl/release/$app" "$M6/out/$app-$arch"
    echo "staged $arch: $M6/out/$app-$arch"
  done
done
