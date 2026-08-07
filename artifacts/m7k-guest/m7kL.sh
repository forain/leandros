#!/bin/sh
# M7k staged comp-launch (short guest command avoids HVF <40-char corruption).
export PATH=/bin:/usr/bin:/usr/libexec
export XDG_RUNTIME_DIR=/run/user/0 HOME=/root ICED_BACKEND=tiny-skia
export COSMIC_BACKEND=kms COSMIC_RENDER_DEVICE=226:0 GBM_ALWAYS_SOFTWARE=1
export SMITHAY_USE_LEGACY=1 COSMIC_DISABLE_SYNCOBJ=1 COSMIC_DISABLE_DIRECT_SCANOUT=1
export LIBSEAT_BACKEND=builtin SEATD_VTBOUND=0 RUST_BACKTRACE=1
unset DISPLAY WAYLAND_DISPLAY
mkdir -p /run/user/0
rm -f /run/user/0/bus
export RUST_LOG=busd=trace,zbus=trace,info
/bin/m7repro armexec /usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &
sleep 5
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus
export RUST_LOG=info
echo COMP_LAUNCH
cosmic-comp --no-xwayland &
echo COMP_BACKGROUNDED
