# container-lib.sh — sourced (POSIX sh) by scripts/build-all.sh and the
# container-staged ports (ports/firefox, ports/portal, ports/pipewire).
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
