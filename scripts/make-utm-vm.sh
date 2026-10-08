#!/bin/bash
# Build (or refresh) a UTM virtual machine that boots LeandrOS aarch64.
#
# UTM is a QEMU front end, so the VM is the run-qemu.sh aarch64/UEFI line
# expressed as a `.utm` bundle: a directory holding `config.plist` plus a
# `Data/` folder with the disk images. Run it once to create the VM, and again
# after every `./scripts/build-all.sh` to push the fresh images into it.
#
# What differs from run-qemu.sh, and why:
#
#   * UTM and its QEMUHelper XPC service are App-Sandboxed. The helper can only
#     open files it was handed a security-scoped bookmark for, and UTM creates
#     those for entries in the `Drive` array — never for a path typed into
#     "Additional Arguments". So the images are copied into `Data/` and declared
#     as drives. Each drive uses `Interface = None`, which makes UTM emit only
#     the `-drive if=none,id=drive<ID>,...` half; the `-device virtio-blk-pci`
#     half is ours, so it can carry `disable-legacy=on` (UTM's own VirtIO
#     interface omits it, and a transitional virtio-blk deadlocks QEMU 10.x on
#     the virt board — see the comment in run-qemu.sh; UTM 4.7 bundles 10.0.2).
#
#   * Sound must be ours too: virtio-snd is not in QEMU's legacy-allowed table,
#     so UTM's bare `virtio-sound-pci` line fails with "device is modern-only".
#     That leaves UTM's `Sound` array empty, and UTM only opens its SPICE
#     playback channel when that array is non-empty
#     (UTMQemuVirtualMachine.swift: `if !config.sound.isEmpty { .hasAudio }`),
#     so pointing the device at UTM's SPICE audiodev gives silence. The device
#     therefore uses its own CoreAudio audiodev — the same backend run-qemu.sh
#     uses — which bypasses SPICE and UTM's mute button entirely.
#
#   * Input is virtio-keyboard/virtio-tablet as in run-qemu.sh; UTM's USB
#     support is disabled so it does not add an xHCI controller plus USB
#     keyboard/tablet/mouse the kernel has no drivers for.
#
#   * Network stays UTM-native (Emulated = SLIRP by default): the kernel's
#     virtio-net driver accepts the transitional device UTM generates, and
#     keeping the entry lets the mode (Shared/Bridged) and port forwards be
#     changed in UTM's UI.
#
#   * HVF via UTM's Hypervisor toggle (-cpu host -accel hvf) with the machine
#     forced to gic-version=3, matching what run-qemu.sh runs on this Mac. The
#     kernel detects GICv2 or GICv3 at boot either way.
#
#   * The display device is plain virtio-gpu-pci (softpipe path). UTM bundles
#     virglrenderer + ANGLE, so virtio-gpu-gl-pci is worth a try on this host
#     (Homebrew QEMU has no EGL), but it is the untested virgl desktop path.
#     Pass --gl to opt in.
#
#   * The serial console goes to UTM's built-in terminal window by default.
#     --serial-tcp PORT makes it a TCP server on 127.0.0.1 instead (wait=on, so
#     the guest does not boot until a client connects and no early output is
#     lost) — that is what a script wants: `nc localhost PORT` is the console.
#
# Usage: scripts/make-utm-vm.sh [--name NAME] [--dest DIR] [--gl] [--mem MiB] [--cpus N]
#                               [--serial-tcp PORT] [--debug-log]

set -euo pipefail

cd "$(dirname "$0")/.."

NAME="LeandrOS aarch64"
DEST="$HOME/Library/Containers/com.utmapp.UTM/Data/Documents"
GPU_DEV="virtio-gpu-pci"
MEM_MIB=2048
CPUS=4
SERIAL_TCP_PORT=""
DEBUG_LOG=false
UTM_APP="/Applications/UTM.app"

while [[ "$#" -gt 0 ]]; do
    case $1 in
        --name) NAME="$2"; shift 2 ;;
        --dest) DEST="$2"; shift 2 ;;
        --gl)   GPU_DEV="virtio-gpu-gl-pci"; shift ;;
        --mem)  MEM_MIB="$2"; shift 2 ;;
        --cpus) CPUS="$2"; shift 2 ;;
        --serial-tcp) SERIAL_TCP_PORT="$2"; shift 2 ;;
        --debug-log) DEBUG_LOG=true; shift ;;
        -h|--help) sed -n '2,52p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

if [ "$(uname -s)" != "Darwin" ]; then echo "❌ UTM is macOS-only" >&2; exit 1; fi
if [ ! -d "$UTM_APP" ]; then echo "❌ $UTM_APP not found" >&2; exit 1; fi
UTMCTL="$UTM_APP/Contents/MacOS/utmctl"

DISK_IMAGE="leandros-limine-aarch64.img"
DATA0_IMG="f2fs-data0-aarch64.img"
DATA1_IMG="f2fs-data1-aarch64.img"
for f in "$DISK_IMAGE" "$DATA0_IMG" "$DATA1_IMG"; do
    if [ ! -f "$f" ]; then
        echo "❌ $f missing — build first: ./scripts/build-all.sh --arch aarch64 (data disks: ./scripts/run-qemu.sh aarch64)" >&2
        exit 1
    fi
done

# UTM's own edk2 pair lives in the app bundle; QEMU gets the code image from
# UTM's -L resource dir and the VARS copy from Data/efi_vars.fd. Seed the copy
# ourselves so the first launch does not depend on UTM's save path having run.
VARS_TEMPLATE="$UTM_APP/Contents/Resources/qemu/edk2-arm-vars.fd"
if [ ! -f "$VARS_TEMPLATE" ]; then echo "❌ $VARS_TEMPLATE missing" >&2; exit 1; fi

BUNDLE="$DEST/$(echo "$NAME" | tr ' /' '--').utm"
DATA="$BUNDLE/Data"
PLIST="$BUNDLE/config.plist"

# Refreshing the images under a running VM would corrupt its view of them.
if "$UTMCTL" status "$NAME" 2>/dev/null | grep -qiE 'started|starting|pausing|paused|resuming'; then
    echo "❌ '$NAME' is running — stop it first: \"$UTMCTL\" stop \"$NAME\"" >&2
    exit 1
fi

# Keep the UUID stable across regenerations so UTM treats it as the same VM.
UUID=""
if [ -f "$PLIST" ]; then
    UUID=$(/usr/libexec/PlistBuddy -c 'Print :Information:UUID' "$PLIST" 2>/dev/null || true)
fi
[ -n "$UUID" ] || UUID=$(uuidgen)

mkdir -p "$DATA"

# cp -c is an APFS clone: instant, and shares blocks until the guest writes.
copy_image() {
    rm -f "$DATA/$1"
    cp -c "$1" "$DATA/$1" 2>/dev/null || cp "$1" "$DATA/$1"
}
echo "📦 Copying images into $DATA"
copy_image "$DISK_IMAGE"
copy_image "$DATA0_IMG"
copy_image "$DATA1_IMG"
if [ ! -f "$DATA/efi_vars.fd" ]; then
    cp "$VARS_TEMPLATE" "$DATA/efi_vars.fd"
    chmod 644 "$DATA/efi_vars.fd"
fi

# Locally administered, unicast MAC (x2:...), derived from the UUID so it is
# stable for DHCP leases yet unique per VM.
MAC=$(printf '%s' "$UUID" | shasum | awk '{print $1}' | sed -E 's/^(..)(..)(..)(..)(..).*/52:\1:\2:\3:\4:\5/')

# Drive identifiers become QEMU ids as "drive<Identifier>"; keep them plain.
plist_arg() { printf '\t\t\t<string>%s</string>\n' "$1"; }

# The Terminal dict is optional to UTM's decoder but not to its terminal window:
# VMDisplayQemuTerminalWindowController does `serialConfig!.terminal!`, so a
# built-in-terminal serial without it crashes UTM (SIGTRAP) the moment the VM
# starts — which from outside looks like the VM silently stopping.
if [ -n "$SERIAL_TCP_PORT" ]; then
    SERIAL_PLIST=$(cat <<EOS
			<key>Mode</key>
			<string>TcpServer</string>
			<key>Target</key>
			<string>Auto</string>
			<key>TcpPort</key>
			<integer>$SERIAL_TCP_PORT</integer>
			<key>WaitForConnection</key>
			<true/>
			<key>RemoteConnectionAllowed</key>
			<false/>
EOS
)
else
    SERIAL_PLIST=$(cat <<EOS
			<key>Mode</key>
			<string>Terminal</string>
			<key>Target</key>
			<string>Auto</string>
			<key>Terminal</key>
			<dict>
				<key>BackgroundColor</key>
				<string>#000000</string>
				<key>CursorBlink</key>
				<true/>
				<key>Font</key>
				<string>Menlo</string>
				<key>FontSize</key>
				<integer>12</integer>
				<key>ForegroundColor</key>
				<string>#ffffff</string>
				<key>Theme</key>
				<string>Default</string>
			</dict>
EOS
)
fi

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Backend</key>
	<string>QEMU</string>
	<key>ConfigurationVersion</key>
	<integer>4</integer>
	<key>Information</key>
	<dict>
		<key>Icon</key>
		<string>linux</string>
		<key>IconCustom</key>
		<false/>
		<key>Name</key>
		<string>$NAME</string>
		<key>Notes</key>
		<string>LeandrOS aarch64 (UEFI/Limine). Generated by scripts/make-utm-vm.sh — rerun it after ./scripts/build-all.sh to refresh the disk images. Login root/root or leandro/leandro.</string>
		<key>UUID</key>
		<string>$UUID</string>
	</dict>
	<key>System</key>
	<dict>
		<key>Architecture</key>
		<string>aarch64</string>
		<key>Target</key>
		<string>virt</string>
		<key>CPU</key>
		<string>default</string>
		<key>CPUCount</key>
		<integer>$CPUS</integer>
		<key>CPUFlagsAdd</key>
		<array/>
		<key>CPUFlagsRemove</key>
		<array/>
		<key>ForceMulticore</key>
		<false/>
		<key>JITCacheSize</key>
		<integer>0</integer>
		<key>MemorySize</key>
		<integer>$MEM_MIB</integer>
	</dict>
	<key>QEMU</key>
	<dict>
		<key>AdditionalArguments</key>
		<array>
$(plist_arg "-device virtio-blk-pci,drive=driveleandros,bootindex=0,disable-legacy=on")
$(plist_arg "-device virtio-blk-pci,drive=drivedata0,disable-legacy=on")
$(plist_arg "-device virtio-blk-pci,drive=drivedata1,disable-legacy=on")
$(plist_arg "-device virtio-keyboard-pci")
$(plist_arg "-device virtio-tablet-pci")
$(plist_arg "-audiodev coreaudio,id=snd0")
$(plist_arg "-device virtio-sound-pci,audiodev=snd0,streams=1,disable-legacy=on")
$(plist_arg "-no-reboot")
		</array>
		<key>BalloonDevice</key>
		<false/>
		<key>DebugLog</key>
		<$DEBUG_LOG/>
		<key>Hypervisor</key>
		<true/>
		<key>MachinePropertyOverride</key>
		<string>gic-version=3</string>
		<key>PS2Controller</key>
		<false/>
		<key>RNGDevice</key>
		<false/>
		<key>RTCLocalTime</key>
		<false/>
		<key>TPMDevice</key>
		<false/>
		<key>TSO</key>
		<false/>
		<key>UEFIBoot</key>
		<true/>
	</dict>
	<key>Display</key>
	<array>
		<dict>
			<key>DownscalingFilter</key>
			<string>Linear</string>
			<key>DynamicResolution</key>
			<false/>
			<key>Hardware</key>
			<string>$GPU_DEV</string>
			<key>NativeResolution</key>
			<false/>
			<key>UpscalingFilter</key>
			<string>Nearest</string>
		</dict>
	</array>
	<key>Drive</key>
	<array>
		<dict>
			<key>Identifier</key>
			<string>leandros</string>
			<key>ImageName</key>
			<string>$DISK_IMAGE</string>
			<key>ImageType</key>
			<string>Disk</string>
			<key>Interface</key>
			<string>None</string>
			<key>InterfaceVersion</key>
			<integer>1</integer>
			<key>ReadOnly</key>
			<false/>
		</dict>
		<dict>
			<key>Identifier</key>
			<string>data0</string>
			<key>ImageName</key>
			<string>$DATA0_IMG</string>
			<key>ImageType</key>
			<string>Disk</string>
			<key>Interface</key>
			<string>None</string>
			<key>InterfaceVersion</key>
			<integer>1</integer>
			<key>ReadOnly</key>
			<false/>
		</dict>
		<dict>
			<key>Identifier</key>
			<string>data1</string>
			<key>ImageName</key>
			<string>$DATA1_IMG</string>
			<key>ImageType</key>
			<string>Disk</string>
			<key>Interface</key>
			<string>None</string>
			<key>InterfaceVersion</key>
			<integer>1</integer>
			<key>ReadOnly</key>
			<false/>
		</dict>
	</array>
	<key>Input</key>
	<dict>
		<key>MaximumUsbShare</key>
		<integer>3</integer>
		<key>UsbBusSupport</key>
		<string>Disabled</string>
		<key>UsbSharing</key>
		<false/>
	</dict>
	<key>Network</key>
	<array>
		<dict>
			<key>Hardware</key>
			<string>virtio-net-pci</string>
			<key>IsolateFromHost</key>
			<false/>
			<key>MacAddress</key>
			<string>$MAC</string>
			<key>Mode</key>
			<string>Emulated</string>
			<key>PortForward</key>
			<array/>
		</dict>
	</array>
	<key>Serial</key>
	<array>
		<dict>
$SERIAL_PLIST
		</dict>
	</array>
	<key>Sharing</key>
	<dict>
		<key>ClipboardSharing</key>
		<false/>
		<key>DirectoryShareMode</key>
		<string>None</string>
		<key>DirectoryShareReadOnly</key>
		<false/>
	</dict>
	<key>Sound</key>
	<array/>
</dict>
</plist>
EOF

plutil -lint "$PLIST" >/dev/null

echo "✅ $BUNDLE"
echo "   GPU $GPU_DEV · ${MEM_MIB} MiB · $CPUS vCPU · HVF · gic-version=3 · MAC $MAC"
if [ -n "$SERIAL_TCP_PORT" ]; then
    echo "   Serial: TCP server 127.0.0.1:$SERIAL_TCP_PORT (boot waits for a client: nc localhost $SERIAL_TCP_PORT)"
else
    echo "   Serial: UTM built-in terminal"
fi
echo "   ⚠ UTM caches config.plist while running — quit and reopen UTM after regenerating."
echo "   Start:  open -a UTM   (the VM appears in the sidebar)   or   \"$UTMCTL\" start \"$NAME\""
