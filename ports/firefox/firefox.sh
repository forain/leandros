#!/bin/sh
# /bin/firefox — launch Alpine's Firefox (ports/firefox) inside a LeandrOS
# COSMIC session, e.g. from cosmic-term. POSIX sh: /bin/sh is brush.
#
# Thin launcher: Firefox's own defaults decide WebRender, Wayland, VA-API and the
# process model (distro prefs: /usr/lib/firefox/defaults/pref/leandros-*.js).
# RULE: GPU rendering only. Without a DRM render node WebRender would fall back
# to its software rasteriser, so this refuses to start instead.
# LEANDROS_FIREFOX_ALLOW_SOFTWARE=1 overrides, for debugging only.
if [ ! -e /dev/dri/renderD128 ]; then
    if [ "${LEANDROS_FIREFOX_ALLOW_SOFTWARE:-0}" != 1 ]; then
        echo "firefox: no GPU render node (/dev/dri/renderD128); refusing to start" >&2
        exit 78
    fi
    echo "firefox: WARNING running without a GPU render node (LEANDROS_FIREFOX_ALLOW_SOFTWARE=1)" >&2
fi

# Sandboxes are ON by default (seccomp-bpf, user/pid/net/ipc namespaces, chroot).
# LEANDROS_FIREFOX_SANDBOX=0 disables every process type's sandbox (debug only).
# LEANDROS_FIREFOX_SANDBOX_LOG=1 turns on Firefox's own "Sandbox:" logging.
if [ "${LEANDROS_FIREFOX_SANDBOX:-1}" = 0 ]; then
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
fi
if [ "${LEANDROS_FIREFOX_SANDBOX_LOG:-0}" = 1 ]; then
    MOZ_SANDBOX_LOGGING=1
    export MOZ_SANDBOX_LOGGING
fi

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
