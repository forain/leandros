#!/bin/sh
# x86_64 only. Run INSIDE the guest (or via scripts/remote.py run /bin/sh ...):
# Firefox's vaapitest must report VAAPI_SUPPORTED TRUE with the iHD driver.
# The guest has no grep, so the output is matched with case.
[ "$(uname -m)" = x86_64 ] || { echo "SKIP check-vaapi: $(uname -m)"; exit 0; }
[ -e /usr/lib/dri/iHD_drv_video.so ] || { echo "FAIL check-vaapi: no /usr/lib/dri/iHD_drv_video.so"; exit 1; }
[ -e /usr/lib/libigdgmm.so.12 ] || { echo "FAIL check-vaapi: no libigdgmm.so.12"; exit 1; }
T=/usr/lib/firefox/vaapitest
[ -x "$T" ] || { echo "FAIL check-vaapi: no $T"; exit 1; }
out=$(LIBVA_DRIVER_NAME=iHD LIBVA_MESSAGING_LEVEL=0 XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/1000} "$T" 2>&1)
echo "$out"
case "$out" in
  *"VAAPI_SUPPORTED TRUE"*) echo "PASS check-vaapi" ;;
  *) echo "FAIL check-vaapi: VAAPI_SUPPORTED TRUE not printed"; exit 1 ;;
esac
