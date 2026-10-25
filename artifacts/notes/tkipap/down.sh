#!/bin/bash
cd /tmp/tkipap
for p in hostapd dnsmasq; do [ -f $p.pid ] && kill $(cat $p.pid) 2>/dev/null; rm -f $p.pid; done
sleep 1; iw dev ap0 del 2>/dev/null; echo down; ip -br link | grep -c ap0
