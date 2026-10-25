#!/bin/bash
# run as root: every 15 s flush ARP for the laptop (forces a broadcast ARP request = group-key frame) and ping it
cd /tmp/tkipap; : > mon.log
for i in $(seq 1 60); do L=$(awk '{print $3}' leases 2>/dev/null | tail -1); [ -n "$L" ] && break; sleep 3; done
echo "lease $L" >> mon.log
for i in $(seq 1 24); do
  ip neigh flush dev ap0
  if ping -c2 -W2 -I ap0 $L >/dev/null 2>&1; then r=ok; else r=FAIL; fi
  b=$(ping -c1 -W2 -b 10.77.0.255 2>/dev/null | grep -c "bytes from $L")
  echo "$(date +%T) arp+ping $r bcast_reply=$b" >> mon.log; sleep 15
done
