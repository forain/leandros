#!/bin/bash
# usage: up.sh mixed|wpa1   (run as root)
set -e; cd /tmp/tkipap; MODE=${1:-mixed}
iw dev ap0 info >/dev/null 2>&1 || iw phy phy0 interface add ap0 type __ap
nmcli dev set ap0 managed no 2>/dev/null || true
ip link set ap0 down; ip link set ap0 address ca:94:02:15:57:ef
ip addr flush dev ap0; ip addr add 10.77.0.1/24 dev ap0
CH=$(iw dev wlan0 info | awk '/channel/{print $2}')
umask 077
{ echo "interface=ap0"; echo "driver=nl80211"; echo "ssid=leandros-tkip-test"; echo "hw_mode=a"; echo "channel=$CH"
  echo "ieee80211n=1"; echo "ht_capab=[HT40-]"; echo "ieee80211ac=1"; echo "vht_oper_chwidth=1"; echo "vht_oper_centr_freq_seg0_idx=42"; echo "wmm_enabled=1"; echo "auth_algs=1"; echo "wpa_key_mgmt=WPA-PSK"; echo "wpa_group_rekey=60"
  echo "ctrl_interface=/tmp/tkipap/ctrl"; echo "wpa_passphrase=$(cat /tmp/tkipap/psk)"
  if [ "$MODE" = mixed ]; then echo "wpa=3"; echo "wpa_pairwise=TKIP CCMP"; echo "rsn_pairwise=CCMP TKIP"
  else echo "wpa=1"; echo "wpa_pairwise=TKIP"; fi; } > hostapd.conf
./hostapd-2.11/hostapd/hostapd -B -t -P /tmp/tkipap/hostapd.pid -f /tmp/tkipap/hostapd.log hostapd.conf
sleep 2
[ -f dnsmasq.pid ] && kill -0 $(cat dnsmasq.pid) 2>/dev/null || dnsmasq --conf-file=/dev/null --interface=ap0 --bind-interfaces --except-interface=lo --port=0 \
  --dhcp-range=10.77.0.50,10.77.0.60,12h --dhcp-option=3 --dhcp-option=6 --pid-file=/tmp/tkipap/dnsmasq.pid \
  --log-dhcp --log-facility=/tmp/tkipap/dnsmasq.log --dhcp-leasefile=/tmp/tkipap/leases
echo "ch=$CH up"; iw dev ap0 info | grep -E 'type|channel'
