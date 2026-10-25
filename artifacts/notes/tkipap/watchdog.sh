#!/bin/bash
# dead-man: teardown AP if box uplink fails 3x in a row, or after deadline (s)
DL=$(( $(date +%s) + ${1:-3600} )); f=0
while :; do
  if ping -c1 -W2 -I wlan0 172.16.0.2 >/dev/null 2>&1; then f=0; else f=$((f+1)); fi
  if [ $f -ge 3 ] || [ $(date +%s) -ge $DL ] || [ -f /tmp/tkipap/stopwd ]; then
    echo "$(date) wd fire f=$f" >> /tmp/tkipap/wd.log; /tmp/tkipap/down.sh >> /tmp/tkipap/wd.log 2>&1
    [ $f -ge 3 ] && nmcli con up "Anvara 5G" >> /tmp/tkipap/wd.log 2>&1; exit 0; fi
  sleep 10
done
