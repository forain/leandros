#!/usr/bin/env bash
# pi-v3d-sysfs.sh - give an already-installed Pi root the rpi5 card0/renderD128
# sysfs identity (platform bus, v3d uevent) and drop the virtio-pci identity
# attributes. pi-sync skips /sys/dev/char/226:*, so an image prepared before
# LEANDROS_BOARD=rpi5 keeps the PCI identity, which makes Mesa's GL loader
# pick softpipe (it finds vendor/device files and no matching PCI id).
# Usage: scripts/pi-v3d-sysfs.sh [pi:<ip>]
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
export LEANDROS_REMOTE="${1:-${LEANDROS_REMOTE:-pi}}"
python3 "$HERE/remote.py" run 'for m in 0 128; do d=/sys/dev/char/226:$m/device; rm -f $d/vendor $d/device $d/subsystem_vendor $d/subsystem_device $d/class $d/config $d/revision $d/subsystem; ln -s ../../../../bus/platform $d/subsystem; printf "DRIVER=v3d\nOF_NAME=v3d\nOF_FULLNAME=/axi/v3d@2000000\nOF_COMPATIBLE_0=brcm,2712-v3d\nOF_COMPATIBLE_N=1\nMODALIAS=of:Nv3dT(null)Cbrcm,2712-v3d\n" > $d/uevent; done; ls -l /sys/dev/char/226:0/device/subsystem'
