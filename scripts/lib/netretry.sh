# shellcheck shell=sh
# Shared retry helpers for host-side network fetches (curl / wget / git).
#
# Why: the build hosts' resolver (Tailscale MagicDNS) occasionally drops
# replies under load, so a single fetch fails with "Could not resolve host"
# and then works minutes later.  These wrappers retry TRANSIENT failures only:
#   curl : exit 6 (DNS), 7 (connect), 28 (timeout), and with -f exit 22 when
#          the HTTP status was 429 or 5xx
#   wget : exit 4 (network failure)
#   git  : stderr shows a resolve / connect / timeout failure or HTTP 429/5xx
# Never retried: 404 and other 4xx, checksum failures (callers verify after
# the fetch), any other exit code.  The URL is never changed.
#
# Usage:  . scripts/lib/netretry.sh ; net_curl -fsSL -o out URL ; net_wget ... ; net_git clone ...
# Knobs:  NET_RETRY_MAX (attempts, default 5); NET_RETRY_SLEEP (command used
#         to wait, default "sleep", unit tests set "true").  Backoff 1,2,4,8 s.

NET_RETRY_MAX="${NET_RETRY_MAX:-5}"
NET_RETRY_SLEEP="${NET_RETRY_SLEEP:-sleep}"

# _net_backoff <what> <attempt> <rc> <delay-var-name>: log + wait; echoes nothing.
_net_wait() {
    printf 'netretry: %s failed (rc=%s, %s), retry %s/%s in %ss\n' \
        "$1" "$3" "$4" "$2" "$((NET_RETRY_MAX - 1))" "$5" >&2
    $NET_RETRY_SLEEP "$5"
}

net_curl() {
    _nc_try=1; _nc_delay=1
    _nc_hdr=$(mktemp "${TMPDIR:-/tmp}/netretry.XXXXXX") || { curl "$@"; return $?; }
    while :; do
        : > "$_nc_hdr"
        curl -D "$_nc_hdr" "$@" && _nc_rc=0 || _nc_rc=$?
        if [ "$_nc_rc" -eq 0 ]; then rm -f "$_nc_hdr"; return 0; fi
        _nc_why=""
        case "$_nc_rc" in
            6) _nc_why="DNS resolution" ;;
            7) _nc_why="connect failure" ;;
            28) _nc_why="timeout" ;;
            22)
                _nc_code=$(awk '/^HTTP\//{c=$2} END{print c}' "$_nc_hdr" 2>/dev/null)
                case "$_nc_code" in
                    429|5[0-9][0-9]) _nc_why="HTTP $_nc_code" ;;
                esac ;;
        esac
        if [ -z "$_nc_why" ] || [ "$_nc_try" -ge "$NET_RETRY_MAX" ]; then
            rm -f "$_nc_hdr"; return "$_nc_rc"
        fi
        _net_wait "curl" "$_nc_try" "$_nc_rc" "$_nc_why" "$_nc_delay"
        _nc_try=$((_nc_try + 1)); _nc_delay=$((_nc_delay * 2))
    done
}

net_wget() {
    _nw_try=1; _nw_delay=1
    while :; do
        wget "$@" && _nw_rc=0 || _nw_rc=$?
        if [ "$_nw_rc" -eq 0 ]; then return 0; fi
        if [ "$_nw_rc" -ne 4 ] || [ "$_nw_try" -ge "$NET_RETRY_MAX" ]; then return "$_nw_rc"; fi
        _net_wait "wget" "$_nw_try" "$_nw_rc" "network failure" "$_nw_delay"
        _nw_try=$((_nw_try + 1)); _nw_delay=$((_nw_delay * 2))
    done
}

net_git() {
    _ng_try=1; _ng_delay=1
    _ng_err=$(mktemp "${TMPDIR:-/tmp}/netretry.XXXXXX") || { git "$@"; return $?; }
    while :; do
        git "$@" 2> "$_ng_err" && _ng_rc=0 || _ng_rc=$?
        cat "$_ng_err" >&2
        if [ "$_ng_rc" -eq 0 ]; then rm -f "$_ng_err"; return 0; fi
        if [ "$_ng_try" -ge "$NET_RETRY_MAX" ] || ! grep -qiE \
            'Could not resolve host|Temporary failure in name resolution|Failed to connect|Connection timed out|Operation timed out|Connection refused|error: (429|5[0-9][0-9])' "$_ng_err"; then
            rm -f "$_ng_err"; return "$_ng_rc"
        fi
        _net_wait "git $1" "$_ng_try" "$_ng_rc" "transient network error" "$_ng_delay"
        _ng_try=$((_ng_try + 1)); _ng_delay=$((_ng_delay * 2))
    done
}

# net_apk / net_pip: same policy for package managers run INSIDE the Alpine
# build containers (busybox ash).  Retried only when stderr shows a transient
# network error; unsatisfiable constraints / missing packages / bad signatures
# fail at once.  stdout passes through untouched, stderr is replayed.
_NET_APK_RE='temporary error|DNS[: ]|DNS lookup|network error|Network is unreachable|Could not resolve|Temporary failure in name resolution|Connection (timed out|refused|reset)|timed out|Operation timed out|Failed to connect|(HTTP|HTTPS)? ?(error )?(429|5[0-9][0-9])( |$)|Bad Gateway|Service Unavailable|Gateway Time-?out'
_NET_PIP_RE='Failed to establish a new connection|Temporary failure in name resolution|Name or service not known|Connection (timed out|refused|reset|aborted)|ReadTimeoutError|ConnectTimeoutError|Max retries exceeded|Network is unreachable|(HTTP|Server) (Error|error)?:? ?(429|5[0-9][0-9])|Bad Gateway|Service Unavailable'

_net_pkg() { # <name> <regex> cmd...
    _np_name=$1; _np_re=$2; shift 2
    _np_try=1; _np_delay=1
    _np_err=$(mktemp "${TMPDIR:-/tmp}/netretry.XXXXXX") || { "$@"; return $?; }
    while :; do
        "$@" 2> "$_np_err" && _np_rc=0 || _np_rc=$?
        cat "$_np_err" >&2
        if [ "$_np_rc" -eq 0 ]; then rm -f "$_np_err"; return 0; fi
        if [ "$_np_try" -ge "$NET_RETRY_MAX" ] || ! grep -qiE "$_np_re" "$_np_err"; then
            rm -f "$_np_err"; return "$_np_rc"
        fi
        _net_wait "$_np_name" "$_np_try" "$_np_rc" "transient network error" "$_np_delay"
        _np_try=$((_np_try + 1)); _np_delay=$((_np_delay * 2))
    done
}
# net_apk: with the shared package cache mounted (leandros_alpine_run mounts the
# host's $LEANDROS_PKGCACHE/apk/<release> at /apkcache) the install-type
# commands (add/upgrade/fix/fetch/update) use it instead of --no-cache:
#   --cache-dir /apkcache/<arch>  one dir per target arch (--arch A, else uname -m)
#   --cache-max-age N             APKINDEX reused for N s (LEANDROS_APK_MAXAGE, 86400)
# Packages are stored under their content hash and checked against the repo index
# and its signature exactly as without the cache, so a hit installs the same bytes
# a download would. When the network is down (or LEANDROS_OFFLINE=1) and the cache
# holds an index, the command is re-run once with --no-network on the cache alone;
# a package that is not cached then fails with apk's own "no such package" error.
_net_apk_args() { # sets _na_cmd (the apk subcommand) and _na_arch, rewrites "$@" into _na_args
  _na_cmd="" _na_arch=$(uname -m) _na_cache=0
  for _na_a in "$@"; do
    case "$_na_cmd:$_na_a" in
      :add|:upgrade|:fix|:fetch|:update) _na_cmd=$_na_a ;;
      :--arch|:--root) ;;
    esac
  done
  _na_prev=""
  for _na_a in "$@"; do
    [ "$_na_prev" = --arch ] && _na_arch=$_na_a
    _na_prev=$_na_a
  done
}
_net_apk_cached() {
  [ -d "${LEANDROS_APKCACHE_DIR:-/apkcache}" ] && [ -w "${LEANDROS_APKCACHE_DIR:-/apkcache}" ] && [ "${LEANDROS_PKGCACHE_OFF:-0}" != 1 ] || return 1
  _net_apk_args "$@"
  case "$_na_cmd" in add|upgrade|fix|fetch|update) ;; *) return 1 ;; esac
  _na_dir=${LEANDROS_APKCACHE_DIR:-/apkcache}/$_na_arch
  mkdir -p "$_na_dir" 2>/dev/null || return 1
  return 0
}
net_apk() {
  if _net_apk_cached "$@"; then
    _na_n=$#
    while [ "$_na_n" -gt 0 ]; do
      _na_x=$1; shift; _na_n=$((_na_n - 1))
      [ "$_na_x" = --no-cache ] || set -- "$@" "$_na_x"
    done
    set -- --cache-dir "$_na_dir" --cache-max-age "${LEANDROS_APK_MAXAGE:-86400}" "$@"
    if [ "${LEANDROS_OFFLINE:-0}" = 1 ]; then
      echo "netretry: LEANDROS_OFFLINE=1, apk from the cache $_na_dir only" >&2
      command apk --no-network "$@"; return $?
    fi
    _net_pkg apk "$_NET_APK_RE" _net_apk_try "$@"; return $?
  fi
  _net_pkg apk "$_NET_APK_RE" command apk "$@"
}
# one attempt; on a network failure with a cached index, fall back to the cache alone
_net_apk_try() {
  _nt_err=$(mktemp "${TMPDIR:-/tmp}/netretry.XXXXXX" 2>/dev/null) || { command apk "$@"; return $?; }
  command apk "$@" 2> "$_nt_err" && _nt_rc=0 || _nt_rc=$?
  if [ "$_nt_rc" -ne 0 ] && grep -qiE "$_NET_APK_RE" "$_nt_err" \
     && ls "$_na_dir"/APKINDEX.*.tar.gz >/dev/null 2>&1; then
    echo "netretry: apk network failure; retrying from the cache $_na_dir alone (--no-network)" >&2
    if command apk --no-network "$@" 2>> "$_nt_err"; then cat "$_nt_err" >&2; rm -f "$_nt_err"; return 0; fi
  fi
  cat "$_nt_err" >&2; rm -f "$_nt_err"; return "$_nt_rc"
}
net_pip() { _net_pkg pip "$_NET_PIP_RE" command pip "$@"; }
# net_shim: shadow apk/pip with the retrying versions for the rest of this
# shell (container scripts: `. /netretry/netretry.sh; net_shim`).  Also covers
# $APK="apk --root ..." since functions win over the binary.
net_shim() { apk() { net_apk "$@"; }; pip() { net_pip "$@"; }; }

# net_fetch_verified URL OUT SHA256: OUT = the file at URL, whose sha256 must be
# SHA256 (pinned in the port). Reused without any network from OUT itself when it
# already verifies, else from the shared distfile cache (/distfiles, mounted by
# leandros_alpine_run, content-addressed by the sha) -- never trusting a file
# whose hash differs: a corrupt or tampered copy is deleted and fetched again,
# and a download that does not verify fails (rc 1) with the two hashes printed.
_net_sha() { sha256sum "$1" 2>/dev/null | cut -d' ' -f1; }
net_fetch_verified() {
  _fv_url=$1 _fv_out=$2 _fv_sha=$3
  if [ -s "$_fv_out" ] && [ "$(_net_sha "$_fv_out")" = "$_fv_sha" ]; then
    echo "netretry: $_fv_out already verified (sha256 $_fv_sha)" >&2; return 0
  fi
  rm -f "$_fv_out"
  _fv_d=${LEANDROS_DISTFILES_DIR:-/distfiles}
  _fv_c=$_fv_d/$_fv_sha
  if [ -d "$_fv_d" ] && [ -s "$_fv_c" ] && [ "$(_net_sha "$_fv_c")" = "$_fv_sha" ]; then
    _fv_part=$(mktemp "$_fv_out.XXXXXX") && cp "$_fv_c" "$_fv_part" && chmod 644 "$_fv_part" && mv "$_fv_part" "$_fv_out"
    echo "netretry: $_fv_url from the distfile cache (sha256 $_fv_sha)" >&2; return 0
  fi
  [ -d "$_fv_d" ] && rm -f "$_fv_c"
  if [ "${LEANDROS_OFFLINE:-0}" = 1 ]; then
    echo "ERROR: $_fv_url is not in the distfile cache and LEANDROS_OFFLINE=1" >&2; return 1
  fi
  _fv_part=$(mktemp "$_fv_out.XXXXXX") || return 1
  net_wget -q -O "$_fv_part" "$_fv_url" || {
    rm -f "$_fv_part"
    echo "ERROR: cannot download $_fv_url (no network, and sha256 $_fv_sha is not cached)" >&2; return 1; }
  _fv_got=$(_net_sha "$_fv_part")
  if [ "$_fv_got" != "$_fv_sha" ]; then
    rm -f "$_fv_part"
    echo "ERROR: $_fv_url: sha256 mismatch: want $_fv_sha got $_fv_got" >&2; return 1
  fi
  chmod 644 "$_fv_part"; mv "$_fv_part" "$_fv_out"
  if [ -d "$_fv_d" ] && [ -w "$_fv_d" ]; then
    cp "$_fv_out" "$_fv_d/.$_fv_sha.$$" && mv -f "$_fv_d/.$_fv_sha.$$" "$_fv_c"
  fi
  return 0
}
