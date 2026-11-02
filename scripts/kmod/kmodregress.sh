#!/usr/bin/env bash
# kmodregress.sh <arch> [unload|e1000e|autoload|ctl]: KMOD_TEST-only module regress, one build per group.
# Standalone like autoloadtest.sh: NOT part of the default all-Y regress (a stock build has no =m drivers).
# Needs an untracked .config with CONFIG_MODULES=y and the =m drivers (virtio_rng virtio_net usb_hid usb_storage e1000e virtio_keyboard virtio_snd hda).
set -uo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)"
arch="${1:-aarch64}"; what="${2:-all}"; rc=0
export FFSESSION_OUT="${FFSESSION_OUT:-/tmp/ffsession-$(basename "$PWD")}"
export KMOD_TEST=1 KMOD_EXTRA="${KMOD_EXTRA:-hello kmbad_hang kmbad_reload kmbad_thread kmbad_freectx kmbad_leak virtio_rng virtio_net usb_hid usb_storage e1000e virtio_keyboard virtio_snd hda}"
build() { "$PWD/scripts/build-all.sh" --arch "$arch" > "${TMPDIR:-/tmp}/kmodregress-build.log" 2>&1 || { tail -n 20 "${TMPDIR:-/tmp}/kmodregress-build.log"; exit 1; }; }
build
RT=.claude/skills/run-leandros/runtests.py
[[ "$what" == all || "$what" == unload ]] && { UPPER=1 EXIT3=1 HID=1 scripts/kmod/unloadtest.sh "$arch" || rc=1; }
[[ "$arch" == x86_64 && ( "$what" == all || "$what" == e1000e ) ]] && { scripts/kmod/e1000etest.sh || rc=1; }
[[ "$what" == all || "$what" == ctl ]] && { python3 "$RT" "$arch" kmodctl --suite kmodctl || rc=1; }
# autoload goes LAST: autoloadtest.sh writes its own .config and does its OWN stock build (modules found only through
# modules.alias, no modules.load), so it must not inherit KMOD_TEST/KMOD_EXTRA, and it replaces the KMOD_TEST images
# the groups above ran on. Two builds are inherent: the KMOD_TEST build above, then this stock one.
[[ "$what" == all || "$what" == autoload ]] && { env -u KMOD_TEST -u KMOD_EXTRA scripts/kmod/autoloadtest.sh "$arch" virtio_rng || rc=1; }
exit $rc
