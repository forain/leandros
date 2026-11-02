# Laptop module-unload trial — 2026-11-01/02

Trial kernel `dc4e6a5cba0e` (`#1 lapunload`, main 8eb1533b, laptop defconfig +
`MODULES=y`, `=m` for HDA, E1000E, USB_HID, USB_VIDEO_CLASS). One-shot trial,
never promoted; good stays `aa47abc412a5`.

## Boot path (dual-boot NVMe)

The firmware boots systemd-boot, whose default is EndeavourOS. `remote.py
deploy-kernel` / `espctl arm` only arm the trial inside Limine, so reaching
LeandrOS needs a one-shot from Linux:

    ssh forain@<laptop> 'sudo -n bootctl set-oneshot leandros.conf && sudo -n systemctl reboot'

LeandrOS has no Wi-Fi link on this install; use wired eth0 (192.168.2.5).

## Results (real hardware)

| module | unload/reload | notes |
|---|---|---|
| uvc | 10/10 | `rmmod` refused (rc 1) while `/dev/video0` is open |
| hda | 10/10 | 34 DMA pages freed per unload; `rmmod` refused while `/dev/pipewire` is open |
| usb_hid | skipped | not loaded (no USB keyboard attached) |
| e1000e | **machine lost** | after the detached loop started, the laptop answered on neither address; netwatch did not reboot it either, so the kernel most likely hung during an e1000e unload or reload on the real I219. QEMU e1000e passes 20 rounds under a ping flood. Needs the previous-boot klog after a power cycle. |

## Caveats found

- The NVMe root has no `/lib/modules` (modules live in the initrd only), so a
  manual reload needs the `.kmod` pushed somewhere (`/root/km`).
- Over `remote.py run`, a `( ... ) &` subshell dies with the session even with
  `trap '' HUP`; `sh -c '...' > f 2>&1 < /dev/null &` survives. The guest has
  no `setsid`/`nohup`.

## Follow-ups

- e1000e reload hang on I219: fetch klog, diagnose (deep-reasoner), fix (fast-worker).
- `deploy-kernel` should set the systemd-boot one-shot itself on a dual-boot ESP.
- Keep initrd modules reachable for manual reload on a disk root.

## Merged-batch regress (main 8eb1533b)

x86_64 66/0 bad; aarch64 66 with 1 bad (futextest `pi_handoff`, stolen=true),
which then passed 3/3 alone: a TCG timing flake. An earlier wedged run was the
box's `/tmp` tmpfs at 100 % (QEMU pauses guests on ENOSPC), not a kernel hang.
