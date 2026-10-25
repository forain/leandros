#!/bin/sh
case "$1" in bound|renew) ip addr flush dev $interface; ip addr add $ip/${mask:-16} dev $interface; [ -n "$router" ] && ip route add default via ${router%% *} 2>/dev/null;; deconfig) ip addr flush dev $interface;; esac
exit 0
