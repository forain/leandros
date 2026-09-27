# Source me (bash). On macOS, make sure host C links work before any cargo
# build script or `cc` runs.
#
# A Command Line Tools update can leave the default SDK (xcrun --show-sdk-path)
# newer than the installed linker understands, e.g. MacOSX27.0.sdk with
# `ld: tapi error: malformed file ... libSystem.B.tbd: unknown architecture
# arm64e.x1-macos`. Every host link then fails: busd's and greetd's build
# scripts, proc-macros, the mkfs ports. If SDKROOT is unset and the default SDK
# cannot link a trivial C program, pick the newest CLT SDK that can and export
# it. An SDKROOT set by the caller is never touched. No-op off macOS.

leandros_pick_darwin_sdk() {
    [ "$(uname -s)" = "Darwin" ] || return 0
    [ -n "${SDKROOT:-}" ] && return 0
    [ -n "${LEANDROS_SDK_CHECKED:-}" ] && return 0
    export LEANDROS_SDK_CHECKED=1

    local tmp default s
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/leandros-sdk.XXXXXX")" || return 0
    _leandros_sdk_links() {
        printf 'int main(void){return 0;}\n' |
            SDKROOT="$1" cc -x c - -o "$tmp/a.out" >/dev/null 2>&1
    }
    default="$(xcrun --show-sdk-path 2>/dev/null)"
    if [ -n "$default" ] && _leandros_sdk_links "$default"; then
        rm -rf "$tmp"
        return 0
    fi
    for s in $(ls -d /Library/Developer/CommandLineTools/SDKs/MacOSX[0-9]*.sdk 2>/dev/null | sort -t X -k 3 -V -r); do
        [ "$s" = "$default" ] && continue
        if _leandros_sdk_links "$s"; then
            export SDKROOT="$(cd "$s" && pwd -P)"
            echo "🍎 SDKROOT=$SDKROOT (default SDK ${default:-<none>} cannot link a C program; newest CLT SDK that can)"
            rm -rf "$tmp"
            return 0
        fi
    done
    rm -rf "$tmp"
    echo "⚠️  default SDK ${default:-<none>} cannot link a C program and no CLT SDK under"
    echo "⚠️  /Library/Developer/CommandLineTools/SDKs can either; host builds will fail."
    echo "⚠️  Set SDKROOT to a working SDK, or reinstall the Command Line Tools."
}
leandros_pick_darwin_sdk
