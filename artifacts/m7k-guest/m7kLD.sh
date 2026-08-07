#!/bin/sh
# M7k self-dumping comp launch: busd(armexec) + comp, then dump the ring IN-GUEST
# at 22s and 42s (dodges HVF host->guest command corruption entirely — only guest
# stdout is used). busd.log key lines grep'd for peer:created confirmation.
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
sleep 22
echo M7K-BUSDLOG-1
grep -a -E 'peer: created|Waiting for message|Accepted connection|Handshake done' /tmp/busd.log | tail -6
echo M7K-SELFDUMP-1
/bin/m7repro dump
sleep 20
echo M7K-BUSDLOG-2
grep -a -E 'peer: created|Waiting for message|Accepted connection|Handshake done' /tmp/busd.log | tail -6
echo M7K-SELFDUMP-2
/bin/m7repro dump
echo M7K-SELFDUMP-DONE
