#!/bin/bash
# LeandrOS Cross-Platform QEMU Runner Script
# Boots LeandrOS on both AArch64 and x86_64 architectures

set -e

OS=$(uname -s)
HOST_ARCH=$(uname -m)
BOOT_MODE="uefi"
ARCH="x86_64"
# HVF (Hypervisor.framework) auto-selects on an Apple Silicon host below, once
# ARCH/BOOT_MODE are known — only the aarch64 UEFI/Limine path supports it.
# QEMU >= 11.1 refuses `-accel hvf` on a GICv2 machine ("HVF does not support
# GICv2 emulation"), so the virt board is GICv3 (the kernel detects either at
# boot). Force override with --tcg (software emulation, e.g. for comparison/
# debugging) or --hvf (force HVF even off Apple Silicon, where it will fail
# to launch).
ACCEL=""
QEMU_EXTRA_ARGS=()
# ── GPU path ────────────────────────────────────────────────────────────────
# COSMIC renders on the host GPU, never in software: the guest's /bin/gpu-env
# refuses to start a compositor unless GBM+EGL come up on a hardware renderer
# (zink over Venus, or virgl), and the graphical login is then simply not
# started (init prints a banner; the serial login is unaffected). So the host
# side picks a GPU device BY DEFAULT wherever one can work:
#
#   auto   (default) Venus where the host QEMU/virglrenderer can do it, virgl
#          where only GL passthrough exists, and nothing where neither does
#          (macOS Homebrew QEMU has no virglrenderer — see the warning below).
#   venus  --venus / LEANDROS_VENUS=1: virtio-gpu venus=on,blob=on — the guest
#          renders through zink -> Venus -> host Vulkan. Also carries the virgl
#          capset, so gpu-env can fall back to virgl on the same device.
#   virgl  --virgl / LEANDROS_VIRGL=1: virtio-vga-gl / virtio-gpu-gl-pci.
#   none   --no-gpu (alias --no-virgl) / LEANDROS_GPU=none: plain virtio-gpu.
#          For headless kernel tests that never draw: the guest skips the
#          graphical login and the serial console works as always.
GPU_MODE="${LEANDROS_GPU:-auto}"
if [ "${LEANDROS_VENUS:-0}" = "1" ]; then GPU_MODE=venus; fi
if [ "${LEANDROS_VIRGL:-0}" = "1" ]; then GPU_MODE=virgl; fi
VENUS=0
VIRGL=0
# --venus opens a real window when the host has a display server; this forces
# the offscreen egl-headless path instead, for harnesses that must not open one.
VENUS_HEADLESS=0
if [ "${LEANDROS_VENUS_HEADLESS:-0}" = "1" ]; then VENUS_HEADLESS=1; fi

# Hardware acceleration only applies when the guest architecture matches the
# host's — a hypervisor virtualises, it does not translate. Map uname's arch
# spelling onto ours so "arm64" (macOS) and "aarch64" (Linux) compare equal.
host_arch_normalized() {
    case "$1" in
        arm64|aarch64) echo "aarch64" ;;
        x86_64|amd64)  echo "x86_64" ;;
        *)             echo "$1" ;;
    esac
}
HOST_ARCH_N=$(host_arch_normalized "$HOST_ARCH")

# Pick an audio backend that can actually open on this host, and echo the
# -audiodev argument for it. PulseAudio is not a safe default on Linux: a
# headless/SSH build box usually has no sound daemon, and QEMU ABORTS AT STARTUP
# when the backend cannot open — so guessing wrong costs the whole run, not just
# the sound. Probe what this QEMU build actually supports rather than assuming.
# Reads $QEMU_SYSTEM, so it must be called after the boot-mode dispatch sets it.
select_audio_args() {
    if [ "$OS" = "Darwin" ]; then
        echo "-audiodev coreaudio,id=snd0"
        return
    fi
    local backends
    backends=$($QEMU_SYSTEM -audiodev help 2>/dev/null || true)
    # A live PulseAudio/PipeWire-pulse session is the only positive evidence
    # that `pa` will connect; presence in -audiodev help only means it compiled.
    if [ -n "${PULSE_SERVER:-}" ] || [ -S "${XDG_RUNTIME_DIR:-/nonexistent}/pulse/native" ]; then
        echo "-audiodev pa,id=snd0"
    elif grep -q '\bpipewire\b' <<<"$backends"; then
        echo "-audiodev pipewire,id=snd0"
    elif grep -q '\balsa\b' <<<"$backends"; then
        echo "-audiodev alsa,id=snd0"
    else
        echo "-audiodev none,id=snd0"
    fi
}

# ── Host QEMU ───────────────────────────────────────────────────────────────
# macOS: Homebrew's qemu has no virglrenderer, so on a Mac prefer the GPU build
# from scripts/mac-qemu-gpu/build.sh. LEANDROS_QEMU_PREFIX picks a prefix
# explicitly on any host (its bin/qemu-system-* and share/qemu firmware win).
# Otherwise, on macOS, the first installed of:
#   ~/.local/qemu-gpu-gles31  build.sh --angle-vulkan: ANGLE on Vulkan/MoltenVK,
#                             guest GLES 3.1 + SSBOs, so iced/wgpu works
#   ~/.local/qemu-gpu         build.sh default: ANGLE on Metal, guest GLES 3.0
# else $PATH.
QEMU_PREFIX="${LEANDROS_QEMU_PREFIX:-}"

# An --angle-vulkan prefix's ANGLE dlopens Homebrew's Vulkan loader by the
# absolute path baked in at build time, and the loader must find MoltenVK.
# setup_moltenvk_env fails with a clear message when either is missing, and
# otherwise pins VK_DRIVER_FILES to MoltenVK's ICD (unless the caller chose an
# ICD already), so nothing needs exporting by hand.
angle_vulkan_loader() { strings "$1/lib/libGLESv2.dylib" 2>/dev/null | grep -m1 '^/.*/libvulkan\.1\.dylib$'; }
setup_moltenvk_env() {
    local loader icd="" brew
    loader="$(angle_vulkan_loader "$1")"
    for brew in /opt/homebrew /usr/local; do
        [ -e "$brew/etc/vulkan/icd.d/MoltenVK_icd.json" ] && { icd="$brew/etc/vulkan/icd.d/MoltenVK_icd.json"; break; }
    done
    if [ ! -e "$loader" ] || [ -z "$icd" ]; then
        echo "❌ $1 is an ANGLE-on-Vulkan QEMU; it needs Homebrew's Vulkan runtime:"
        [ -e "$loader" ] || echo "❌   missing vulkan-loader ($loader)"
        [ -n "$icd" ] || echo "❌   missing molten-vk (etc/vulkan/icd.d/MoltenVK_icd.json)"
        echo "❌ Install with:  brew install molten-vk vulkan-loader"
        return 1
    fi
    if [ -z "${VK_DRIVER_FILES:-}" ] && [ -z "${VK_ICD_FILENAMES:-}" ]; then
        # Resolve Homebrew's symlink: the ICD's library_path is relative to the file.
        export VK_DRIVER_FILES="$(cd "$(dirname "$icd")" && cd "$(dirname "$(readlink "$icd" || echo "$icd")")" && pwd)/MoltenVK_icd.json"
    fi
}
if [ "$OS" = "Darwin" ]; then
    if [ -n "$QEMU_PREFIX" ]; then
        if [ -n "$(angle_vulkan_loader "$QEMU_PREFIX")" ]; then setup_moltenvk_env "$QEMU_PREFIX" || exit 1; fi
    else
        if [ -x "$HOME/.local/qemu-gpu-gles31/bin/qemu-system-aarch64" ]; then
            if setup_moltenvk_env "$HOME/.local/qemu-gpu-gles31"; then
                QEMU_PREFIX="$HOME/.local/qemu-gpu-gles31"
            else
                echo "❌ Falling back to the next GPU QEMU: the guest gets GLES 3.0, so no wgpu."
            fi
        fi
        if [ -z "$QEMU_PREFIX" ] && [ -x "$HOME/.local/qemu-gpu/bin/qemu-system-aarch64" ]; then
            QEMU_PREFIX="$HOME/.local/qemu-gpu"
        fi
    fi
fi

# Firmware search paths. Ordered most-specific first; the first hit wins.
# Arch/EndeavourOS keeps edk2 under /usr/share/edk2/<arch>/ with a 4 MB split
# CODE/VARS pair, which is why the plain OVMF.fd names below do not match there.
X86_64_FW_PATHS=("/usr/share/ovmf/OVMF.fd" "/usr/share/OVMF/OVMF_CODE.fd" "/opt/homebrew/share/qemu/edk2-x86_64-code.fd" "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd" "/usr/share/edk2/x64/OVMF_CODE.4m.fd" "/usr/share/edk2/x64/OVMF_CODE.fd")
AARCH64_FW_PATHS=("/usr/share/AAVMF/AAVMF_CODE.fd" "/opt/homebrew/share/qemu/edk2-aarch64-code.fd" "/usr/share/edk2-armvirt/aarch64/QEMU_EFI-pflash.raw" "/usr/share/edk2/aarch64/QEMU_CODE.4m.fd" "/usr/share/edk2/aarch64/QEMU_CODE.fd" "/usr/share/edk2/aarch64/QEMU_EFI.fd")

# Matching writable VARS templates, same ordering convention. A split firmware
# build needs its own VARS pflash; a combined image (OVMF.fd) does not.
X86_64_VARS_PATHS=("/opt/homebrew/share/qemu/edk2-i386-vars.fd" "/usr/share/edk2/x64/OVMF_VARS.4m.fd" "/usr/share/edk2/x64/OVMF_VARS.fd" "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd" "/usr/share/OVMF/OVMF_VARS.fd")
AARCH64_VARS_PATHS=("/opt/homebrew/share/qemu/edk2-arm-vars.fd" "/usr/share/edk2/aarch64/QEMU_VARS.4m.fd" "/usr/share/edk2/aarch64/QEMU_VARS.fd" "/usr/share/edk2-armvirt/aarch64/vars-template-pflash.raw" "/usr/share/AAVMF/AAVMF_VARS.fd")
if [ -n "$QEMU_PREFIX" ]; then
    X86_64_FW_PATHS=("$QEMU_PREFIX/share/qemu/edk2-x86_64-code.fd" "${X86_64_FW_PATHS[@]}")
    AARCH64_FW_PATHS=("$QEMU_PREFIX/share/qemu/edk2-aarch64-code.fd" "${AARCH64_FW_PATHS[@]}")
    X86_64_VARS_PATHS=("$QEMU_PREFIX/share/qemu/edk2-i386-vars.fd" "${X86_64_VARS_PATHS[@]}")
    AARCH64_VARS_PATHS=("$QEMU_PREFIX/share/qemu/edk2-arm-vars.fd" "${AARCH64_VARS_PATHS[@]}")
fi

while [[ "$#" -gt 0 ]]; do
    case $1 in
        x86_64|aarch64) ARCH="$1"; shift ;;
        --direct) BOOT_MODE="direct"; shift ;;
        --uefi) BOOT_MODE="uefi"; shift ;;
        # QEMU raspi4b (BCM2711) — testable stepping stone for the sdhci
        # driver (drivers/src/sdhci.rs); aarch64-only, no PCI bus, no
        # GPU/sound/keyboard devices. Not a hardware target.
        --raspi4b) BOOT_MODE="raspi4b"; ARCH="aarch64"; shift ;;
        --hvf) ACCEL="hvf"; shift ;;
        --kvm) ACCEL="kvm"; shift ;;
        --tcg) ACCEL="tcg"; shift ;;
        --venus) GPU_MODE=venus; shift ;;
        --venus-headless) GPU_MODE=venus; VENUS_HEADLESS=1; shift ;;
        --virgl) GPU_MODE=virgl; shift ;;
        --no-gpu|--no-virgl) GPU_MODE=none; shift ;;
        --gpu) GPU_MODE="$2"; shift 2 ;;
        -d) QEMU_EXTRA_ARGS+=("$2"); shift 2 ;;
        *) QEMU_EXTRA_ARGS+=("$1"); shift ;;
    esac
done

if [ -z "$ACCEL" ]; then
    # Pick the fastest accelerator this host can actually provide for this
    # guest. Requires arch match; anything else falls back to TCG emulation.
    if [ "$HOST_ARCH_N" != "$ARCH" ]; then
        ACCEL="tcg"
    elif [ "$OS" = "Darwin" ]; then
        # HVF is only wired up for the aarch64 UEFI/Limine path; direct boot
        # hangs on an upstream PL011/HVF timer-starvation bug.
        if [ "$ARCH" = "aarch64" ] && [ "$BOOT_MODE" = "uefi" ]; then
            ACCEL="hvf"
        else
            ACCEL="tcg"
        fi
    elif [ "$OS" = "Linux" ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        ACCEL="kvm"
    else
        ACCEL="tcg"
    fi
fi

# Validate an explicit request rather than letting QEMU fail obscurely later.
case "$ACCEL" in
    hvf)
        if [ "$OS" != "Darwin" ]; then echo "❌ --hvf requires a macOS host"; exit 1; fi
        if [ "$ARCH" != "aarch64" ] || [ "$BOOT_MODE" != "uefi" ]; then
            echo "❌ --hvf only works with aarch64 --uefi (the default boot mode for that arch)"; exit 1
        fi ;;
    kvm)
        if [ "$OS" != "Linux" ]; then echo "❌ --kvm requires a Linux host"; exit 1; fi
        if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
            echo "❌ --kvm requested but /dev/kvm is not readable/writable (add your user to the 'kvm' group)"; exit 1
        fi
        if [ "$HOST_ARCH_N" != "$ARCH" ]; then
            echo "❌ --kvm cannot run a $ARCH guest on a $HOST_ARCH_N host"; exit 1
        fi ;;
esac

echo "⚡ Accelerator: $ACCEL (host ${OS}/${HOST_ARCH_N}, guest ${ARCH})"

if [ "$BOOT_MODE" = "raspi4b" ]; then
    QEMU_SYSTEM="qemu-system-aarch64"
    # No gic-version=/-cpu override: raspi4b is a fixed-SoC board (4x
    # cortex-a72, GIC-400), unlike the generic `virt` machine.
    MACHINE_ARGS="-machine raspi4b -m 2G -smp 4"
    DISK_IMAGE="leandros-limine-aarch64.img" # unused in raspi4b mode
elif [ "$ARCH" = "aarch64" ]; then
    QEMU_SYSTEM="qemu-system-aarch64"
    # -smp 4: SMP bringup via PSCI CPU_ON.
    # gic-version=3: the only GIC HVF will launch (QEMU >= 11.1); the kernel
    # drives GICv2 or GICv3 by detection, so TCG/KVM hosts use the same line.
    MACHINE_ARGS="-machine virt,gic-version=3 -m 2G -smp 4"
    # -cpu host: real host ID registers, required by HVF/KVM passthrough
    # (vs. -cpu max's synthesized model, which is TCG-only).
    #
    # lpa2=off on the TCG model: `max` otherwise advertises FEAT_LPA2 (52-bit
    # physical addresses) and Limine 11.4.1 wedges on it during its final
    # handoff — spinning forever on one instruction with the kernel entry
    # already in x0, so the kernel never prints. Nothing here uses 52-bit PAs.
    case "$ACCEL" in
        hvf) CPU_ARGS="-cpu host -accel hvf" ;;
        kvm) CPU_ARGS="-cpu host -accel kvm" ;;
        *)   CPU_ARGS="-cpu max,lpa2=off -accel tcg" ;;
    esac
    DISK_IMAGE="leandros-limine-aarch64.img"
else
    QEMU_SYSTEM="qemu-system-x86_64"
    # 2 cores × 2 threads: exercises the scheduler's SMT-aware idle-CPU
    # selection (CPUID leaf 0xB reports the hyperthread topology).
    MACHINE_ARGS="-machine q35 -smp 4,sockets=1,cores=2,threads=2"
    case "$ACCEL" in
        kvm) CPU_ARGS="-cpu host -accel kvm" ;;
        *)   CPU_ARGS="-cpu max -accel tcg" ;;
    esac
    DISK_IMAGE="leandros-limine-x86_64.img"
fi

if [ -n "$QEMU_PREFIX" ]; then
    QEMU_SYSTEM="$QEMU_PREFIX/bin/$QEMU_SYSTEM"
    [ -x "$QEMU_SYSTEM" ] || { echo "❌ $QEMU_SYSTEM not found (LEANDROS_QEMU_PREFIX=$QEMU_PREFIX)"; exit 1; }
    echo "🧰 QEMU: $QEMU_SYSTEM"
fi

# Resolve GPU_MODE=auto into a concrete path for THIS host. Every check is
# a capability probe of the QEMU we are about to run, never an OS-name guess.
qemu_has_device() { $QEMU_SYSTEM -device help 2>&1 | grep -q "\"$1\""; }
host_gl_possible() {
    # virglrenderer needs a host EGL: a render node on Linux; on macOS the
    # ANGLE EGL of a scripts/mac-qemu-gpu/build.sh QEMU, whose display
    # is egl-headless. Homebrew's QEMU has neither *-gl devices nor
    # egl-headless, so it still resolves to none.
    if [ "$OS" = "Darwin" ]; then
        $QEMU_SYSTEM -display help 2>/dev/null | grep -qx egl-headless || return 1
    else
        [ "$OS" = "Linux" ] || return 1
        ls /dev/dri/renderD* >/dev/null 2>&1 || return 1
    fi
    qemu_has_device virtio-gpu-gl-pci || qemu_has_device virtio-vga-gl
}
host_venus_possible() {
    # QEMU exposes the venus= property only when built against a
    # virglrenderer with Venus; the device still needs a host Vulkan driver
    # for the GPU, which the guest verifies (zink probe) and falls back from.
    $QEMU_SYSTEM -device virtio-gpu-gl-pci,help 2>&1 | grep -q '^ *venus='
}
case "$GPU_MODE" in
    auto)
        if [ "$BOOT_MODE" = "raspi4b" ]; then GPU_MODE=none
        # macOS Venus (UTM's virglrenderer fork + MoltenVK) is experimental:
        # the host rejects the guest's vkCreateInstance, so auto never picks it.
        elif [ "$OS" != "Darwin" ] && host_gl_possible && host_venus_possible; then GPU_MODE=venus
        elif host_gl_possible; then GPU_MODE=virgl
        else GPU_MODE=none
        fi ;;
    venus|virgl|none) ;;
    *) echo "❌ --gpu must be auto|venus|virgl|none (got '$GPU_MODE')"; exit 1 ;;
esac
case "$GPU_MODE" in
    venus) VENUS=1 ;;
    virgl) VIRGL=1 ;;
esac
if [ "$GPU_MODE" = "none" ] && [ "$BOOT_MODE" != "raspi4b" ]; then
    echo "⚠️  ────────────────────────────────────────────────────────────────"
    echo "⚠️  NO GPU PATH: this guest gets a plain virtio-gpu (no 3D)."
    if [ "$OS" = "Darwin" ]; then
        echo "⚠️  $QEMU_SYSTEM on macOS has no virglrenderer, so neither Venus"
        echo "⚠️  nor virgl exists here. Build the GPU QEMU once with"
        echo "⚠️  scripts/mac-qemu-gpu/build.sh --angle-vulkan (virgl over ANGLE on"
        echo "⚠️  MoltenVK, GLES 3.1; installs to ~/.local/qemu-gpu-gles31, picked up"
        echo "⚠️  automatically; needs brew molten-vk vulkan-loader), or run on the linux"
        echo "⚠️  desktop (x86_64/KVM, Venus)."
    fi
    echo "⚠️  COSMIC will NOT start (no software rendering); serial login only."
    echo "⚠️  Opt in to softpipe for debugging, in the guest:"
    echo "⚠️      touch /etc/leandros/allow-software-render"
    echo "⚠️  ────────────────────────────────────────────────────────────────"
else
    echo "🎮 GPU path: $GPU_MODE"
fi

# Host-side workaround, AMD radeonsi hosts only (lane hostgpufault, 2026-09-27).
# With radeonsi's threaded context (u_threaded_context) on, a virgl COSMIC
# session that opens cosmic-term faulted the HOST GPU in most boots on the
# linux desktop (Raphael iGPU, Mesa 26.1.3, virglrenderer 1.3.0): `[gfxhub]
# page fault ... SQC (data)` at garbage GPU addresses (0x0, 0x3f800000 = 1.0f,
# 0x80010xx000), a gfx ring reset, and QEMU exiting with "The CS has cancelled
# because the context is lost". GALLIUM_THREAD=0 made it go away; Venus (RADV)
# never faulted. Guest pages cannot be involved: without blob resources virgl
# never lets the host GPU see guest memory, and the address comes from the host
# driver. Setting GALLIUM_THREAD yourself overrides this.
if [ "$OS" = "Linux" ] && [ "$GPU_MODE" != "none" ] && [ -z "${GALLIUM_THREAD+x}" ]; then
    for _drv in /sys/class/drm/renderD*/device/driver; do
        if [ "$(basename "$(readlink -f "$_drv" 2>/dev/null)")" = "amdgpu" ]; then
            export GALLIUM_THREAD=0
            echo "🛡️  amdgpu host: GALLIUM_THREAD=0 for QEMU (radeonsi threaded-context GPU fault workaround)"
            break
        fi
    done
    unset _drv
fi

# Select GPU device.
# x86_64: prefer virtio-vga — it is VGA-compatible so UEFI/OVMF exposes a GOP
#         framebuffer that Limine can use.  virtio-gpu-pci has no VGA interface
#         and leaves UEFI with no display device to hand to Limine.
# aarch64: virtio-gpu-pci is correct; VGA is an x86 concept.
GL_ARGS=()
if [ "$ARCH" = "aarch64" ]; then
    if [ "$VIRGL" = "1" ] && qemu_has_device virtio-gpu-gl-pci; then
        GPU_DEV="virtio-gpu-gl-pci"
        GL_ARGS=("-display" "default,gl=on")
    else
        GPU_DEV="virtio-gpu-pci"
    fi
else
    # x86_64 needs a VGA-compatible device or OVMF has no GOP to hand Limine —
    # which is why virtio-gpu-gl-pci is NOT a candidate here, however much we
    # want its virgl. virtio-vga-gl is the device that satisfies both: it is
    # virtio-vga plus a virglrenderer context, so OVMF still sees VGA registers
    # and the guest still gets 3D. Prefer it, and keep plain virtio-vga as the
    # fallback for a QEMU built without virglrenderer.
    #
    # Ordering matters: `grep -q virtio-vga` also matches "virtio-vga-gl", so the
    # GL probe has to come FIRST or it can never be reached. That exact shadowing
    # is what made the old virtio-gpu-gl-pci branch below dead code on x86_64.
    if [ "$VIRGL" = "1" ] && $QEMU_SYSTEM -device help 2>&1 | grep -q virtio-vga-gl; then
        GPU_DEV="virtio-vga-gl"
        GL_ARGS=("-display" "default,gl=on")
    elif qemu_has_device virtio-vga; then
        GPU_DEV="virtio-vga"
    else
        GPU_DEV="virtio-gpu-pci"
    fi
fi

# Select display. Without X or Wayland, QEMU's default GTK/SDL backend cannot
# open and the run dies at startup — so a headless host (an SSH session on a
# build box) must be told explicitly. egl-headless keeps the guest's virtio-gpu
# GL-capable, which venus needs; it just renders offscreen. Applies to every
# boot mode, so it lives before the boot-mode dispatch below.
# --venus makes its own display choice below and would only override this one,
# so skip it here rather than print a message that the next block contradicts.
# macOS: upstream QEMU's cocoa UI has no GL, so a GL device always runs under
# egl-headless (ANGLE/Metal, offscreen) and is viewed over VNC: the readback
# lands on the console surface, which VNC and QMP screendump both serve.
# LEANDROS_VNC (default 127.0.0.1:0 → port 5900) moves the listener.
#
# Interactive runs (stdout is a terminal, LEANDROS_NO_VIEWER unset) open a
# viewer window by themselves once QEMU's VNC port listens, so the display
# shows up like a cocoa window would; the serial console stays on stdio.
# macOS Screen Sharing refuses a no-auth VNC server (it asks for a password
# that nothing accepts), so an interactive run gives the listener a random
# one-run password (QEMU VNC auth via -object secret) and hands it to Screen
# Sharing in the vnc:// URL: no prompt. Non-interactive runs keep the no-auth
# listener and never open a window (driver.py does not use this script).
if [ "$OS" = "Darwin" ] && [ "${#GL_ARGS[@]}" -gt 0 ]; then
    MAC_VNC="${LEANDROS_VNC:-127.0.0.1:0}"
    MAC_VNC_HOST="${MAC_VNC%:*}"
    MAC_VNC_PORT=$((5900 + ${MAC_VNC##*:}))
    if [ -t 1 ] && [ -z "${LEANDROS_NO_VIEWER:-}" ]; then
        MAC_VNC_PW="$(LC_ALL=C tr -dc 'a-zA-Z0-9' < /dev/urandom | head -c 8)"
        GL_ARGS=("-display" "egl-headless"
                 "-object" "secret,id=leandrosvncpw,data=$MAC_VNC_PW"
                 "-vnc" "$MAC_VNC,password-secret=leandrosvncpw")
        echo "🖥️  GPU display (virgl on ANGLE, egl-headless) opens in Screen Sharing when QEMU is up"
        echo "🖥️  (LEANDROS_NO_VIEWER=1: no window; --no-gpu: plain cocoa window, no GPU, no COSMIC)"
        # This PID becomes QEMU's (exec below), so wait for *our* listener:
        # another QEMU may already hold the port, and then ours fails to start.
        (
            for _ in $(seq 1 120); do
                kill -0 $$ 2>/dev/null || exit 0
                if lsof -nP -a -p $$ -iTCP:"$MAC_VNC_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
                    open "vnc://:$MAC_VNC_PW@$MAC_VNC_HOST:$MAC_VNC_PORT"
                    exit 0
                fi
                sleep 0.5
            done
        ) </dev/null >/dev/null 2>&1 &
    else
        GL_ARGS=("-display" "egl-headless" "-vnc" "$MAC_VNC")
        echo "🖥️  virgl on ANGLE via egl-headless; no-auth VNC at $MAC_VNC_HOST:$MAC_VNC_PORT"
        echo "🖥️  (Screen Sharing needs a password: run from a terminal to get the auto-viewer)"
        echo "🖥️  (--no-gpu: plain cocoa window, no GPU, no COSMIC)"
    fi
fi
if [ "$VENUS" = "0" ] && [ "$OS" != "Darwin" ] && [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
    if [ "${#GL_ARGS[@]}" -gt 0 ] && $QEMU_SYSTEM -display help 2>/dev/null | grep -q egl-headless; then
        GL_ARGS=("-display" "egl-headless")
        echo "🖥️  Headless host: using egl-headless (GL preserved)"
    else
        GL_ARGS=("-display" "none")
        echo "🖥️  Headless host: using -display none"
    fi
fi

# q35 adds a default std-VGA adapter that would become the primary display, so
# the x86_64 UEFI path suppresses it and lets virtio-vga be the sole device.
# Venus is the one case that wants it back (see below); nothing else changes it.
X86_UEFI_VGA_ARGS=(-vga none)

# Venus needs one specific device line, and every way of getting it wrong fails
# silently rather than loudly — a non-GL device, a -display that gets overridden,
# a QEMU built without virglrenderer, and a macOS host all produce a guest that
# merely reports "no Venus capset". So --venus never autodetects and never
# degrades: it either produces the proven line or refuses with the reason.
if [ "$VENUS" = "1" ]; then
    if [ "$OS" = "Darwin" ] && ! host_venus_possible; then
        echo "❌ --venus: this macOS QEMU has no venus= property (upstream"
        echo "   virglrenderer has no macOS Venus). Use --virgl, or the Linux box."
        exit 1
    fi
    if [ "$BOOT_MODE" = "raspi4b" ]; then
        echo "❌ --venus is meaningless with --raspi4b: that board has no PCI bus and"
        echo "   the raspi4b command line attaches no GPU device at all."
        exit 1
    fi
    if ! $QEMU_SYSTEM -device help 2>&1 | grep -qE 'virtio-gpu-gl-pci|virtio-vga-gl'; then
        echo "❌ --venus needs a GL virtio-gpu device (virtio-vga-gl or"
        echo "   virtio-gpu-gl-pci), and this $QEMU_SYSTEM provides neither (a QEMU"
        echo "   built without virglrenderer). Fix the host QEMU."
        exit 1
    fi
    # -nographic implies -display none and silently wins over any -display
    # earlier on the command line, killing Venus with no diagnostic whatsoever.
    case " ${QEMU_EXTRA_ARGS[*]} " in
        *" -nographic "*)
            echo "❌ --venus is incompatible with -nographic: it implies -display none and"
            echo "   silently overrides the -display this block sets. Drop it — this script"
            echo "   already uses -serial mon:stdio."
            exit 1 ;;
    esac
    # Device: ONE head if the host can give us one.
    #
    # virtio-gpu-gl-pci has no VGA interface, so on x86_64/UEFI it has to be
    # paired with q35's default std-VGA to give OVMF/Limine a GOP. That leaves
    # the VM with TWO display consoles — std-VGA is console 0 and carries the
    # framebuffer text console, the GL device is console 1 and carries whatever
    # cosmic-comp scans out. A working desktop then looks like a black screen,
    # because the window, VNC and screendump all show console 0; you have to
    # switch to View #2 (or pass `screendump -d venusgpu`) to see anything.
    # Worse, under `-display gtk,gl=on` the two consoles fight over EGL contexts
    # and the host spams `Gdk-WARNING: eglMakeCurrent failed` while the UI stalls.
    #
    # virtio-vga-gl is virtio-vga PLUS a virglrenderer context, and it accepts
    # venus=on/blob=on/hostmem= just like virtio-gpu-gl-pci — so it satisfies
    # OVMF and Venus with a single device, one console, and `-vga none` intact.
    # Prefer it; keep the two-device layout only as the fallback for a QEMU that
    # lacks it. aarch64 has no VGA at all, so it always takes the -pci device.
    #
    # hostmem= backs the host-visible blob window Mesa's Venus ring maps.
    if [ "$ARCH" != "aarch64" ] && $QEMU_SYSTEM -device help 2>&1 | grep -q virtio-vga-gl; then
        GPU_DEV="virtio-vga-gl,venus=on,blob=on,hostmem=4G,id=venusgpu"
    else
        GPU_DEV="virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G,id=venusgpu"
        # Only this path drops `-vga none`, and only on x86_64/UEFI, where the
        # GL device cannot give OVMF a GOP by itself.
        [ "$ARCH" = "aarch64" ] || X86_UEFI_VGA_ARGS=()
    fi
    # Display: a window when the host has a display server to open one on,
    # egl-headless otherwise. egl-headless keeps the GL pipeline alive but
    # attaches no window, which is right for an SSH session or a harness and
    # useless when you are trying to look at the desktop. LEANDROS_VENUS_DISPLAY
    # overrides with a literal QEMU -display spec; --venus-headless forces the
    # offscreen path even on a desktop (harnesses that must not open a window).
    if [ -n "${LEANDROS_VENUS_DISPLAY:-}" ]; then
        GL_ARGS=("-display" "$LEANDROS_VENUS_DISPLAY")
    elif [ "$VENUS_HEADLESS" = "0" ] && { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; } \
         && $QEMU_SYSTEM -display help 2>/dev/null | grep -qx gtk; then
        GL_ARGS=("-display" "gtk,gl=on")
    else
        GL_ARGS=("-display" "egl-headless")
    fi
    if [ "$OS" = "Darwin" ] && [ -z "${LEANDROS_VENUS_DISPLAY:-}" ]; then
        GL_ARGS+=("-vnc" "${LEANDROS_VNC:-127.0.0.1:0},display=venusgpu")
    fi
    echo "🌋 Venus: -device $GPU_DEV ${GL_ARGS[*]}"
    if [ "${#X86_UEFI_VGA_ARGS[@]}" -eq 0 ] && [ "$ARCH" != "aarch64" ]; then
        echo "   ⚠ two display consoles (std-VGA + GL): the desktop is on View #2."
    fi
fi


# ── Network backend ─────────────────────────────────────────────────────────
#
# Two mutually exclusive backends, and the choice changes the QEMU command line
# in TWO places (the -netdev argument, and whether QEMU is exec'd directly or
# through a wrapper), so it is decided once, here.
#
#   * vmnet, via socket_vmnet — macOS only. vmnet.framework requires root, and
#     socket_vmnet is the signed/notarized helper daemon that holds that
#     privilege so QEMU need not (see github.com/lima-vm/socket_vmnet). Its
#     client wrapper connects to the daemon's unix socket and hands QEMU the
#     resulting fd as fd 3 — which is the ONLY reason `-netdev socket,fd=3`
#     works. Start the daemon once with `sudo brew services start socket_vmnet`.
#     Guest gets a routable 192.168.105.2 by DHCP.
#
#   * user-mode (SLIRP) — everywhere else: Linux, and a Mac where socket_vmnet
#     is not installed or its daemon is not running. Needs no privilege and no
#     wrapper. Guest gets 10.0.2.15 behind QEMU's NAT, gateway/DNS 10.0.2.2.
#
# Either way the guest configures itself by DHCP (servers/net's smoltcp dhcpv4
# client), so nothing in the guest has to know which one it got. Only inbound
# connections differ: vmnet is reachable from the host, SLIRP needs -netdev
# user,hostfwd=... to expose a port.
#
# Before this, the socket_vmnet wrapper was exec'd unconditionally, so every
# UEFI run on Linux died with a not-found on the hardcoded Homebrew path.
SOCKET_VMNET_CLIENT=""
SOCKET_VMNET_SOCK=""
# LEANDROS_NET=user forces SLIRP on a Mac whose socket_vmnet daemon is up, for
# a run that needs Linux's 10.0.2.x layout (10.0.2.2 does not exist on vmnet).
if [ "$OS" = "Darwin" ] && [ "${LEANDROS_NET:-auto}" != "user" ]; then
    HOMEBREW_PREFIX=$(brew --prefix 2>/dev/null || echo /opt/homebrew)
    _svc="$HOMEBREW_PREFIX/opt/socket_vmnet/bin/socket_vmnet_client"
    _svs="$HOMEBREW_PREFIX/var/run/socket_vmnet"
    if [ -x "$_svc" ] && [ -S "$_svs" ]; then
        SOCKET_VMNET_CLIENT="$_svc"
        SOCKET_VMNET_SOCK="$_svs"
    elif [ -x "$_svc" ]; then
        # Installed but not running: the wrapper would fail to connect and take
        # the whole run down with it. Say why, then fall back.
        echo "⚠️  socket_vmnet installed but its daemon is not running ($_svs missing)"
        echo "    → falling back to user-mode networking; start it with: sudo brew services start socket_vmnet"
    fi
fi

if [ -n "$SOCKET_VMNET_CLIENT" ]; then
    NETDEV_ARGS=(-netdev socket,id=net0,fd=3)
    NET_DESC="vmnet (via socket_vmnet), guest gets 192.168.105.2 via DHCP, host gateway 192.168.105.1"
else
    NETDEV_ARGS=(-netdev user,id=net0)
    NET_DESC="user-mode/SLIRP, guest gets 10.0.2.15 via DHCP, gateway+DNS 10.0.2.2"
fi

# Every QEMU NIC defaults to MAC 52:54:00:12:34:56. Under socket_vmnet all VMs
# share one bridge, so two concurrent LeandrOS guests would DHCP the same
# 192.168.105.2 and each would RST the other's TCP segments (no matching
# socket), killing connections right after the handshake. Derive a MAC that is
# stable per (tree, arch, run id) instead; LEANDROS_MAC overrides it. driver.py
# computes the same value.
if [ -n "${LEANDROS_MAC:-}" ]; then
    NIC_MAC="$LEANDROS_MAC"
else
    _mac_hash=$(printf '%s|%s|%s' "$(cd "$(dirname "$0")/.." && pwd -P)" "$ARCH" "${LEANDROS_RUN_ID:-}" | shasum -a 256 | cut -c1-6)
    NIC_MAC="52:54:00:${_mac_hash:0:2}:${_mac_hash:2:2}:${_mac_hash:4:2}"
fi
NET_DESC="$NET_DESC, MAC $NIC_MAC"

# ── Keep the Mac awake while the VM runs ────────────────────────────────────
# A MacBook on battery idle-sleeps a few minutes after the last user input,
# whatever the CPU load, and a sleeping host freezes the guest, its serial line
# and its display together: from outside that is indistinguishable from a
# guest wedge (the 6 and 8 minute "stalls" of wave 2026-09-24 were the host's
# Idle Sleep periods in `pmset -g log`). Every launch below execs, so this PID
# becomes QEMU's and `caffeinate -w $$` holds PreventUserIdleSystemSleep
# exactly as long as QEMU lives. Display sleep stays allowed.
# LEANDROS_ALLOW_HOST_SLEEP=1 opts out.
if [ "$OS" = "Darwin" ] && [ -z "${LEANDROS_ALLOW_HOST_SLEEP:-}" ] && command -v caffeinate >/dev/null 2>&1; then
    caffeinate -i -w $$ </dev/null >/dev/null 2>&1 &
fi

# ── F2FS data disks (created once, reused across runs) ──────────────────────
DATA0_IMG="f2fs-data0-${ARCH}.img"
DATA1_IMG="f2fs-data1-${ARCH}.img"
for IDX in 0 1; do
    FDISK="f2fs-data${IDX}-${ARCH}.img"
    if [ ! -f "$FDISK" ]; then
        echo "Creating $FDISK (64 MB)..."
        dd if=/dev/zero of="$FDISK" bs=1M count=64 2>/dev/null
        if command -v mkfs.f2fs &>/dev/null; then
            mkfs.f2fs -f -O "^extra_attr,^inline_data,^inline_dentry" "$FDISK"
        elif command -v python3 &>/dev/null; then
            python3 "$(dirname "$0")/mkfs-f2fs-minimal.py" "$FDISK"
        else
            echo "WARNING: neither mkfs.f2fs nor python3 found — $FDISK is blank"
        fi
    fi
done

# ── QMP socket (TODO.md item 18 gap 1) ──────────────────────────────────────
# HMP (this script's `-serial mon:stdio`) can't hold a chord — `sendkey`
# presses and releases a scancode in one shot, so Ctrl+Alt+Fn (the VT-switch
# combo) has no HMP equivalent — and HMP `mouse_move` is relative while our
# virtio-tablet is absolute-only, so it's silently dropped. A permanent QMP
# endpoint (mirroring driver.py's) fixes both. Unique per run (arch + this
# shell's pid) so two QEMUs never collide on one socket. Skip adding our own
# if the caller already passed one via extra args (e.g. `-d -qmp ...`).
QMP_ARGS=()
case " ${QEMU_EXTRA_ARGS[*]} " in
    *" -qmp "*) ;;  # caller already set one — don't add a second
    *)
        QMP_SOCK="/tmp/leandros-qmp-${ARCH}-$$.sock"
        rm -f "$QMP_SOCK"
        QMP_ARGS=(-qmp "unix:${QMP_SOCK},server=on,wait=off")
        echo "🔌 QMP: unix:${QMP_SOCK}"
        ;;
esac

echo "🚀 Starting LeandrOS ($ARCH) in $BOOT_MODE mode"
echo "=========================================="
if [ "$BOOT_MODE" = "uefi" ]; then
    echo "🌐 Network: $NET_DESC"
fi

if [ "$BOOT_MODE" = "raspi4b" ]; then
    KERNEL_ELF="target/final-aarch64/kernel-direct"
    if [ ! -f "$KERNEL_ELF" ]; then
        echo "❌ Direct kernel ELF not found: $KERNEL_ELF (build with: ./scripts/build-all.sh --arch aarch64 --raspi4b)"
        exit 1
    fi
    echo "🏗️  Using Direct Kernel ELF: $KERNEL_ELF (QEMU raspi4b — sdhci driver test path)"

    # No PCI bus exists on raspi4b (confirmed via QMP `info mtree`), so the
    # F2FS test image attaches through the SD card slot instead of
    # virtio-blk-pci. QEMU routes `-drive if=sd` to the second of two
    # generic-sdhci instances (0xfe340000), matching SDHCI_BASE in
    # drivers/src/sdhci.rs for this feature. No GPU/sound/keyboard devices
    # exist on this board — verification is serial-log only.
    # -accel tcg: force software emulation, matching the other direct-boot
    # paths below (avoids any host-acceleration mismatch with the new
    # EL3->EL2 boot prologue in kernel/src/entry_aarch64.s).
    exec $QEMU_SYSTEM $MACHINE_ARGS -accel tcg \
        -kernel "$KERNEL_ELF" \
        -device loader,file=initrd-aarch64.cpio,addr=0x48000000,force-raw=on \
        -drive if=sd,format=raw,file="$DATA0_IMG" \
        -net none \
        -serial mon:stdio \
        -parallel none \
        -no-reboot \
        "${QMP_ARGS[@]}" \
        "${QEMU_EXTRA_ARGS[@]}"
elif [ "$BOOT_MODE" = "uefi" ]; then
    # A guest reboot (reboot(2) -> ACPI RESET_REG / PSCI SYSTEM_RESET) restarts
    # the VM through the firmware, as on real hardware, and a guest power-off
    # (ACPI S5 / PSCI SYSTEM_OFF) ends QEMU. LEANDROS_NO_REBOOT=1 brings back
    # -no-reboot (QEMU exits on any guest reset, a triple fault included).
    REBOOT_ARGS=()
    if [ "${LEANDROS_NO_REBOOT:-0}" = "1" ]; then REBOOT_ARGS=(-no-reboot); fi
    UEFI_FIRMWARE=""
    FW_PATHS=("${X86_64_FW_PATHS[@]}")
    if [ "$ARCH" = "aarch64" ]; then FW_PATHS=("${AARCH64_FW_PATHS[@]}"); fi
    for path in "${FW_PATHS[@]}"; do if [ -f "$path" ]; then UEFI_FIRMWARE="$path"; break; fi; done
    if [ -z "$UEFI_FIRMWARE" ]; then echo "❌ UEFI firmware not found"; exit 1; fi
    
    # Locate a writable VARS template matching the firmware we picked.
    VARS_TEMPLATE=""
    VARS_PATHS=("${X86_64_VARS_PATHS[@]}")
    if [ "$ARCH" = "aarch64" ]; then VARS_PATHS=("${AARCH64_VARS_PATHS[@]}"); fi
    for path in "${VARS_PATHS[@]}"; do if [ -f "$path" ]; then VARS_TEMPLATE="$path"; break; fi; done

    AUDIO_ARGS=$(select_audio_args)

    if [ "$ARCH" = "aarch64" ]; then
        VARS_FILE="aarch64_vars.fd"
        if [ ! -f "$VARS_FILE" ]; then
            # Copy the host's VARS template; fall back to a blank 64 MB region
            # (edk2 will initialise it on first boot).
            cp "$VARS_TEMPLATE" "$VARS_FILE" 2>/dev/null || dd if=/dev/zero of="$VARS_FILE" bs=1M count=64
        fi

        # disable-legacy=on forces non-transitional (modern) VirtIO for block
        # devices.  Transitional devices (0x1001) trigger a QEMU 10.x deadlock
        # in the doorbell write handler on the virt machine because the new
        # coroutine-based block I/O path needs the iothread event loop — which
        # cannot run while inside the MMIO write handler.  Modern non-
        # transitional devices use a different notification path that doesn't
        # have this issue.
        QEMU_ARGS=($MACHINE_ARGS $CPU_ARGS -m ${LEANDROS_QEMU_MEM:-2G} -boot menu=on,splash-time=0 -serial mon:stdio -parallel none \
            -drive if=pflash,unit=0,format=raw,readonly=on,file="$UEFI_FIRMWARE" \
            -drive if=pflash,unit=1,format=raw,file="$VARS_FILE" \
            -drive if=none,id=drive0,format=raw,file="$DISK_IMAGE" \
            -device virtio-blk-pci,drive=drive0,bootindex=0,disable-legacy=on \
            -drive if=none,id=data0,format=raw,file="$DATA0_IMG" \
            -device virtio-blk-pci,drive=data0,disable-legacy=on \
            -drive if=none,id=data1,format=raw,file="$DATA1_IMG" \
            -device virtio-blk-pci,drive=data1,disable-legacy=on \
            -device "$GPU_DEV" \
            -device virtio-keyboard-pci \
            -device virtio-tablet-pci \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -device virtio-net-pci,netdev=net0,disable-legacy=on,mac="$NIC_MAC" "${NETDEV_ARGS[@]}" "${REBOOT_ARGS[@]}" \
            "${QMP_ARGS[@]}")
    else
        # A split firmware (OVMF_CODE*) is read-only and needs its writable VARS
        # half as a second pflash unit; a combined image (OVMF.fd) does not.
        # Arch ships only the split pair, which is why this is not optional.
        X86_VARS_ARGS=()
        if [[ "$(basename "$UEFI_FIRMWARE")" == *CODE* ]] && [ -n "$VARS_TEMPLATE" ]; then
            X86_VARS_FILE="x86_64_vars.fd"
            if [ ! -f "$X86_VARS_FILE" ]; then cp "$VARS_TEMPLATE" "$X86_VARS_FILE"; fi
            X86_VARS_ARGS=(-drive "if=pflash,unit=1,format=raw,file=$X86_VARS_FILE")
        fi
        QEMU_ARGS=($MACHINE_ARGS $CPU_ARGS -m ${LEANDROS_QEMU_MEM:-2G} -boot menu=on,splash-time=0 -serial mon:stdio -parallel none \
            -drive if=pflash,unit=0,format=raw,readonly=on,file="$UEFI_FIRMWARE" \
            "${X86_VARS_ARGS[@]}" \
            -drive if=none,id=drive0,format=raw,file="$DISK_IMAGE" \
            -device virtio-blk-pci,drive=drive0,bootindex=0 \
            -drive if=none,id=data0,format=raw,file="$DATA0_IMG" \
            -device virtio-blk-pci,drive=data0 \
            -drive if=none,id=data1,format=raw,file="$DATA1_IMG" \
            -device virtio-blk-pci,drive=data1 \
            "${X86_UEFI_VGA_ARGS[@]}" -device "$GPU_DEV" \
            -device virtio-keyboard-pci \
            -device virtio-tablet-pci \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -device virtio-net-pci,netdev=net0,mac="$NIC_MAC" "${NETDEV_ARGS[@]}" "${REBOOT_ARGS[@]}" \
            "${QMP_ARGS[@]}")

    fi
    # The vmnet backend selected above is `-netdev socket,fd=3`, and that fd only
    # exists when QEMU is launched *through* socket_vmnet's client wrapper — so
    # the wrapper is part of the netdev choice, not an unconditional prefix. The
    # SLIRP backend needs no wrapper and no privilege, so it execs QEMU directly.
    if [ -n "$SOCKET_VMNET_CLIENT" ]; then
        exec "$SOCKET_VMNET_CLIENT" "$SOCKET_VMNET_SOCK" \
            $QEMU_SYSTEM "${QEMU_ARGS[@]}" "${QEMU_EXTRA_ARGS[@]}"
    else
        exec $QEMU_SYSTEM "${QEMU_ARGS[@]}" "${QEMU_EXTRA_ARGS[@]}"
    fi
else
    AUDIO_ARGS=$(select_audio_args)

    if [ "$ARCH" = "aarch64" ]; then
        # Use ELF for AArch64
        KERNEL_ELF="target/final-aarch64/kernel-direct"
        if [ ! -f "$KERNEL_ELF" ]; then echo "❌ Direct kernel ELF not found: $KERNEL_ELF"; exit 1; fi
        echo "🏗️  Using Direct Kernel ELF: $KERNEL_ELF"
        
        # QEMU's -initrd is part of the Linux boot protocol and is NOT loaded
        # for a bare ELF entered at its own entry point. Place the initrd at a
        # fixed physical address with -device loader instead; the kernel scans
        # RAM for the CPIO 070701 magic and finds it there. 0x48000000 is well
        # clear of the kernel image at 0x40080000.
        exec $QEMU_SYSTEM $MACHINE_ARGS -cpu max -accel tcg \
            -kernel "$KERNEL_ELF" \
            -device loader,file=initrd-aarch64.cpio,addr=0x48000000,force-raw=on \
            -drive if=none,id=data0,format=raw,file="$DATA0_IMG" \
            -device virtio-blk-pci,drive=data0,disable-legacy=on \
            -drive if=none,id=data1,format=raw,file="$DATA1_IMG" \
            -device virtio-blk-pci,drive=data1,disable-legacy=on \
            -device "$GPU_DEV" \
            -device virtio-keyboard-pci \
            -device virtio-tablet-pci \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -net none \
            -serial mon:stdio \
            -parallel none \
            -no-reboot \
            "${QMP_ARGS[@]}" \
            "${QEMU_EXTRA_ARGS[@]}"
    else
        # Use 32-bit ELF for x86_64 (PVH/Multiboot)
        KERNEL_ELF="target/final-x86_64/kernel-direct-32.elf"
        if [ ! -f "$KERNEL_ELF" ]; then 
            # Fallback to standard name if 32-bit specific one is missing
            KERNEL_ELF="target/final-x86_64/kernel-direct"
        fi
        if [ ! -f "$KERNEL_ELF" ]; then echo "❌ Direct kernel ELF not found: $KERNEL_ELF"; exit 1; fi
        echo "🏗️  Using Direct Kernel ELF: $KERNEL_ELF"
        
        # As on aarch64 direct boot, the kernel locates the initrd by scanning
        # for the CPIO magic; place it at a fixed physical address with
        # -device loader. 0x1000_0000 (256 MiB) is clear of the kernel image at
        # 0x10_0000 and within the trampoline's low-2 GiB HHDM window.
        # -vga none: q35 otherwise adds a default std VGA adapter that becomes
        # the primary display (showing only SeaBIOS), leaving the kernel's
        # VirtIO-GPU console on a secondary, unseen head.  Disabling it makes
        # VirtIO-GPU the sole display — matching the UEFI path above.
        exec $QEMU_SYSTEM $MACHINE_ARGS -cpu max -accel tcg -m 2G \
            -kernel "$KERNEL_ELF" \
            -device loader,file=initrd-x86_64.cpio,addr=0x10000000,force-raw=on \
            -drive if=none,id=data0,format=raw,file="$DATA0_IMG" \
            -device virtio-blk-pci,drive=data0 \
            -drive if=none,id=data1,format=raw,file="$DATA1_IMG" \
            -device virtio-blk-pci,drive=data1 \
            -vga none -device "$GPU_DEV" \
            -device virtio-keyboard-pci \
            -device virtio-tablet-pci \
            "${GL_ARGS[@]}" \
            -device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on $AUDIO_ARGS \
            -net none \
            -serial mon:stdio \
            -no-reboot \
            "${QMP_ARGS[@]}" \
            "${QEMU_EXTRA_ARGS[@]}"
    fi
fi

