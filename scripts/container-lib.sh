# container-lib.sh — sourced (POSIX sh) by scripts/build-all.sh and the
# Alpine-staged ports (ports/firefox, ports/portal, ports/pipewire, ports/mesa,
# ports/nvk-tests).
#
# Ports do not call podman/docker directly any more: they run their Alpine
# step through leandros_alpine_run (bottom of this file), which has two
# backends -- a container on the host, and a chroot'ed Alpine root staged with
# apk.static when the build runs inside LeandrOS itself.
#
# Why: `docker info` against a wedged Docker Desktop never returns, and every
# port used it as its "is the daemon up?" probe, so one hung daemon hung the
# whole build-all forever. Everything here is bounded.
#
#   leandros_pick_container   sets CT to podman or docker — the first one that
#                             answers `info` within LEANDROS_CT_TIMEOUT seconds
#                             (default 20), podman first. LEANDROS_CONTAINER=
#                             podman|docker skips the probe and uses that tool;
#                             LEANDROS_CONTAINER=none means "no container".
#                             Returns 1 (CT empty) when nothing answers; the
#                             reason is in CT_WHY. The verdict is cached in
#                             LEANDROS_CONTAINER (exported) so later ports in
#                             the same build-all run do not probe again.
#   leandros_port_reuse PORTDIR ARCH NAME
#                             makes PORTDIR/out/ARCH usable without a container:
#                             keeps a complete (.stamp'ed) tree that is already
#                             there, even if stale, else copies the newest packaged tree
#                             from (1) the main checkout of this repository
#                             (worktrees share it) or (2) $LEANDROS_ARTIFACTS/
#                             NAME-out/ARCH. Only trees with a .stamp count.
#                             The copy keeps its old .stamp mtime, so the next
#                             build still sees the port as stale and retries the
#                             container when one is reachable.

# Run "$@" with a wall-clock bound of $1 seconds. 124 on timeout (like
# timeout(1), which macOS lacks by default). Output goes where the caller sends it.
leandros_bounded() {
  _lb_secs=$1; shift
  "$@" &
  _lb_pid=$!
  _lb_ticks=0
  while kill -0 "$_lb_pid" 2>/dev/null; do
    if [ "$_lb_ticks" -ge $((_lb_secs * 10)) ]; then
      kill "$_lb_pid" 2>/dev/null
      sleep 1
      kill -9 "$_lb_pid" 2>/dev/null
      wait "$_lb_pid" 2>/dev/null
      return 124
    fi
    sleep 0.1
    _lb_ticks=$((_lb_ticks + 1))
  done
  wait "$_lb_pid"
}

leandros_pick_container() {
  CT=""
  CT_WHY=""
  case "${LEANDROS_CONTAINER:-}" in
    none) CT_WHY="LEANDROS_CONTAINER=none"; return 1 ;;
    podman|docker)
      if command -v "$LEANDROS_CONTAINER" >/dev/null 2>&1; then
        CT=$LEANDROS_CONTAINER; return 0
      fi
      CT_WHY="LEANDROS_CONTAINER=$LEANDROS_CONTAINER is not installed"; return 1 ;;
  esac
  _pc_t=${LEANDROS_CT_TIMEOUT:-20}
  for _pc_tool in podman docker; do
    command -v "$_pc_tool" >/dev/null 2>&1 || continue
    _pc_rc=0
    leandros_bounded "$_pc_t" "$_pc_tool" info >/dev/null 2>&1 || _pc_rc=$?
    if [ "$_pc_rc" = 0 ]; then
      CT=$_pc_tool
      LEANDROS_CONTAINER=$CT; export LEANDROS_CONTAINER
      return 0
    fi
    if [ "$_pc_rc" = 124 ]; then
      CT_WHY="${CT_WHY}${CT_WHY:+; }'$_pc_tool info' did not answer within ${_pc_t} s (daemon hung?)"
    else
      CT_WHY="${CT_WHY}${CT_WHY:+; }'$_pc_tool info' failed (rc $_pc_rc: daemon/machine not running)"
    fi
  done
  [ -n "$CT_WHY" ] || CT_WHY="neither podman nor docker is installed"
  LEANDROS_CONTAINER=none; export LEANDROS_CONTAINER
  return 1
}

leandros_port_reuse() {
  _pr_dir=$1 _pr_arch=$2 _pr_name=$3
  _pr_dst="$_pr_dir/out/$_pr_arch"
  if [ -f "$_pr_dst/.stamp" ]; then
    echo "  reusing the previously staged $_pr_dst (may be older than the port sources)"
    return 0
  fi
  _pr_rel=${_pr_dir#"$(git -C "$_pr_dir" rev-parse --show-toplevel 2>/dev/null)/"}
  _pr_main=""
  _pr_common=$(git -C "$_pr_dir" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) \
    && _pr_main=$(dirname "$_pr_common")
  for _pr_src in ${_pr_main:+"$_pr_main/$_pr_rel/out/$_pr_arch"} \
                 "${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}/$_pr_name-out/$_pr_arch"; do
    [ -f "$_pr_src/.stamp" ] || continue
    [ "$(cd "$_pr_src" && pwd -P)" = "$(mkdir -p "$_pr_dst" && cd "$_pr_dst" && pwd -P)" ] && continue
    echo "  copying the packaged $_pr_name tree from $_pr_src"
    rm -rf "$_pr_dst.reuse"
    if cp -a "$_pr_src" "$_pr_dst.reuse"; then
      rm -rf "$_pr_dst" && mv "$_pr_dst.reuse" "$_pr_dst"
      return 0
    fi
    rm -rf "$_pr_dst.reuse"
  done
  rmdir "$_pr_dst" 2>/dev/null
  return 1
}

# ── Alpine 3.21, with or without a container ────────────────────────────────
#
# One abstraction, two backends:
#
#   container  the host build (macOS / Linux): podman or docker runs
#              alpine:3.21 of the requested architecture, a foreign arch under
#              binfmt emulation. Exactly what the ports always did; the tool is
#              picked by leandros_pick_container above.
#   local      the build runs INSIDE LeandrOS, where there are no containers:
#              an Alpine 3.21 root is staged with the image's static apk-tools
#              (/usr/sbin/apk.static via /usr/sbin/alpine-sysroot, both from
#              ports/apk) under $LEANDROS_ALPINE_DIR, and the command runs
#              chroot'ed into a throw-away copy of it -- the same "fresh
#              alpine:3.21, --rm" contract as the container. Native arch only:
#              there is no qemu-user in the guest, so asking for the other
#              architecture is a clear error, not an attempt.
#
#   leandros_in_guest        0 when this shell runs on LeandrOS: the kernel's
#                            uname -s is "Leandros" (Linux says "Linux"), and
#                            /etc/os-release ID=leandros as a second opinion.
#   leandros_pick_alpine     sets ALPINE_BACKEND (container|local) and
#                            ALPINE_TOOL (podman, docker or the apk.static
#                            path, for log lines); returns 1 with the reason in
#                            ALPINE_WHY. LEANDROS_ALPINE_BACKEND=container|local
#                            overrides the detection; the container side keeps
#                            honouring LEANDROS_CONTAINER.
#   leandros_alpine_run [--arch A] [--image I] [-e K=V]... [-v HOST:DIR[:ro]]... -- CMD...
#                            run CMD in Alpine 3.21 and return its status.
#                            --arch omitted = this machine's arch. --image
#                            (container only) replaces alpine:3.21. Mounts: the
#                            container bind-mounts. LeandrOS has no bind mounts,
#                            so the local backend COPIES each HOST dir into the
#                            root first and, for the writable ones, copies DIR
#                            back onto HOST afterwards (new and changed files;
#                            deletions inside the run are not propagated).
#
# Local-backend knobs: LEANDROS_ALPINE_DIR (default /var/lib/leandros-alpine;
# keep it short, the kernel's path limit is 255 bytes INCLUDING this prefix),
# LEANDROS_ALPINE_KEEP=1 keeps the per-run root for inspection, and
# alpine-sysroot's own ALPINE_REPO.

leandros_in_guest() {
  [ "$(uname -s 2>/dev/null)" = Leandros ] && return 0
  grep -qsE '^ID="?leandros"?$' /etc/os-release
}

# What the alpine:3.21 image is made of; the local base root gets the same set
# so a script written for the container sees the same userland.
LEANDROS_ALPINE_BASE_PKGS="alpine-baselayout alpine-baselayout-data alpine-keys
  alpine-release apk-tools busybox busybox-binsh ca-certificates-bundle
  musl-utils scanelf ssl_client"

leandros_pick_alpine() {
  ALPINE_BACKEND="" ALPINE_TOOL="" ALPINE_WHY=""
  _pa_mode=${LEANDROS_ALPINE_BACKEND:-auto}
  if [ "$_pa_mode" = auto ]; then
    if leandros_in_guest; then _pa_mode=local; else _pa_mode=container; fi
  fi
  case "$_pa_mode" in
    container)
      if leandros_pick_container; then
        ALPINE_BACKEND=container ALPINE_TOOL=$CT; return 0
      fi
      ALPINE_WHY=$CT_WHY; return 1 ;;
    local)
      _pa_apk=""
      for _pa_c in "${APK:-}" /usr/sbin/apk.static /sbin/apk.static; do
        [ -n "$_pa_c" ] && [ -x "$_pa_c" ] && { _pa_apk=$_pa_c; break; }
      done
      if [ -z "$_pa_apk" ]; then
        ALPINE_WHY="no apk.static (the image ships /usr/sbin/apk.static, ports/apk)"; return 1
      fi
      LEANDROS_ALPINE_SYSROOT=$(command -v alpine-sysroot 2>/dev/null || echo /usr/sbin/alpine-sysroot)
      if [ ! -x "$LEANDROS_ALPINE_SYSROOT" ]; then
        ALPINE_WHY="no alpine-sysroot (ports/apk/alpine-sysroot.sh)"; return 1
      fi
      if ! command -v chroot >/dev/null 2>&1; then
        ALPINE_WHY="no chroot command"; return 1
      fi
      if [ "$(id -u)" != 0 ]; then
        ALPINE_WHY="the local Alpine backend needs root (chroot(2) is euid-0 only)"; return 1
      fi
      ALPINE_BACKEND=local ALPINE_TOOL=$_pa_apk; return 0 ;;
    *) ALPINE_WHY="LEANDROS_ALPINE_BACKEND=$_pa_mode (want container|local)"; return 1 ;;
  esac
}

# Single-quote one word so it survives eval.
_leandros_q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

leandros_alpine_run() {
  _ar_arch="" _ar_image="alpine:3.21" _ar_env="" _ar_cenv="" _ar_cmnt="" _ar_mlist=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --arch)  _ar_arch=$2; shift 2 ;;
      --image) _ar_image=$2; shift 2 ;;
      -e)      _ar_env="$_ar_env $(_leandros_q "$2")"
               _ar_cenv="$_ar_cenv -e $(_leandros_q "$2")"; shift 2 ;;
      -v)      _ar_cmnt="$_ar_cmnt -v $(_leandros_q "$2")"
               _ar_mlist="$_ar_mlist$2
"; shift 2 ;;
      --)      shift; break ;;
      *) echo "leandros_alpine_run: unknown option $1" >&2; return 2 ;;
    esac
  done
  [ $# -gt 0 ] || { echo "leandros_alpine_run: no command" >&2; return 2; }
  if [ -z "${ALPINE_BACKEND:-}" ]; then
    leandros_pick_alpine || { echo "no Alpine backend: $ALPINE_WHY" >&2; return 1; }
  fi
  if [ "$ALPINE_BACKEND" = container ]; then
    case "$_ar_arch" in
      aarch64) _ar_plat="--platform linux/arm64" ;;
      x86_64)  _ar_plat="--platform linux/amd64" ;;
      linux/*) _ar_plat="--platform $_ar_arch" ;;
      "")      _ar_plat="" ;;
      *) echo "leandros_alpine_run: unknown arch $_ar_arch" >&2; return 2 ;;
    esac
    eval "\"\$CT\" run --rm $_ar_plat $_ar_cenv $_ar_cmnt $(_leandros_q "$_ar_image") \"\$@\""
    return
  fi
  _leandros_alpine_run_local "$@"
}

_leandros_alpine_run_local() {
  _rl_native=$(uname -m)
  case "$_ar_arch" in
    ""|"$_rl_native") _rl_ok=1 ;;
    linux/arm64) if [ "$_rl_native" = aarch64 ]; then _rl_ok=1; else _rl_ok=0; fi ;;
    linux/amd64) if [ "$_rl_native" = x86_64 ]; then _rl_ok=1; else _rl_ok=0; fi ;;
    *) _rl_ok=0 ;;
  esac
  if [ "$_rl_ok" = 0 ]; then
    echo "❌ in-guest Alpine builds are native only: wanted $_ar_arch, this machine is $_rl_native" >&2
    echo "   (there is no qemu-user inside LeandrOS: build $_ar_arch on a $_ar_arch guest or on the host)" >&2
    return 3
  fi
  [ "$_ar_image" = alpine:3.21 ] || echo "note: the local Alpine backend ignores --image $_ar_image" >&2
  _rl_dir=${LEANDROS_ALPINE_DIR:-/var/lib/leandros-alpine}
  _rl_base="$_rl_dir/$_rl_native-base"
  if [ ! -f "$_rl_base/lib/apk/db/installed" ]; then
    echo "staging the Alpine 3.21 base root $_rl_base" >&2
    # shellcheck disable=SC2086
    APK="$ALPINE_TOOL" "$LEANDROS_ALPINE_SYSROOT" "$_rl_base" $LEANDROS_ALPINE_BASE_PKGS >&2 \
      || { echo "❌ could not stage $_rl_base" >&2; return 1; }
  fi
  _rl_run="$_rl_dir/run.$$"
  rm -rf "$_rl_run"
  mkdir -p "$_rl_run"
  cp -a "$_rl_base/." "$_rl_run/" || { echo "❌ cannot copy $_rl_base" >&2; return 1; }
  cp /etc/resolv.conf "$_rl_run/etc/resolv.conf" 2>/dev/null || true
  mkdir -p "$_rl_run/tmp" "$_rl_run/proc" "$_rl_run/dev" "$_rl_run/root"
  chmod 1777 "$_rl_run/tmp"
  _rl_ifs=$IFS
  IFS='
'
  _rl_back=""
  for _rl_v in $_ar_mlist; do
    _rl_h=${_rl_v%%:*} _rl_rest=${_rl_v#*:}
    _rl_c=${_rl_rest%%:*}
    mkdir -p "$_rl_run$_rl_c"
    if [ -d "$_rl_h" ]; then
      cp -a "$_rl_h/." "$_rl_run$_rl_c/" \
        || { IFS=$_rl_ifs; echo "❌ cannot copy $_rl_h into the root" >&2; return 1; }
    fi
    case "$_rl_rest" in *:ro) ;; *) _rl_back="$_rl_back$_rl_h:$_rl_c
" ;; esac
  done
  IFS=$_rl_ifs
  # The container contract: a clean environment plus the -e pairs, cwd /.
  _rl_rc=0
  eval "chroot $(_leandros_q "$_rl_run") /usr/bin/env -i \
    PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    HOME=/root TERM=dumb $_ar_env \"\$@\"" || _rl_rc=$?
  IFS='
'
  for _rl_b in $_rl_back; do
    _rl_h=${_rl_b%%:*} _rl_c=${_rl_b#*:}
    mkdir -p "$_rl_h"
    cp -a "$_rl_run$_rl_c/." "$_rl_h/" \
      || { echo "⚠️  copy-back of $_rl_c to $_rl_h failed" >&2; [ "$_rl_rc" != 0 ] || _rl_rc=1; }
  done
  IFS=$_rl_ifs
  if [ "${LEANDROS_ALPINE_KEEP:-0}" = 1 ]; then
    echo "kept the run root $_rl_run" >&2
  else
    rm -rf "$_rl_run"
  fi
  return $_rl_rc
}
