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

. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib/m6out.sh"
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib/cosmic-cache.sh"
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
    # Upstream feature flags only, no source patch. `flatpak` links libflatpak.so.0
    # (ports/libflatpak: Alpine flatpak 1.14 + closure, staged in the image, -dev tree
    # merged into the cross sysroot) next to the PackageKit apk backend.
    cosmic-store) echo "--no-default-features --features dbus-config,desktop,flatpak,logind,notify,packagekit,single-instance,wayland,wgpu,xdg-portal" ;;
    # Upstream default features (mpris-server, xdg-portal, wgpu, wayland); its
    # GStreamer stack comes from ports/gstreamer (merged into the sysroot).
    cosmic-player) echo "" ;;
    cosmic-monitor) echo "--no-default-features --features dbus-config,wayland,wgpu" ;;   # no nvml
    cosmic-initial-setup) echo "--no-default-features --features wgpu,wayland" ;;
    *)            echo "" ;;
  esac
}

# Target dirs: cargo's fingerprint/metadata hash ignores the precise git rev of
# a branch/tag dependency, so two apps locked to different libcosmic revs that
# share one target dir overwrite each other's libcosmic/iced artifacts and every
# switch between them recompiled ~18 crates (30-80 s) even with nothing changed
# (7 apps, 3 revs, switched ~6 times: the 100-200 s "up to date" build). Apps
# are therefore grouped by the libcosmic rev in their Cargo.lock; the first app
# of a group (in ALL_APPS order) owns the group's target dir, the others symlink
# to it. Apps without libcosmic join cosmic-files' group.
ALL_APPS=(cosmic-files cosmic-edit cosmic-screenshot cosmic-store cosmic-player cosmic-monitor cosmic-initial-setup)
rev_of() { grep -m1 -A3 'name = "libcosmic"' "$SRC/$1/Cargo.lock" 2>/dev/null | grep -o 'libcosmic.git[^"]*#[0-9a-f]*' | head -1 || true; }
leader_of() {
  local r a; r=$(rev_of "$1")
  [[ -n "$r" ]] || { echo cosmic-files; return; }
  for a in "${ALL_APPS[@]}"; do [[ "$(rev_of "$a")" == "$r" ]] && { echo "$a"; return; }; done
  echo "$1"
}

# cosmic-store's reqwest pulls aws-lc-sys, whose C objects reference
# __stack_chk_guard; the sysroot libc.so does not export it (same gap as
# ports/mesa/ssp_guard.c), so link the same shim object in. Rustflags are part
# of cargo's metadata hash, so the whole target-dir group builds with them
# (a different flag set in one app would thrash the shared dir again).
extra_for() {
  [[ "$(leader_of "$1")" == cosmic-edit ]] || return 0
  # Stable, content-named, shared by all trees: per-tree paths would change the
  # rustflags (and so every crate's metadata hash) in each worktree.
  local c o h
  c="$(dirname "$0")/../mesa/ssp_guard.c"
  h=$(sha1sum "$c" | cut -c1-8)
  o="$M6/toolchain/ssp_guard-$h-$2.o"
  if [[ ! -f "$o" ]]; then
    local t; t=$(mktemp "$o.XXXXXX")
    "$M6/toolchain/$2-linux-musl-cc" -fno-stack-protector -c "$c" -o "$t" || { rm -f "$t"; return 1; }
    mv -f "$t" "$o"
  fi
  echo "--config=target.$2-unknown-linux-musl.rustflags=[\"-C\",\"link-arg=$o\"]"
}

cosmic_apps_cargo() { # <app> <arch> <feats> <extra>
  if ! "$M6/build-rust.sh" "$SRC/$1" "$2" $3 $4 > /dev/null; then
    echo "cosmic-apps: $1 build failed for $2 (log: $M6/logs/$1-$2.log)" >&2
    return 1
  fi
}

build_one() {
  local app="$1" lead arch
  if [[ ! -f "$SRC/$app/Cargo.toml" ]]; then
    echo "cosmic-apps: no source at $SRC/$app -- not built (git clone https://github.com/pop-os/$app, tag epoch-1.3.0)" >&2
    return 1
  fi
  # target dir shared per libcosmic-rev group (see leader_of)
  lead=$(leader_of "$app")
  if [[ "$app" != "$lead" ]]; then
    [[ "$(readlink "$SRC/$app/target" 2>/dev/null)" == "../$lead/target" ]] || { rm -f "$SRC/$app/target"; ln -s "../$lead/target" "$SRC/$app/target"; }
  elif [[ -L "$SRC/$app/target" ]]; then
    rm -f "$SRC/$app/target"   # was a symlink into another group's dir
  fi
  mkdir -p "$SRC/$lead/target"
  sh "$M6/gen-cargo-config.sh" "$SRC/$app" > /dev/null
  for arch in "${ARCHES[@]}"; do
    local feats extra; feats=$(feat_for "$app"); extra=$(extra_for "$app" "$arch")
    cosmic_cache_build "$app" "$arch" "$SRC/$app" "feats=$feats extra=$extra" "$(dirname "$0")/../mesa/ssp_guard.c" \
      "$SRC/$app/target/$arch-unknown-linux-musl/release/$app|$app-$arch" -- \
      cosmic_apps_cargo "$app" "$arch" "$feats" "$extra" || return 1
    echo "staged $arch: $M6_OUT/$app-$arch"
  done
}

# Groups (distinct target dirs) build concurrently, apps inside a group in order.
GROUPS_=()
for app in "${APPS[@]}"; do
  [[ -f "$SRC/$app/Cargo.toml" ]] || { build_one "$app"; exit 1; }
  l=$(leader_of "$app"); found=0
  for i in "${!GROUPS_[@]}"; do
    [[ "${GROUPS_[$i]%% *}" == "$l" ]] && { GROUPS_[$i]+=" $app"; found=1; }
  done
  [[ $found -eq 1 ]] || GROUPS_+=("$l $app")
done
pids=()
for g in "${GROUPS_[@]}"; do
  ( set -e; for app in ${g#* }; do build_one "$app"; done ) &
  pids+=($!)
done
rc=0
for p in "${pids[@]}"; do wait "$p" || rc=1; done
exit $rc
