#!/bin/sh
# M7h desktop bringup: busd + cosmic-comp (+ wallpaper/panel as wayland clients).
set -u
export HOME=/root
export XDG_RUNTIME_DIR=/run/user/0
export XDG_CONFIG_HOME=/root/.config
export XDG_CACHE_HOME=/root/.cache
export XDG_DATA_HOME=/root/.local/share
export XDG_DATA_DIRS=/usr/local/share:/usr/share
export XDG_CURRENT_DESKTOP=COSMIC
export XDG_SESSION_TYPE=wayland
export COSMIC_BACKEND=kms
export COSMIC_RENDER_DEVICE=226:0
export GBM_ALWAYS_SOFTWARE=1
export SMITHAY_USE_LEGACY=1
export COSMIC_DISABLE_SYNCOBJ=1
export COSMIC_DISABLE_DIRECT_SCANOUT=1
export ICED_BACKEND=tiny-skia
export XCURSOR_THEME=default
export XCURSOR_SIZE=24
export RUST_BACKTRACE=1
mkdir -p /run/user/0
chmod 0700 /run/user/0
rm -f /run/user/0/bus /run/user/0/wayland-1 /run/user/0/wayland-1.lock
echo ===STARTING_BUSD===
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus RUST_LOG=trace /usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus &
sleep 2
echo ===STARTING_COMP===
unset WAYLAND_DISPLAY
unset DISPLAY
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus RUST_LOG=info /bin/cosmic-comp --no-xwayland &
sleep 8
echo ===STARTING_BG===
export WAYLAND_DISPLAY=wayland-1
RUST_LOG=info /bin/cosmic-bg &
sleep 6
echo ===STARTING_PANEL===
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus RUST_LOG=info /bin/cosmic-panel &
sleep 12
echo ===DESKTOP_DONE===
sleep 30
