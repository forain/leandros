#!/bin/bash
cd /tmp/tkipap
for p in $(pgrep -f '[b]oxmon.sh') $(pgrep -f '[w]atchdog.sh'); do kill $p; done
[ -f dnsmasq.pid ] && kill $(cat dnsmasq.pid); rm -f dnsmasq.pid
pkill -x hostapd; sleep 1; iw dev ap0 del 2>/dev/null
d=/sys/kernel/debug/ieee80211/phy0/mt76; echo 1 > $d/runtime-pm; echo 1 > $d/deep-sleep
rm -f psk hostapd.conf ccmp.conf h.tgz
echo "rpm=$(cat $d/runtime-pm) ds=$(cat $d/deep-sleep) hostapd=$(pgrep -xc hostapd) dnsmasq_ours=$(pgrep -af dnsmasq | grep -c tkipap) scripts=$(pgrep -fc '[b]oxmon.sh|[w]atchdog.sh') ifaces=$(iw dev | grep -c Interface)"
