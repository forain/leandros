#!/bin/sh
# /bin/nvk-autotest — run the NVK bring-up test unattended at boot and print
# everything to the console (which the real-PC boot persists to the USB log).
# POSIX sh: /bin/sh is brush. Bounded: at most NVK_AUTOTEST_WAIT (120 s) for
# the render node, NVK_AUTOTEST_DELAY (20 s) of settling, then nvktest's own
# per-subtest watchdog; nothing here loops forever.
#
#   NVK_AUTOTEST_WAIT=N    seconds to wait for /dev/dri/renderD128 (default 120)
#   NVK_AUTOTEST_DELAY=N   seconds to sleep once it exists, so the NVIDIA
#                          bring-up (GSP boot, channel setup) can finish (default 20)
#   NVK_AUTOTEST_ARGS=...  extra nvktest arguments (e.g. "--timeout-ms=60000")

echo "==== nvk-autotest: start ===="

NODE=/dev/dri/renderD128
WAIT=${NVK_AUTOTEST_WAIT:-120}
DELAY=${NVK_AUTOTEST_DELAY:-20}

waited=0
while [ ! -e "$NODE" ] && [ "$waited" -lt "$WAIT" ]; do
    sleep 2
    waited=$((waited + 2))
done
if [ -e "$NODE" ]; then
    echo "nvk-autotest: $NODE present after ${waited} s; settling ${DELAY} s for the NVIDIA bring-up"
    sleep "$DELAY"
else
    echo "nvk-autotest: $NODE did not appear within ${WAIT} s; running nvktest anyway (expect 0 devices)"
fi
echo "nvk-autotest: /dev/dri contains: $(ls /dev/dri 2>&1 | tr '\n' ' ')"

MESA_SHADER_CACHE_DISABLE=true
HOME=/root
export MESA_SHADER_CACHE_DISABLE HOME
unset VK_ICD_FILENAMES

if [ -x /bin/nvktest ]; then
    # shellcheck disable=SC2086
    /bin/nvktest --icd /usr/lib/libvulkan_nouveau.so --out-dir=/root ${NVK_AUTOTEST_ARGS:-} 2>&1
    rc=$?
else
    echo "nvk-autotest: /bin/nvktest is not installed"
    rc=127
fi
echo "==== nvk-autotest: nvktest exit code $rc ===="

if [ -x /bin/vulkaninfo ]; then
    MANIFEST=""
    for m in /usr/share/vulkan/icd.d/nouveau_icd.*.json; do
        [ -e "$m" ] && MANIFEST=$m
    done
    if [ -n "$MANIFEST" ]; then
        echo "==== nvk-autotest: vulkaninfo --summary (VK_ICD_FILENAMES=$MANIFEST) ===="
        # vulkaninfo has no watchdog of its own: bound it when timeout(1) exists.
        if command -v timeout >/dev/null 2>&1; then
            VK_ICD_FILENAMES=$MANIFEST timeout 60 /bin/vulkaninfo --summary 2>&1
        else
            VK_ICD_FILENAMES=$MANIFEST /bin/vulkaninfo --summary 2>&1
        fi
        echo "==== nvk-autotest: vulkaninfo exit code $? ===="
    else
        echo "nvk-autotest: no nouveau ICD manifest under /usr/share/vulkan/icd.d; skipping vulkaninfo"
    fi
fi

echo "==== nvk-autotest: done ===="
exit 0
