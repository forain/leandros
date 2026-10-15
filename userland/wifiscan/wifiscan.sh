#!/bin/sh
# /bin/wifiscan — show what the Wi-Fi driver (kernel iwlwifi) sees: the BSS
# list of its last scan (BSSID, channel, signal in dBm, security, SSID) and,
# with -v, the device and link state. The driver rescans every 30 s while
# it is not connected; there is nothing to trigger here.
#
#   wifiscan        the scan table (/proc/net/wifi)
#   wifiscan -v     plus the NIC state (/proc/net/nic)
# POSIX sh (/bin/sh is brush).

if [ ! -r /proc/net/wifi ]; then
    echo "wifiscan: /proc/net/wifi not available" >&2
    exit 1
fi
cat /proc/net/wifi
if [ "$1" = "-v" ]; then
    echo
    cat /proc/net/nic
fi
