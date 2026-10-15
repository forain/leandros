#!/bin/sh
# /bin/wifiscan — show what the Wi-Fi driver (kernel iwlwifi) sees: the BSS
# list of its last scan (BSSID, channel, signal in dBm, security, SSID) and,
# with -v, the device and link state. The driver rescans every 30 s while
# it is not connected; there is nothing to trigger here.
#
#   wifiscan        the scan table (/proc/net/wifi)
#   wifiscan -v     plus the wifi0 interface state (its /proc/net/nic section)
# POSIX sh (/bin/sh is brush).

if [ ! -r /proc/net/wifi ]; then
    echo "wifiscan: /proc/net/wifi not available" >&2
    exit 1
fi
cat /proc/net/wifi
if [ "$1" = "-v" ]; then
    echo
    # /proc/net/nic has one [ifname] section per interface: print the Wi-Fi
    # one(s) and the daemon line; the whole file on a kernel without sections.
    show=0
    any=0
    while IFS= read -r line; do
        case "$line" in
            "[wifi"*"]") show=1; any=1 ;;
            "["*"]") show=0 ;;
            netd:*) [ "$any" = 1 ] && show=1 ;;
        esac
        [ "$show" = 1 ] && printf '%s\n' "$line"
    done < /proc/net/nic
    [ "$any" = 1 ] || cat /proc/net/nic
fi
