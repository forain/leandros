#!/bin/sh
# test window on the throwaway TKIP-group AP, then always back to Anvara
P=$(cat /root/tkip.psk)
netctl save leandros-tkip-test "$P" --no-auto --prio 0 > /root/tkip-join.log 2>&1
netctl connect leandros-tkip-test "$P" --if wifi0 >> /root/tkip-join.log 2>&1
sleep 20
netctl status >> /root/tkip-join.log 2>&1
ping -c 360 10.77.0.1 > /root/tkip-ping.txt 2>&1
netctl connect --uuid 9cbc2450-5216-453d-8c7e-84a3c682540c >> /root/tkip-join.log 2>&1
