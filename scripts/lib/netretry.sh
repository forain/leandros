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
        curl -D "$_nc_hdr" "$@"
        _nc_rc=$?
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
        wget "$@"; _nw_rc=$?
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
        git "$@" 2> "$_ng_err"; _ng_rc=$?
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
        "$@" 2> "$_np_err"; _np_rc=$?
        cat "$_np_err" >&2
        if [ "$_np_rc" -eq 0 ]; then rm -f "$_np_err"; return 0; fi
        if [ "$_np_try" -ge "$NET_RETRY_MAX" ] || ! grep -qiE "$_np_re" "$_np_err"; then
            rm -f "$_np_err"; return "$_np_rc"
        fi
        _net_wait "$_np_name" "$_np_try" "$_np_rc" "transient network error" "$_np_delay"
        _np_try=$((_np_try + 1)); _np_delay=$((_np_delay * 2))
    done
}
net_apk() { _net_pkg apk "$_NET_APK_RE" command apk "$@"; }
net_pip() { _net_pkg pip "$_NET_PIP_RE" command pip "$@"; }
# net_shim: shadow apk/pip with the retrying versions for the rest of this
# shell (container scripts: `. /netretry/netretry.sh; net_shim`).  Also covers
# $APK="apk --root ..." since functions win over the binary.
net_shim() { apk() { net_apk "$@"; }; pip() { net_pip "$@"; }; }
