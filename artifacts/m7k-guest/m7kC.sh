#!/bin/sh
mkdir -p /run/user/0
rm -f /run/user/0/bus
export RUST_LOG=busd=trace,zbus=trace,info
/bin/m7repro armexec /usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &
sleep 5
echo CLIENT_LAUNCH
/bin/m7repro coalclient /run/user/0/bus 4 >/tmp/cli.log 2>&1 &
echo CLIENT_BACKGROUNDED
