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
#   NVK_AUTOTEST_KMS=0     skip the KMS test (/bin/nvkmstest, last: it takes the
#                          screen for ~30 s, then gives the text console back)
#   NVK_AUTOTEST_KMS_ARGS=... extra nvkmstest arguments (e.g. "--frames=300")
#   NVK_AUTOTEST_GL=0      skip nvgltest (OpenGL ES via GBM/EGL = zink on NVK)
#   NVK_AUTOTEST_GL_ARGS=  extra nvgltest arguments (e.g. "--only=info,fbo")

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

# OpenGL ES the way cosmic-comp and Firefox get it: GBM + EGL on the render
# node; Mesa's loader picks zink for a Turing+ nouveau device, and zink runs
# on NVK through the Khronos loader. The ICD is pinned to NVK so the virtio
# ICD (which finds no device here) stays out of the picture.
glrc=skipped
if [ "${NVK_AUTOTEST_GL:-1}" != 0 ]; then
    if [ -x /bin/nvgltest ]; then
        GLICD=""
        for m in /usr/share/vulkan/icd.d/nouveau_icd.*.json; do
            [ -e "$m" ] && GLICD=$m
        done
        if [ -n "$GLICD" ]; then
            VK_ICD_FILENAMES=$GLICD
            export VK_ICD_FILENAMES
        fi
        echo "==== nvk-autotest: nvgltest (GLES on GBM/EGL; VK_ICD_FILENAMES=${GLICD:-unset}) ===="
        # shellcheck disable=SC2086
        /bin/nvgltest --out-dir=/root ${NVK_AUTOTEST_GL_ARGS:-} 2>&1
        glrc=$?
        # cosmic-comp opens the PRIMARY node (card0) and builds GBM on it;
        # repeat the core of the test there when the render node passed.
        if [ "$glrc" = 0 ] && [ -e /dev/dri/card0 ]; then
            echo "==== nvk-autotest: nvgltest on /dev/dri/card0 (the compositor's node) ===="
            /bin/nvgltest --node=/dev/dri/card0 --only=info,fbo,gbm_bo,prime --out-dir=/root 2>&1
            echo "==== nvk-autotest: nvgltest card0 exit code $? ===="
        fi
        unset VK_ICD_FILENAMES
    else
        echo "nvk-autotest: /bin/nvgltest is not installed"
        glrc=127
    fi
    echo "==== nvk-autotest: nvgltest exit code $glrc ===="
    for tool in eglinfo es2_info; do
        if [ -x "/bin/$tool" ]; then
            echo "==== nvk-autotest: $tool ===="
            "/bin/$tool" 2>&1 | head -80
        fi
    done
fi

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

if [ "${NVK_AUTOTEST_KMS:-1}" != "0" ]; then
    if [ -x /bin/nvkmstest ]; then
        echo "==== nvk-autotest: KMS test (/bin/nvkmstest): card0 takes the screen, then gives the console back ===="
        sleep 2
        # shellcheck disable=SC2086
        /bin/nvkmstest --icd=/usr/lib/libvulkan_nouveau.so ${NVK_AUTOTEST_KMS_ARGS:-} 2>&1
        echo "==== nvk-autotest: nvkmstest exit code $? ===="
    else
        echo "nvk-autotest: /bin/nvkmstest is not installed; skipping the KMS test"
    fi
fi

echo "==== nvk-autotest: done (nvktest exit $rc, nvgltest exit $glrc) ===="
exit 0
