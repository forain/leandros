#!/usr/bin/env bash
# Build the COSMIC apps shipped in the Applications menu: cosmic-files (file
# manager), cosmic-edit (text editor) and cosmic-screenshot (the program the
# Print key runs via System(Screenshot) in cosmic-comp's default keybindings).
# All three are unmodified upstream epoch-1.3.0 with the m6 recipe:
#   m6-session-bins/build-rust.sh src/<app> <arch>
# Sources live in m6-session-bins/src/<app>; cosmic-edit and cosmic-screenshot
# share cosmic-files' cargo target dir (symlinked) to save disk.
#
# Output: ~/code/leandros-artifacts/m6-session-bins/out/<app>-<arch>
# mkfs stages each as /bin/<app> together with its .desktop (this directory)
# and hicolor icons, or none of them.
#
# Usage: build.sh [aarch64|x86_64|both] [app...]   (default: both, all apps)
set -euo pipefail

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
[[ ${#APPS[@]} -gt 0 ]] || APPS=(cosmic-files cosmic-edit cosmic-screenshot)

if [[ ! -x "$M6/build-rust.sh" ]]; then
  echo "cosmic-apps: no m6 toolchain at $M6 -- not built" >&2
  exit 1
fi

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
    if ! "$M6/build-rust.sh" "$SRC/$app" "$arch" > /dev/null; then
      echo "cosmic-apps: $app build failed for $arch (log: $M6/logs/$app-$arch.log)" >&2
      exit 1
    fi
    install -m 0755 "$SRC/$app/target/$arch-unknown-linux-musl/release/$app" "$M6/out/$app-$arch"
    echo "staged $arch: $M6/out/$app-$arch"
  done
done
