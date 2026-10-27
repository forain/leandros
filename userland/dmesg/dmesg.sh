#!/bin/sh
# /bin/dmesg — the kernel's RAM log (the newest 256 KiB of the ring), read from
# /proc/klog. Works with no ESP (Raspberry Pi 5) and with no grep installed.
#   dmesg            the whole snapshot
#   dmesg <text>     only lines containing <text> (e.g. dmesg brcmf)
# POSIX sh (/bin/sh is brush).

if [ ! -r /proc/klog ]; then
    echo "dmesg: /proc/klog not available (kernel too old)" >&2
    exit 1
fi
if [ -z "$1" ]; then
    cat /proc/klog
    exit 0
fi
while IFS= read -r line; do
    case "$line" in
        *"$1"*) printf '%s\n' "$line" ;;
    esac
done < /proc/klog
