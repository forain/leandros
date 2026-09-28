#!/bin/sh
# /bin/firefox — launch Alpine's Firefox (ports/firefox) inside a LeandrOS
# COSMIC session, e.g. from cosmic-term. POSIX sh: /bin/sh is brush.
#
# RULE: GPU rendering only. /bin/gpu-env (already sourced by the session, so
# this is normally a no-op) decides zink or virgl; without one of them this
# refuses to start instead of letting WebRender fall back to its software
# rasteriser. LEANDROS_FIREFOX_ALLOW_SOFTWARE=1 overrides, for debugging only.

if [ -r /bin/gpu-env ]; then
    . /bin/gpu-env
fi
case "${LEANDROS_RENDERER_PATH:-}" in
zink|virgl) ;;
*)
    if [ "${LEANDROS_FIREFOX_ALLOW_SOFTWARE:-0}" != 1 ]; then
        echo "firefox: no GPU renderer (LEANDROS_RENDERER_PATH=${LEANDROS_RENDERER_PATH:-unset}); refusing to start" >&2
        exit 78
    fi
    echo "firefox: WARNING running without a GPU renderer (LEANDROS_FIREFOX_ALLOW_SOFTWARE=1)" >&2
    ;;
esac

# Native Wayland, through GTK3's Wayland backend.
MOZ_ENABLE_WAYLAND=1
GDK_BACKEND=wayland
export MOZ_ENABLE_WAYLAND GDK_BACKEND

# Hardware compositing: WebRender on EGL (our Mesa: zink or virgl). The prefs
# in /usr/lib/firefox/defaults/pref/leandros-prefs.js force it past the GPU
# blocklist and forbid the software-WebRender fallback.
MOZ_ACCELERATED=1
MOZ_WEBRENDER=1
export MOZ_ACCELERATED MOZ_WEBRENDER

# The kernel has no seccomp-bpf and no user/pid namespaces, which every
# Firefox sandbox is built on. Turn each process type's sandbox off rather
# than let it fail half-way.
MOZ_DISABLE_CONTENT_SANDBOX=1
MOZ_DISABLE_GMP_SANDBOX=1
MOZ_DISABLE_RDD_SANDBOX=1
MOZ_DISABLE_SOCKET_PROCESS_SANDBOX=1
MOZ_DISABLE_UTILITY_SANDBOX=1
MOZ_DISABLE_GPU_SANDBOX=1
MOZ_DISABLE_VR_SANDBOX=1
export MOZ_DISABLE_CONTENT_SANDBOX MOZ_DISABLE_GMP_SANDBOX MOZ_DISABLE_RDD_SANDBOX \
       MOZ_DISABLE_SOCKET_PROCESS_SANDBOX MOZ_DISABLE_UTILITY_SANDBOX \
       MOZ_DISABLE_GPU_SANDBOX MOZ_DISABLE_VR_SANDBOX

# No crash reporter, no accessibility bus, no dconf, no portals: none of those
# services exist here, and each one is a D-Bus round trip that can only fail.
MOZ_CRASHREPORTER_DISABLE=1
NO_AT_BRIDGE=1
GTK_A11Y=none
GSETTINGS_BACKEND=memory
GTK_USE_PORTAL=0
GIO_USE_VFS=local
export MOZ_CRASHREPORTER_DISABLE NO_AT_BRIDGE GTK_A11Y GSETTINGS_BACKEND GTK_USE_PORTAL GIO_USE_VFS

# LEANDROS_FIREFOX_ICON_TRACE=1: log every icon name GTK/Firefox look up
# ("ICONTRACE ..." on stderr) — how ports/firefox/icons.txt is maintained.
if [ "${LEANDROS_FIREFOX_ICON_TRACE:-0}" = 1 ] && [ -r /usr/lib/firefox/libleandros-icontrace.so ]; then
    LD_PRELOAD=/usr/lib/firefox/libleandros-icontrace.so${LD_PRELOAD:+:$LD_PRELOAD}
    export LD_PRELOAD
fi

exec /usr/lib/firefox/firefox "$@"
