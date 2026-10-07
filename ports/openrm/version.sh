# Pinned NVIDIA open-gpu-kernel-modules (OpenRM) release and the GSP firmware
# that must match it byte-for-version: RM refuses a GSP image whose version
# differs from its own, so tag and firmware move together.
#
# 570.86.16 is the version X547's nvidia-haiku port is built on (its submodule
# branch `570.86.16-haiku`, 4 small commits on top of the upstream tag).
# Sourced by fetch.sh and build.sh.

OPENRM_TAG="570.86.16"

OPENRM_TARBALL_URL="https://github.com/NVIDIA/open-gpu-kernel-modules/archive/refs/tags/${OPENRM_TAG}.tar.gz"
OPENRM_TARBALL_SHA256="36c373c2ce695e2b6590f9cddad4a0825e448a5556688ed0be240bf2c9101ad7"

# GSP firmware ships only inside NVIDIA's proprietary-userspace installer
# (`firmware/gsp_*.bin`); linux-firmware carries nouveau-renamed copies of
# other driver versions (nouveau pins its own), never 570.86.16.
NVRUN_URL="https://download.nvidia.com/XFree86/Linux-x86_64/${OPENRM_TAG}/NVIDIA-Linux-x86_64-${OPENRM_TAG}.run"
NVRUN_SHA256="4563ea4bb654247f491005a57c72c676aacd95abe1691d71332cecf25261cbd5"
# GA10x (Ampere: GA102/GA103/GA104/GA106/GA107; RTX 3070 = GA104) and TU10x
# (Turing, also used for GA100) images.
GSP_GA10X_SHA256="1805a695d3695007761a64d4b98ea121a287574a660b44add2e1bb587d7dbf2f"
GSP_TU10X_SHA256="53a8b59968ed85a7965187ebcf7a150e4f855ebdcc87947ff99ed0f3a9515b8d"

OPENRM_CACHE="${OPENRM_CACHE:-$HOME/.cache/leandros/openrm}"
OPENRM_SRC="$OPENRM_CACHE/open-gpu-kernel-modules-${OPENRM_TAG}"
OPENRM_FW_DIR="$OPENRM_CACHE/firmware/${OPENRM_TAG}"
