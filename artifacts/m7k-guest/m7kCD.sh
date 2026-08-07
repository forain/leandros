#!/bin/sh
# M7k self-dumping coalescing-client repro (NO comp -> clean serial even under HVF).
# Confirms busd's handshake + socket_reader are healthy under HVF timing.
export PATH=/bin:/usr/bin:/usr/libexec
mkdir -p /run/user/0
rm -f /run/user/0/bus
export RUST_LOG=busd=trace,zbus=trace,info
/bin/m7repro armexec /usr/libexec/busd --config /usr/share/dbus-1/session.conf --address unix:path=/run/user/0/bus >/tmp/busd.log 2>&1 &
sleep 5
echo CLIENT_LAUNCH
/bin/m7repro coalclient /run/user/0/bus 4 >/tmp/cli.log 2>&1 &
echo CLIENT_BACKGROUNDED
sleep 24
echo M7K-CLILOG
cat /tmp/cli.log
echo M7K-BUSDLOG
grep -a -E 'peer: created|Waiting for message|Accepted connection|Handshake done|unique name' /tmp/busd.log | tail -8
echo M7K-SELFDUMP
/bin/m7repro dump
echo M7K-SELFDUMP-DONE
