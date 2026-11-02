#!/usr/bin/env bash
# unloadtest.sh [arch]: kmod unload (lane U1 subset). Needs a build with KMOD_EXTRA="hello kmbad_hang kmbad_reload virtio_rng" (U2 adds kmbad_thread kmbad_freectx virtio_net).
# hello load/unload loop, -ENOENT / -EBUSY / -EPERM, kmbad_hang Zombie, kmbad_reload stale-gate proof (kmod.leaktick=1).
set -uo pipefail
cd "$(cd "$(dirname "$0")/../.." && pwd)"
arch="${1:-aarch64}"; N="${UNLOAD_N:-50}"
RT=.claude/skills/run-leandros/runtests.py
export FFSESSION_OUT="${FFSESSION_OUT:-/tmp/ffsession-$(basename "$PWD")}"
# Guard: every module a selected section loads must be =m in the build's .config, else a built-in
# driver makes the section fail misleadingly.
need="VIRTIO_RNG VIRTIO_NET"; [ -n "${UPPER:-}" ] && need="$need USB_HID USB_STORAGE"
for c in $need; do
  grep -q "^CONFIG_$c=m" .config 2>/dev/null || { echo "CONFIG_$c is not =m in .config; this run would give misleading FAILs (fix .config and rebuild)" >&2; exit 2; }
done
# Guard: refuse to test a stale image or a half-written one.
if ps aux | grep "[b]uild-all.sh" | grep -q "$PWD" >/dev/null; then echo "build-all running in this worktree; wait" >&2; exit 2; fi
img=leandros-limine-$arch.img
if [ -n "$img" ]; then for e in target/kmod/$arch/*.elf; do [ -e "$e" ] && [ "$e" -nt "$img" ] && { echo "image $img older than $e; rebuild" >&2; exit 2; }; done; fi
export LEANDROS_RUN_ID="unloadtest-$arch"
M='$MD'  # guest var: the lib/modules/<id> dir whose build-id this kernel accepts (there can be several)
load() { echo "kmodctl load ${M}$1.kmod"; }
python3 "$RT" "$arch" ul1 --cmdline "kmod.leaktick=1 kmod.stage=1" \
  "for d in /lib/modules/*/; do if kmodctl load \${d}hello.kmod >/dev/null 2>&1; then kmodctl rmmod hello >/dev/null 2>&1; MD=\${d%/}; break; fi; done; echo MD=\$MD" \
  "for i in \$(seq 1 $N); do kmodctl load $M/hello.kmod >/dev/null 2>&1 || echo LOADFAIL \$i; kmodctl rmmod hello >/dev/null 2>&1 || echo UNLOADFAIL \$i; done; echo HELLOLOOP_DONE" \
  "kmodctl rmmod hello; echo ENOENT_RC=\$?" \
  "kmodctl load $M/kmbad_hang.kmod; kmodctl rmmod kmbad_hang; echo HANGRM_RC=\$?; kmodctl load $M/kmbad_hang.kmod; echo HANGRELOAD_RC=\$?; cat /proc/modules" \
  "kmodctl load $M/kmbad_reload.kmod; sleep 1; kmodctl rmmod kmbad_reload; kmodctl load $M/kmbad_reload.kmod; sleep 1; kmodctl rmmod kmbad_reload; echo RELOAD_DONE" \
  "o=\$(kmodctl rmmod i915 2>&1); echo NOEXIT_RC=\$? OUT=\$o" \
  "kmodctl load $M/kmbad_thread.kmod; sleep 1; kmodctl rmmod kmbad_thread; echo THREADRM_RC=\$?; kmodctl load $M/kmbad_freectx.kmod; sleep 1; kmodctl rmmod kmbad_freectx; echo FREECTXRM_RC=\$?; sleep 1; echo BAD_DONE" \
  "kmodctl rmmod virtio_rng; kmodctl rmmod virtio_net; for i in \$(seq 1 $N); do kmodctl load $M/virtio_rng.kmod >/dev/null 2>&1 || echo RNGLOADFAIL \$i; kmodctl rmmod virtio_rng >/dev/null 2>&1 || echo RNGUNLOADFAIL \$i; done; echo RNGLOOP_DONE" \
  "(ping -c 100000 -i 1 192.168.105.1 >/dev/null 2>&1 &); for i in \$(seq 1 ${NET_N:-20}); do kmodctl load $M/virtio_net.kmod >/dev/null 2>&1 || echo NETLOADFAIL \$i; sleep 4; echo NETRELOAD_OK \$i; echo NETCNT \$i \$(grep eth0 /proc/net/dev);  kmodctl rmmod virtio_net >/dev/null 2>&1 || echo NETUNLOADFAIL \$i; done; echo NETLOOP_DONE" \
  "kmodctl load hello; echo NAMELOAD_RC=\$?; kmodctl rmmod hello; kmodctl load hello.kmod; echo NAMELOAD2_RC=\$?; kmodctl rmmod hello; o=\$(kmodctl load nosuchmod 2>&1); echo NAMEMISS_RC=\$? OUT=\$o; cat /proc/kmod_buildid" \
  "I=\$(cat /proc/kmod_buildid); mv /lib/modules/\$I /lib/modules/\$I.off; echo MVOFF_RC=\$?; kmodctl rmmod hello; kmodctl load hello; echo RUNLOAD_RC=\$?; kmodctl rmmod hello; mv /lib/modules/\$I.off /lib/modules/\$I; echo MVBACK_RC=\$?; ls /run/modules/\$I | head -3" \
  "kmodctl load $M/hello.kmod; o=\$(/bin/sudo -u leandro /bin/kmodctl rmmod hello 2>&1); echo NONROOT_RC=\$? OUT=\$o; kmodctl rmmod hello" > /dev/null 2>&1
L="${FFSESSION_OUT:-/tmp/ffsession}/tests-ul1/serial-full.log"
fail=0
check() { if grep -aq -- "$1" "$L"; then echo "ok   $2"; else echo "FAIL $2"; fail=1; fi; }
nocheck() { if grep -aq -- "$1" "$L"; then echo "FAIL $2"; fail=1; else echo "ok   $2"; fi; }
check HELLOLOOP_DONE "hello loop finished"
nocheck '^\(LOADFAIL\|UNLOADFAIL\) [0-9]' "hello x$N load/unload clean"
check 'ENOENT_RC=1' "rmmod of unloaded module fails"
check 'No such file' "ENOENT reported"
check 'ZOMBIE: exit timeout' "kmbad_hang becomes Zombie"
check 'HANGRELOAD_RC=1' "zombie reload refused"
check 'kmbad_hang .* Zombie' "Zombie in /proc/modules"
check 'kmbad_reload tick ok gen' "reload fixture ticks"
check 'NOEXIT_RC=1' "no-exit/unknown rmmod fails"
check 'BAD_DONE' "kmbad_thread/kmbad_freectx run finished"
check 'kmbad_thread body stopped' "kthread saw should_stop"
check 'kmbad_freectx exit: ctx freed' "freectx exit ran"
nocheck 'panic\|PANIC\|Data abort\|Oops' "no crash in the bad fixtures"
check 'RNGLOOP_DONE' "virtio_rng x$N finished"
nocheck '^RNG\(LOAD\|UNLOAD\)FAIL' "virtio_rng x$N clean"
check 'NETLOOP_DONE' "virtio_net loop finished"
nocheck '^NET\(UNLOAD\|LOAD\)FAIL' "virtio_net reload clean"
nocheck '\[NET\] netif eth1' "reload never creates eth1 (eth0 keeps its name)"
# Counters per cycle: rx/tx packets (fields 3 and 11 after "eth0:") must rise across reloads.
grep -a '^NETCNT ' "$L" | tr -d '\r' | sed 's/eth0:/eth0: /' > "${L%.log}-netcnt.txt"
cat "${L%.log}-netcnt.txt"
python3 - "${L%.log}-netcnt.txt" <<'PY' || fail=1
import sys
rows=[l.split() for l in open(sys.argv[1])]
rx=[];tx=[]
for r in rows:
    f=r[r.index('eth0:')+1:]
    if len(f)>=10: rx.append(int(f[1])); tx.append(int(f[9]))
ok=len(rx)>=2 and all(b>a for a,b in zip(rx,rx[1:])) and all(b>a for a,b in zip(tx,tx[1:]))
print(("ok   " if ok else "FAIL ")+"eth0 rx/tx packets rise across reloads (%d cycles, rx %s..%s, tx %s..%s)"%(len(rx),rx[:1],rx[-1:],tx[:1],tx[-1:]))
sys.exit(0 if ok else 1)
PY
n=$(grep -a 'DHCP configured, address: .* (eth0)' "$L" | wc -l)
if [ "$n" -ge "${NET_N:-20}" ]; then echo "ok   eth0 DHCP + traffic after each reload ($n)"; else echo "FAIL eth0 DHCP after reloads ($n)"; fail=1; fi
check 'NAMELOAD_RC=0' "kmodctl load <name> (build-id dir found via /proc/kmod_buildid; works with no /lib/modules on the root, the initrd copy is opened by path)"
check 'RUNLOAD_RC=0' "kmodctl load <name> with /lib/modules/<id> renamed away succeeds from /run/modules (staged by init)"
check 'modules: staged [0-9]* files' "init logs the staged module copy"
check 'NAMELOAD2_RC=0' "kmodctl load <name>.kmod"
check 'NAMEMISS_RC=1 OUT=.*module nosuchmod not found; searched /lib/modules/[0-9a-f]*/nosuchmod.kmod' "missing module error names the searched path"
check 'NONROOT_RC=[0-9]* OUT=.*delete_module: Operation not permitted (errno 1)' "non-root errno is EPERM"
nocheck 'STALE REACHED' "stale registration never reaches the new generation"
# ---- U3 upper edge (kmodupper): mounted usb_storage is busy, reload re-mounts, usb_hid x10, kmbad_leak Zombie.
# Needs KMOD_EXTRA="... kmbad_leak usb_storage usb_hid" and an untracked .config (CONFIG_MODULES=y CONFIG_USB_HID=m CONFIG_USB_STORAGE=m).
# The stick is a small f2fs volume made by scripts/mkfs-f2fs-home.py (mounted read-only; a rw umount of it broke the root lookup).
if [ -n "${UPPER:-}" ]; then
  stick="${TMPDIR:-/tmp}/ul-stick.img"; rm -f "$stick"; python3 scripts/mkfs-f2fs-home.py create "$stick" --size 128M >/dev/null
  export LEANDROS_QEMU_EXTRA="-device qemu-xhci,id=xhs -device usb-kbd,bus=xhs.0,id=kbd1 -drive if=none,id=stk,format=raw,file=$stick -device usb-storage,bus=xhs.0,drive=stk"
  python3 "$RT" "$arch" ul3 --cmdline "kmod.leakinput=1 modules.blacklist=virtio_keyboard" \
    "for d in /lib/modules/*/; do if kmodctl load \${d}hello.kmod >/dev/null 2>&1; then kmodctl rmmod hello >/dev/null 2>&1; MD=\${d%/}; break; fi; done; mkdir /tmp/stk" \
    "mount -t f2fs /dev/sda /tmp/stk; echo MOUNTRW_RC=\$?; kmodctl rmmod usb_storage; echo BUSYRW_RC=\$?; echo hi > /tmp/stk/f1 && sync; echo WRITEAFTER_RC=\$?; cat /tmp/stk/f1 > /tmp/f1copy; echo CATAFTER_RC=\$?; umount /tmp/stk; echo UMOUNTRW_RC=\$?" \
    "mount -t f2fs -o ro /dev/sda /tmp/stk; echo MOUNT_RC=\$?; kmodctl rmmod usb_storage; echo BUSY_RC=\$?" \
    "umount /tmp/stk; kmodctl rmmod usb_storage; echo UMNT_RM_RC=\$?" \
    "sleep 1; kmodctl load $M/usb_storage.kmod; sleep 3; mount -t f2fs -o ro /dev/sda /tmp/stk; echo REMOUNT_RC=\$?; umount /tmp/stk; kmodctl rmmod usb_storage; echo RM2_RC=\$?" \
    "sleep 1; kmodctl load $M/usb_storage.kmod; sleep 3; exec 3</dev/sda; kmodctl rmmod usb_storage; echo FDBUSY_RC=\$?; dd if=/dev/sda bs=512 count=1 of=/dev/null 2>/dev/null; echo FDREAD_RC=\$?; exec 3<&-; kmodctl rmmod usb_storage; echo FDCLOSE_RM_RC=\$?" \
    "sleep 1; kmodctl load $M/usb_storage.kmod; sleep 3; for i in \$(seq 1 20); do exec 3</dev/sda; kmodctl rmmod usb_storage >/dev/null 2>&1; echo FDLOOP_RC=\$?; exec 3<&-; done; echo FDLOOP_DONE; kmodctl rmmod usb_storage; echo FDLOOP_RM_RC=\$?; cat /proc/modules" \
    "for i in \$(seq 1 10); do kmodctl rmmod usb_hid >/dev/null 2>&1; kmodctl load $M/usb_hid.kmod >/dev/null 2>&1 || echo HIDLOADFAIL \$i; sleep 1; done; echo HIDLOOP_DONE" \
    "kmodctl load $M/kmbad_leak.kmod; kmodctl rmmod kmbad_leak; echo LEAKRM_RC=\$?; cat /proc/modules" > /dev/null 2>&1
  L="${FFSESSION_OUT:-/tmp/ffsession}/tests-ul3/serial-full.log"
  check 'usb_storage busy: mounted' "mounted usb_storage is busy"
  check 'BUSY_RC=1' "rmmod of mounted usb_storage fails"
  check 'BUSYRW_RC=1' "rmmod of rw-mounted usb_storage fails (fast EBUSY)"
  check 'WRITEAFTER_RC=0' "rw fs still writes + syncs right after the refused rmmod"
  check 'CATAFTER_RC=0' "rw fs still reads after the refused rmmod"
  check 'UMOUNTRW_RC=0' "rw umount after refused rmmod"
  check 'UMNT_RM_RC=0' "rmmod after umount ok"
  check 'REMOUNT_RC=0' "reload re-mounts"
  check 'RM2_RC=0' "second rmmod ok"
  check 'FDBUSY_RC=1' "rmmod with an open /dev/sda fd is busy"
  check 'busy: block device open' "busy reason names the open block fd"
  check 'FDREAD_RC=0' "disk still reads after the refused rmmod"
  check 'FDCLOSE_RM_RC=0' "rmmod after closing the fd ok"
  check 'FDLOOP_DONE' "open-fd rmmod loop x20 finished"
  nocheck 'FDLOOP_RC=0' "no rmmod succeeded while the fd was open"
  check 'FDLOOP_RM_RC=0' "rmmod after the loop ok"
  nocheck 'usb_storage .*Zombie' "no Zombie after the fd loop"
  echo "skip loop-on-device and BLKPG partition pins: the guest has no losetup or BLKPG tool (covered by code path block_family_busy only)"
  check 'HIDLOOP_DONE' "usb_hid x10 finished"
  nocheck '^HIDLOADFAIL [0-9]' "usb_hid reload clean"
  check 'kmbad_leak ZOMBIE: ungated registrations left' "kmbad_leak becomes Zombie"
  check 'kmbad_leak .* Zombie' "kmbad_leak Zombie in /proc/modules"
fi
# ---- usb_hid unload/reload x10 WHILE a USB keyboard is bound (HID=1; also part of UPPER=1 builds' needs).
# Needs KMOD_EXTRA to include usb_hid and CONFIG_USB_HID=m. The keyboard is a QEMU usb-kbd on xHCI, bound by usb_hid
# through the `usb * 03 01 01` alias. /dev/input/hotplug (a broadcast stream of add/remove eventN records) is read
# by a background cat across the rounds; an open reader on the keyboard's event node must see EOF/error on unload
# rather than pin rmmod (input detaches by design). After the final reload, background readers on every event node are
# started, the runner's `host:hidkeytest.py` step sends KEY_A over QMP, and the readers' output must hold an EV_KEY
# KEY_A record (od 16-bit words "0001 001e" = type 1, code 30).
if [ -n "${HID:-}" ]; then
  HN="${HID_N:-10}"
  export LEANDROS_QEMU_EXTRA="-device qemu-xhci,id=xhs -device usb-kbd,bus=xhs.0,id=kbd1"
  python3 "$RT" "$arch" ul5 --cmdline "modules.blacklist=virtio_keyboard" \
    "for d in /lib/modules/*/; do if kmodctl load \${d}hello.kmod >/dev/null 2>&1; then kmodctl rmmod hello >/dev/null 2>&1; MD=\${d%/}; break; fi; done; echo MD=\$MD; sleep 3; cat /proc/modules" \
    "(cat /dev/input/hotplug > /tmp/hp 2>&1 &); sleep 1; for i in \$(seq 1 $HN); do kmodctl rmmod usb_hid; echo HIDRM_RC \$i \$?; cat /proc/modules; echo HIDGONE \$i; ls /dev/input; kmodctl load $M/usb_hid.kmod; echo HIDLD_RC \$i \$?; sleep 2; echo HIDBACK \$i; cat /proc/modules; done; echo HIDLOOP_DONE; cat /tmp/hp" \
    "(cat /dev/input/hotplug > /tmp/hp2 2>&1 &); for n in 0 1 2 3 4 5 6 7; do [ -e /dev/input/event\$n ] && echo EVNODE \$n; done; ( cat /dev/input/event2 > /dev/null; echo READER_EXIT \$? ) & sleep 2; echo PRE_RM; kmodctl rmmod usb_hid; echo OPENRM_RC=\$?; sleep 2; echo OPENWAIT_DONE; cat /tmp/hp2; kmodctl load $M/usb_hid.kmod; echo OPENRELOAD_RC=\$?; sleep 2; cat /proc/modules; echo HID_DONE" \
    "rm -f /tmp/kk*; for f in /dev/input/event*; do (cat \$f > /tmp/kk\${f##*event} 2>/dev/null &); done; sleep 1; echo KEYREADERS_UP" \
    "host:hidkeytest.py" \
    "sleep 2; for f in /tmp/kk*; do echo KEYFILE \$f \$(od -An -tx2 -w24 \$f | tr -s ' ' | grep -c ' 0001 001e '); done; echo KEYCHECK_DONE" > /dev/null 2>&1
  L="${FFSESSION_OUT:-/tmp/ffsession}/tests-ul5/serial-full.log"
  check '\[USB-HID\].*keyboard' "usb_hid bound the QEMU keyboard"
  check 'HIDLOOP_DONE' "usb_hid x$HN with a bound keyboard finished"
  n=$(grep -ac '^HIDRM_RC [0-9]* 0' "$L"); if [ "$n" -ge "$HN" ]; then echo "ok   rmmod rc 0 every round ($n)"; else echo "FAIL rmmod rc 0 ($n/$HN)"; fail=1; fi
  n=$(grep -ac '^HIDLD_RC [0-9]* 0' "$L"); if [ "$n" -ge "$HN" ]; then echo "ok   load rc 0 every round ($n)"; else echo "FAIL load rc 0 ($n/$HN)"; fail=1; fi
  nocheck 'usb_hid .*Zombie' "no Zombie in /proc/modules"
  nocheck '\[BUDDY\]\|panic\|PANIC\|Data abort\|Oops' "no [BUDDY] / panic in the serial log"
  n=$(grep -ac '^remove event[0-9]* kbd' "$L"); if [ "$n" -ge "$HN" ]; then echo "ok   keyboard event node removed on each unload ($n remove records)"; else echo "FAIL keyboard remove records ($n/$HN)"; fail=1; fi
  n=$(grep -ac '^add event[0-9]* kbd' "$L"); if [ "$n" -ge "$HN" ]; then echo "ok   keyboard event node re-added on each load ($n add records)"; else echo "FAIL keyboard add records ($n/$HN)"; fail=1; fi
  check 'OPENWAIT_DONE' "rmmod with open readers on event nodes did not block"
  if tr -d '\r' < "$L" | grep -a '^\(PRE_RM\|READER_EXIT\)' | tr '\n' ' ' | grep -aq 'PRE_RM READER_EXIT'; then echo "ok   open reader on the keyboard node ended only after the unload (EOF/error, no pin)"; else echo "FAIL open reader did not end after the unload"; fail=1; fi
  check 'OPENRM_RC=0' "open event-node reader does not pin rmmod"
  check 'OPENRELOAD_RC=0' "reload after the open-reader unload"
  check 'HID_DONE' "usb_hid bound-keyboard run finished"
  if grep -aq HOSTKEY_SENT "$(dirname "$L")/results.txt"; then echo "ok   KEY_A sent over QMP after the final reload"; else echo "FAIL KEY_A sent over QMP after the final reload"; fail=1; fi
  if tr -d '\r' < "$L" | grep -aq '^KEYFILE /tmp/kk[0-9]* [1-9]'; then echo "ok   KEY_A (EV_KEY code 30) reached a keyboard event node after the final reload"; else echo "FAIL no KEY_A record on any event node after the final reload"; fail=1; fi
  unset LEANDROS_QEMU_EXTRA
fi
# ---- kmodexit: virtio_keyboard, virtio_snd and hda unload/reload x${EXIT_N:-20} (EXIT3=1).
# Needs KMOD_EXTRA to include "virtio_keyboard virtio_snd hda" and an untracked .config with CONFIG_VIRTIO_KEYBOARD=m
# CONFIG_VIRTIO_SND=m CONFIG_HDA=m. QEMU's default virtio-keyboard/tablet/virtio-snd are used; an Intel HDA codec is added.
# An open /dev/pipewire pins the sound module (-EBUSY); input detaches. hda attaches after virtio_snd is unloaded
# (one sound backend at a time), which also proves the backend slot is freed on unload.
if [ -n "${EXIT3:-}" ]; then
  EN="${EXIT_N:-20}"
  export LEANDROS_QEMU_EXTRA="-device ich9-intel-hda,id=hdax -device hda-duplex,bus=hdax.0,audiodev=snd0"
  python3 "$RT" "$arch" ul4 \
    "for d in /lib/modules/*/; do if kmodctl load \${d}hello.kmod >/dev/null 2>&1; then kmodctl rmmod hello >/dev/null 2>&1; MD=\${d%/}; break; fi; done; echo MD=\$MD; cat /proc/modules" \
    "for i in \$(seq 1 $EN); do kmodctl rmmod virtio_keyboard >/dev/null 2>&1 || echo KBUNLOADFAIL \$i; kmodctl load $M/virtio_keyboard.kmod >/dev/null 2>&1 || echo KBLOADFAIL \$i; done; echo KBLOOP_DONE; cat /proc/modules" \
    "for i in \$(seq 1 $EN); do kmodctl rmmod virtio_snd >/dev/null 2>&1 || echo SNDUNLOADFAIL \$i; kmodctl load $M/virtio_snd.kmod >/dev/null 2>&1 || echo SNDLOADFAIL \$i; done; echo SNDLOOP_DONE" \
    "(sleep 6 > /dev/pipewire &); sleep 1; kmodctl rmmod virtio_snd; echo SNDBUSY_RC=\$?; sleep 7; kmodctl rmmod virtio_snd; echo SNDFREE_RC=\$?" \
    "for i in \$(seq 1 $EN); do kmodctl load $M/hda.kmod >/dev/null 2>&1 || echo HDALOADFAIL \$i; kmodctl rmmod hda >/dev/null 2>&1 || echo HDAUNLOADFAIL \$i; done; echo HDALOOP_DONE" \
    "kmodctl load $M/hda.kmod; (sleep 6 > /dev/pipewire &); sleep 1; kmodctl rmmod hda; echo HDABUSY_RC=\$?; sleep 7; kmodctl rmmod hda; echo HDAFREE_RC=\$?; kmodctl load $M/virtio_snd.kmod; echo SNDBACK_RC=\$?; cat /proc/modules" \
    "for i in \$(seq 1 12); do rm -f /tmp/held\$i; ( if exec 3>/dev/pipewire; then grep -q virtio_snd /proc/modules && : > /tmp/held\$i; sleep 1; else echo RACEOPENFAIL \$i; fi ) & kmodctl rmmod virtio_snd >/dev/null 2>&1; rc=\$?; wait; if [ \$rc = 0 ] && [ -e /tmp/held\$i ]; then echo RACEBAD \$i; fi; echo RACE_RC \$i \$rc; [ \$rc = 0 ] && kmodctl load $M/virtio_snd.kmod >/dev/null 2>&1; done; echo RACE_DONE; echo EXIT3_DONE" > /dev/null 2>&1
  L="${FFSESSION_OUT:-/tmp/ffsession}/tests-ul4/serial-full.log"
  check 'KBLOOP_DONE' "virtio_keyboard x$EN finished"
  nocheck '^KB\(UNLOAD\|LOAD\)FAIL' "virtio_keyboard x$EN unload/reload clean"
  check 'SNDLOOP_DONE' "virtio_snd x$EN finished"
  nocheck '^SND\(UNLOAD\|LOAD\)FAIL' "virtio_snd x$EN unload/reload clean"
  check 'SNDBUSY_RC=1' "open /dev/pipewire pins virtio_snd (EBUSY)"
  check 'SNDFREE_RC=0' "virtio_snd unloads after the fd closed"
  check 'HDALOOP_DONE' "hda x$EN finished"
  nocheck '^HDA\(UNLOAD\|LOAD\)FAIL' "hda x$EN unload/reload clean"
  check 'HDABUSY_RC=1' "open /dev/pipewire pins hda (EBUSY)"
  check 'HDAFREE_RC=0' "hda unloads after the fd closed"
  check 'SNDBACK_RC=0' "virtio_snd attaches again after hda (backend slot freed)"
  # Sound race: open /dev/pipewire as rmmod starts. Each round either rmmod is -EBUSY / the open failed, or
  # the open landed after the unload; never a held fd while rmmod returned 0 (a silent stream).
  check 'RACE_DONE' "pipewire open vs rmmod race loop finished"
  nocheck '^RACEBAD' "no open held across a successful rmmod (no silent stream)"
  # NOTE: held-key release (input_release pushes key-ups) is not tested here: the runner has no QEMU monitor
  # sendkey channel; covered by review only.
  check 'EXIT3_DONE' "kmodexit run finished"
  n=$(grep -ac 'virtio_keyboard: freed' "$L"); if [ "$n" -ge "$EN" ]; then echo "ok   virtio_keyboard exit ran ($n)"; else echo "FAIL virtio_keyboard exit ($n)"; fail=1; fi
  n=$(grep -ac 'hda: freed' "$L"); if [ "$n" -ge "$EN" ]; then echo "ok   hda exit ran ($n)"; else echo "FAIL hda exit ($n)"; fail=1; fi
  nocheck 'ZOMBIE' "no Zombie"
  nocheck 'exit left [0-9]* DMA pages owned' "no DMA pages left owned"
  nocheck 'panic\|PANIC\|Data abort\|Oops' "no crash"
fi
exit $fail
