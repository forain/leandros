#!/usr/bin/env python3
"""runtests.py <arch> <tag> [options] [cmd ...] -- boot, serial root login, run
each command, report each one's exit status reliably.

Every command goes through driver.serial_run: a nonce sentinel plus a status
file in the guest, so interleaved kernel console lines cannot turn a finished
run into `RC=?` (see the comment above serial_run in driver.py).

Options:
  --gpu none|virgl|venus   boot device (default none; RUNTESTS_VIRGL=1 = virgl)
  --virgl / --venus        same as --gpu virgl / --gpu venus
  --cmdline "ARGS"         boot a copy of the image with this kernel command line
                           (scripts/set-image-cmdline.sh; suite i915crtc2 = two-CRTC stub pipe; suite i915fake implies
                           "i915.fake=2 i915.kms=1")
  --suite NAME             add a named list of commands (repeatable):
                             regress  the 13 regression suites + vfstest + nettest + udptest + pcachetest + procmax + syscalltest + uaccesstest + proctest + chroottest + netctltest + bttest + sudotest + nmtest --quick (mock) + blueztest (org.bluez shim) + evherdtest + pollnarrowtest + listentagtest + vfstagtest + nettag2test + kparkstest + sshsmoke
                                      + wifisimtest (last: it reboots into its own VM)
                             wifisim  host:wifisimtest.py alone (simulated Wi-Fi self-test boot)
                             serialxfer host:serialxfertest.py alone (serial push + kexec, scripts/pi-serial-*)
                             psklog   host:psklogtest.py alone (a cmdline wifi.psk= never reaches serial/klog/root or non-root /proc/cmdline)
                             nitrox86 host:nitrotest.py alone (x86_64 without PIT/HPET/i8042/display, EC2 Nitro-like)
                             pcibridge host:pcibridgetest.py alone (root port -> PCI bridge -> virtio-rng, own boot)
                             usbisoch host:usbisochtest.py alone (xHCI isoch OUT: 2 s 1 kHz sine to usb-audio, WAV checked)
                             blkslow  host:blkslowtest.py alone (3 s late virtio-blk request survives; page-in EIO = SIGBUS)
                             blkmany  host:blkmanytest.py alone (12 extra virtio-blk disks: all in /dev, all mount)
                             rpi5check host:rpi5checktest.py (cargo check of the --features rpi5 kernel)
                             nvmeebs  host:nvmeebstest.py alone (Amazon EBS NVMe quirks via nvme.quirks=amzn: by-id links, writable root, 300 s stall cap)
                             stdev    host:stdevtest.py alone (two cloned f2fs mounts: distinct st_dev, cp -a works)
                             kexecacpi host:kexecacpitest.py alone (x86_64: ACPI tables in Reserved RAM > 2 GiB after kexec, own boot)
                             rootpick host:rootpicktest.py alone (extra modern virtio disk ahead: root still found by LABEL)
                             net-e2e  netctltest + nmtest (mock, full and --ro) + host:nmrealtest.py
                                      (the NM shim on the real /dev/netctl + wifi_sim stack, own boot)
                                      + host:netroamtest.py (Wi-Fi roaming between two subnets, own boot) + host:netflaptest.py
                                      + host:netidletest.py (idle net-daemon passes < 10/s)
                                      + host:inloadtest.py (hostfwd connects under bulk load and an unpumped held serial)
                                      + host:udppeertest.py (udptest peer: fragments, ICMP errors, IGMP, broadcast)
                             netroam  host:netroamtest.py alone
                             ec2init  host:ec2inittest.py alone (ec2init vs a fake IMDSv2 behind a slirp guestfwd)
                             a64its   host:a64itstest.py alone (aarch64 GICv3 ITS: virtio-net + NVMe on MSI-X LPIs, /proc/interrupts, gic.its=0 fallback)
                             udppeer  host:udppeertest.py alone (udptest peer vs a raw-frame host peer on a 2nd NIC)
                             ssh      host:sshsmoke.py (the guest sshd from the host's ssh client)
                             sshfull  host:sshsmoke.py --full (keys, sftp/scp, git over ssh/https)
                             straysweep host:straysweeptest.py (greetd exit spares detached ssh jobs, own boot)
                             net      /bin/nettest alone
                             udp      /bin/udptest alone
                             pollnarrow host:pollnarrowtest.py alone
                             listentag host:listentagtest.py alone
                             vfstag   host:vfstagtest.py alone
                             nettag2  host:nettag2test.py alone
                             kparks   host:kparkstest.py alone
                             podpasta host:podpastatest.py alone (rootless podman on pasta: echo, host wget, -p port-forward, dns, churn, no leaks)
                             podsmoke host:podsmoketest.py alone (root podman run --rm alpine echo hi; skips without the alpine image)
                             podbridge host:podbridgetest.py alone (root podman default bridge: echo, host/bridge wget, dns, 20x churn, no leaks)
                             podpause host:podpausetest.py alone (podman pause/unpause freezes and resumes a container)
                             drm      /bin/drmsmoke + /bin/gpuprobe gl (needs --gpu virgl|venus)
                           no commands and no --suite = regress
  --repeat N               boot N times, run the whole list each time
  A command `host:<script> [args]` runs that script of this directory on the
  host instead of in the guest (FAIL lines and exit status as for guest ones).
  --timeout S              per-command timeout in seconds, before scaling
                           (default 300)
  --mode M                 driver.py start mode (uefi, uefi-tcg, ...)
  --keep                   leave QEMU running after the last boot

Waits scale with the accelerator (driver.wait_scale: x3 on TCG, x1 on HVF/KVM;
LEANDROS_WAIT_SCALE overrides).

Output: $FFSESSION_OUT (default /tmp/ffsession)/tests-<tag>/
  run-<i>/results.txt (every command's output), run-<i>/serial.log (what the
  driver read), run-<i>/serial-full.log (everything, from QEMU's logfile),
  summary.json. One line per command on stdout:
  `=== <cmd>: RC=<n> [<status>, <secs>s] fails=<k>`, then a summary.
Exit status 0 iff every command returned 0 with a known status.
LEANDROS_RUN_ID defaults to "runtests" (scopes driver.py's sockets/logs).
"""
import json
import os
import re
import subprocess
import sys
import time

os.environ.setdefault("LEANDROS_RUN_ID", "runtests")
if any("podbridge" in a for a in sys.argv[1:]):
    # podbridgetest publish_ext: a host port unique per run (a fixed one made concurrent runs collide).
    if "LEANDROS_HOSTFWD" not in os.environ:
        import socket as _s
        _sk = _s.socket()
        _sk.bind(("127.0.0.1", 0))
        _hp = _sk.getsockname()[1]
        _sk.close()
        os.environ["PODBRIDGE_HOSTFWD_PORT"] = str(_hp)
        os.environ["LEANDROS_HOSTFWD"] = f"tcp:127.0.0.1:{_hp}-:18081"
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
sys.path.insert(0, HERE)
os.chdir(REPO)
import driver  # noqa: E402

SYSFS_CHECK = (
    "__f=0; "
    "t() { eval \"$2\" >/dev/null 2>&1 && echo \"sysfs: $1: PASS\" || { echo \"sysfs: $1: FAIL\"; __f=1; }; }; "
    "t drm_class_card0 'test -L /sys/class/drm/card0 && grep -q DEVNAME=dri/card0 /sys/class/drm/card0/uevent'; "
    "t drm_class_render 'test -L /sys/class/drm/renderD128 && grep -q MINOR=128 /sys/class/drm/renderD128/uevent'; "
    "t drm_class_list 'ls /sys/class/drm | grep -q card0'; "
    "t drm_dev_char 'grep -q MAJOR=226 /sys/dev/char/226:0/uevent && test -d /sys/dev/char/226:0/device/drm'; "
    "t drm_device_vendor 'grep -q 0x /sys/class/drm/card0/device/vendor && grep -q 0x /sys/class/drm/card0/device/device'; "
    "t drm_subsystem 'test -L /sys/dev/char/226:0/device/subsystem'; "
    "t input_class 'test -L /sys/class/input/event0 && test -L /sys/class/input/event1'; "
    "t input_uevent 'grep -q DEVNAME=input/event1 /sys/class/input/event1/uevent && grep -q MAJOR=13 /sys/class/input/event0/uevent'; "
    "t input_name 'grep -q Keyboard /sys/class/input/event0/device/name && grep -q Tablet /sys/class/input/event1/device/name'; "
    "t input_caps 'test -s /sys/class/input/event0/device/capabilities/ev && test -s /sys/class/input/event1/device/capabilities/abs'; "
    "t input_dev_char 'test -d /sys/dev/char/13:65/device/id'; "
    "test $__f = 0"
)

SUITES = {
    "regress": ["/bin/sigtest", "/bin/sigtest2", "/bin/memtest", "/bin/scmtest", "/bin/fusetest",
                "/bin/polltest", "/bin/fdmaxtest", "/bin/mqtest", "/bin/pipebw", "/bin/forktest", "/bin/shmtest", "/bin/exectest", "/bin/memfdtest", "/bin/conmontest", "/bin/exitracetest", "/bin/pthreadtest", "/bin/futextest",
                "/bin/epolltest", "/bin/strtest", "/bin/floattest", "/bin/uinputtest", SYSFS_CHECK,
                # Unlinked file held only by a mapping is reclaimed (lane f2fsfu).
                "/bin/f2fstest mmaporphan /root",
                # Truncate of a file with cold node blocks: one fetch round (lane f2fshang).
                "/bin/f2fstest trunc_cold /home",
                # Nested epoll sets on the ready list: LT/ET/ONESHOT/close/dup
                # cases, and probes per call of a cosmic-comp-shaped set.
                "/bin/epolltest cost",
                "/bin/epolltest abi",
                # /proc/epollstat per-instance attribution (lane epattr).
                "/bin/epolltest stat",
                "/bin/epolltest churn 20",
                # + a non-leader thread forking in a loop (lane tforkhang).
                "/bin/epolltest churn 20 tfork",
                # 5000 spawn/join + 2500 detached threads: VMAs/VmSize/RSS stay flat (lane threadleak).
                "/bin/pthreadtest churn 5000",
                # kmscube on the v3d UAPI stub (lane kmscube2, openbugs L11.8):
                # sync-file fence export + ADDFB2 of a v3d BO. The override is
                # needed only in QEMU, where Mesa names the driver from the
                # virtio-gpu PCI id before DRM_IOCTL_VERSION. Always disarmed.
                # aarch64 only: the x86_64 megadriver has no v3d.
                "if [ \"$(uname -m)\" = aarch64 ]; then drmsmoke --arm-v3d && MESA_LOADER_DRIVER_OVERRIDE=v3d kmscube -c 30; __k=$?; drmsmoke --disarm-v3d; test $__k = 0; fi",
                "/bin/timertest", "/bin/jobtest", "/bin/waittest",
                "/bin/sigchldtest", "/bin/vfstest", "/bin/nettest", "/bin/ip6test", "/bin/netlinktest", "/bin/nfttest", "/bin/udpselftest", "/bin/udptest", "/bin/pcachetest",
                "/bin/procmax", "/bin/syscalltest", "/bin/uaccesstest", "/bin/proctest", "/bin/chroottest", "/bin/mnttest", "/bin/nstest", "/bin/tuntest", "/bin/brtest", "/bin/cgtest", "/bin/ovltest", "/bin/ovlmknodtest", "/bin/idmaptest", "/bin/inotifytest", "/bin/captest", "/bin/seccomptest", "/bin/bpftest", "/bin/preadtest", "/bin/netctltest", "/bin/kmodctltest", "/bin/bttest", "/bin/pstest", "/bin/utmptest", "/bin/camtest --probe",
                # kexec tool: load + unload only (non-destructive).
                # A blocking pipe write of >PIPE_BUF delivers every byte (lane pipefix).
                "/bin/sh -c '[ \"$(dd if=/dev/zero bs=1M count=2 2>/dev/null | wc -c)\" = 2097152 ]'",
                "/bin/sh -c '/bin/kexec -l /boot/kexec --initrd=/boot/kexec-initrd --append=x && /bin/kexec -u'",
                # ripgrep (rg): file match, recursive dir walk, stdin, exit codes 0/1.
                "/bin/sh -c 'rg --version >/dev/null && rg -n root /etc/passwd >/dev/null && rg -l root /etc >/dev/null && echo foo | rg -q foo && { echo foo | rg -q zzz; [ $? -eq 1 ]; }'",
                "/bin/sh -c '[ \"$(echo foo | grep -c foo)\" = 1 ] && grep -n root /etc/passwd >/dev/null && grep -r PATH /etc >/dev/null && echo ab | grep -q \"a\\\\|zz\" && { grep -q x /nonexistent 2>/dev/null; [ $? -eq 2 ]; } && { echo foo | grep -q zzz; [ $? -eq 1 ]; }'",
                # su + sudo (sudo-rs): identity, groups, env, auth, policy.
                "/bin/sudotest",
                # Scheduler stress (lane percpu): lost wakes, stop/cont, group
                # exit, MT fork/exec, fairness, reaping storm; short mode.
                "/bin/schedstress short",
                # Process lifetime (lane proclife): a killed multithreaded
                # group is waitable only once its fds and memory are gone; a
                # non-leader execve raced by SIGKILL / the leader's
                # exit_group; what the exec'd image inherits.
                "/bin/killmt 40 proclife",
                # The NetworkManager shim (mock backend) through nmrs, the
                # applet's library; --quick skips the 25 s snapshot storm.
                # dbus-run-session is a script: the kernel has no #! binfmt.
                "/bin/sh /usr/bin/dbus-run-session -- /bin/nmtest --spawn --quick",
                # org.bluez shim (COSMIC Bluetooth applet/Settings) over btd on
                # bttest's scripted controller: scan, pair, connect, remove.
                "/bin/sh /usr/bin/dbus-run-session -- /usr/libexec/leandros-sysbus bluez-test",
                # xdg-desktop-portal activation (Screenshot/FileChooser, cosmic
                # backend), timedate1, SwitcherooControl and the a11y bus.
                "/bin/sh /usr/bin/dbus-run-session -- /usr/libexec/leandros-sysbus svc-test",
                # Input on one evdev device wakes no waiter on another (QMP
                # tablet motion vs a waiter parked on the keyboard).
                "host:evherdtest.py",
                # EVIOCGKEY reports held buttons/keys, also after a SYN_DROPPED.
                "host:evgkeytest.py",
                # Serial-console bytes reach no event0 reader; the opt-in
                # serial-kbd node gets mapped press+release pairs (L12 #67).
                "host:serialkbdtest.py",
                # Blocked /dev/netctl and console readers park narrow and
                # still wake on a snapshot, a cursor report, a key, a UART byte.
                "host:pollnarrowtest.py",
                # A TCP listener's pollers (EPOLLET, poll, select) park on its
                # own key and every connect reaches them within 20 ms.
                "host:listentagtest.py",
                # Blocked VFS waiters (flock/F_SETLKW, signalfd, inotify,
                # static fds, /dev/pipewire, DRM and evdev reads) park narrow
                # and wake on their event (lock release, signal, flip, input).
                "host:vfstagtest.py",
                # Blocking recv/connect and unbound/ICMP socket pollers park
                # narrow and still wake within 20 ms of the real event.
                "host:nettag2test.py",
                # Console poll/epoll, empty poll/select sets and VT_WAITACTIVE
                # park narrow and wake on input / their deadline / a VT switch.
                "host:kparkstest.py",
                "host:sshsmoke.py",
                # Input event paths in the running VM (lane regresscov2; ~20 s total).
                "host:evbtntest.py", "host:evclocktest.py", "host:hidkeytest.py",
                # Root `podman run --rm alpine echo hi` (OCI masked /proc paths; lane procbind). Skips without the alpine image.
                "host:podsmoketest.py",
                # f2fs nid allocator never re-hands a live nid. Own boot: it
                # stops this VM, so it must come after every suite that uses it.
                "host:f2fsnidtest.py",
                # Unknown-outcome checkpoint commit: acked fsyncs survive (own boot, stops this VM).
                "host:f2fscommittest.py",
                # Space reuse on a small f2fs volume: churn writes N x capacity,
                # statfs/ENOSPC and fsck invariants (lane f2fsgc; own boot, stops this VM).
                "host:f2fsgctest.py",  # default factor 8: the 2026-11-15 statfs/ENOSPC bug only showed at churn round 27
                # Wayland frame callbacks while outputs are off (lane regresscov2; own boot, ~1.5 min).
                "host:blanktest.py",
                # Must stay last: it stops this VM and boots its own (wifi_sim
                # self-test on a second NIC, see wifisimtest.py).
                "host:wifisimtest.py"],
    # nettest finds the gateway in /proc/net/route and the DNS server in
    # /etc/resolv.conf, so it runs unchanged on slirp (10.0.2.x) and on
    # socket_vmnet (192.168.105.x); LEANDROS_NET=user forces slirp. Its DNS
    # cases resolve example.com, so the host needs internet access.
    "net": ["/bin/nettest"],
    "netlink": ["/bin/netlinktest"],
    "nft": ["/bin/nfttest"],
    "bridge": ["/bin/brtest"],
    # socket_vmnet head-of-line guard (L5.13): wedges the shared bridge with a
    # client that never reads, checks driver.py's probe sees it and that it
    # heals. Not in regress: while it runs, every vmnet VM on this Mac is deaf.
    "vmnethealth": ["host:vmnethealth.py --selftest"],
    # Nested epoll sets on the ready list: correctness, then the cost bench
    # (`epolltest costbench` prints µs per call, probes and useless wakes).
    "epollcost": ["/bin/epolltest cost", "/bin/epolltest costbench"],
    # lane epollabi: epoll_ctl EINVAL/ELOOP/EPOLLEXCLUSIVE rules, and the
    # poll() cost bench (`pollbench`: ns per call, probes per woken call).
    "epollabi": ["/bin/epolltest abi", "/bin/polltest", "/bin/epolltest pollbench"],
    # UDP semantics and the EPOLLET edges of UDP/ICMP/TCP listeners (loopback).
    "udp": ["/bin/udptest"],
    # Narrow poll parks of blocked /dev/netctl and console readers.
    "pollnarrow": ["host:pollnarrowtest.py"],
    # Narrow poll parks of TCP listeners, and their accept latency.
    "listentag": ["host:listentagtest.py"],
    # Narrow poll parks of blocked VFS waiters, and their wake on a real event.
    "vfstag": ["host:vfstagtest.py"],
    # Narrow parks of blocking socket I/O and of sockets with no object yet.
    "nettag2": ["host:nettag2test.py"],
    # Narrow kernel parks: console poll/epoll, empty poll sets, VT_WAITACTIVE.
    "kparks": ["host:kparkstest.py"],
    # Root `podman run --rm alpine echo hi` (skips without the LEANDROS_IMAGE_ARCHIVE image).
    "podsmoke": ["host:podsmoketest.py"],
    # Root podman on the default bridge network (needs LEANDROS_IMAGE_ARCHIVE image).
    "podbridge": ["host:podbridgetest.py"],
    # podman pause/unpause freeze and resume a container (needs LEANDROS_IMAGE_ARCHIVE image).
    "podpause": ["host:podpausetest.py"],
    # Rootless podman on the default pasta network, as user leandro (needs LEANDROS_IMAGE_ARCHIVE image).
    "podpasta": ["host:podpastatest.py"],
    # /proc/<pid>/comm, prctl names and /bin/ps.
    "procps": ["/bin/pstest"],
    "utmp": ["/bin/utmptest"],
    # /dev/netctl: permission model, event stream, profiles (runs as root).
    "netctl": ["/bin/netctltest"],
    "kmodctl": ["/bin/kmodctltest"],
    "ns": ["/bin/nstest"],
    "mnt": ["/bin/mnttest"],
    "ovl": ["/bin/ovltest"],
    "ovlmknod": ["/bin/ovlmknodtest"],
    "bpf": ["/bin/bpftest"],
    "idmap": ["/bin/idmaptest"],
    # inotify (on by default; with --cmdline vfs.inotify=0 only the kill-switch guard runs).
    "inotify": ["/bin/inotifytest"],
    "inotifyforce": ["/bin/inotifytest force"],
    "cgroup": ["/bin/cgtest"],
    # The simulated Wi-Fi (kernel wifi_sim + wifi_mgr) self-test in its own boot.
    "wifisim": ["host:wifisimtest.py"],
    "psklog": ["host:psklogtest.py"],
    # lane piserialxfer: serial-console push (bootstrap + 5.6 MB) and kexec; leaves the VM kexec'd.
    "serialxfer": ["host:serialxfertest.py"],
    # lane kexecfdt: the kexec boot FDT block returns to the map along a gen1..gen4 chain (leaves the VM kexec'd).
    "kexecfdt": ["host:kexecfdttest.py"],
    # lane kmodblk: a kexec'd kernel mounts root with VIRTIO_BLK=m (CONFIG_VIRTIO_BLK=m build only).
    "kexecblk": ["host:kexecblktest.py"],
    # lane kexecacpi: x86_64 ACPI tables in Reserved RAM above 2 GiB survive kexec (own boot, 6G + max-ram-below-4g=3G).
    "kexecacpi": ["host:kexecacpitest.py"],
    # lane dtbres: devices behind PCIe root port -> PCI-PCI bridge (own boot).
    "pcibridge": ["host:pcibridgetest.py"],
    # lane nitrox86: x86_64 with no PIT/HPET/i8042/display (EC2 Nitro-like): login on serial, TSC stated, clock true (own boot).
    "nitrox86": ["/bin/timertest", "host:nitrotest.py"],
    "usbisoch": ["host:usbisochtest.py"],
    "blkslow": ["host:blkslowtest.py"],
    # 12 extra virtio-blk data disks all appear in /dev and mount (own boot, openbugs #83).
    "blkmany": ["host:blkmanytest.py"],
    # --features rpi5 kernel compile-check (cargo check, no boot; integ lanes run it).
    "rpi5check": ["host:rpi5checktest.py"],
    "stdev": ["host:stdevtest.py"],
    "f2fsnid": ["host:f2fsnidtest.py"],
    "f2fscommit": ["host:f2fscommittest.py"],
    "f2fsgc": ["host:f2fsgctest.py"],
    "a64acpi": ["host:a64acpitest.py"],
    "blank": ["host:blanktest.py"],
    "blkhang": ["host:blkhangtest.py"],
    "espreset": ["host:espresettest.py"],
    "evbtn": ["host:evbtntest.py"],
    "evclock": ["host:evclocktest.py"],
    "f2fscp": ["host:f2fscptest.py"],
    "f2fscrash": ["host:f2fscrashtest.py"],
    "f2fsfull": ["host:f2fsfulltest.py"],
    "f2fsorphan": ["host:f2fsorphantest.py"],
    "hidkey": ["host:hidkeytest.py"],
    "netwatch": ["host:netwatchtest.py"],
    "nvmemsix": ["host:nvmemsixtest.py"],
    "pagecache-crash": ["host:pagecache-crashtest.py"],
    "powerctl": ["host:powerctltest.py"],
    "reboot": ["host:reboottest.py"],
    "trialwatch": ["host:trialwatchtest.py"],
    "uartgap": ["host:uartgaptest.py"],
    "usbstall": ["host:usbstalltest.py"],
    "usbunplug": ["host:usbunplugtest.py"],
    "rootpick": ["host:rootpicktest.py"],
    # Dead virtio-blk: a mapped-page fault must SIGBUS (own boot, ~2 min; not in regress).
    "blkfault": ["host:blkfaulttest.py"],
    # NVMe command timeout: the driver resets the controller and the stalled
    # read finishes with intact data (own boot, ~2 min; not in regress).
    "nvmefault": ["host:nvmefaulttest.py"],
    "nvmeebs": ["host:nvmeebstest.py"],
    # Network management end to end: /dev/netctl conformance, the NM shim on
    # its mock (full, with the snapshot storm, and read-only), then the shim
    # on the REAL stack (nmrealtest.py: its own boot with wifi_sim, nmtest as
    # leandro and as a non-netdev user, profile persistence across a reboot).
    "net-e2e": ["/bin/netctltest",
                "/bin/sh /usr/bin/dbus-run-session -- /bin/nmtest --spawn",
                "/bin/sh /usr/bin/dbus-run-session -- /bin/nmtest --spawn --ro",
                "host:nmrealtest.py",
                # Roaming between two subnets on wifi_sim (listeners and UDP
                # servers across address churn, autoconnect policy, the
                # not-joinable diagnostics): its own boot, see netroamtest.py.
                "host:netroamtest.py",
                # netctl event path on a QMP set_link flap (own boot).
                "host:netflaptest.py",
                # netctl metric + net.metric.<if>= boot knob (own boot).
                "host:netmetrictest.py",
                # EC2 first-boot service vs a fake IMDSv2 (own boot).
                "host:ec2inittest.py",
                # Idle net-daemon wake bound (own boot): netidletest.py.
                "host:netidletest.py",
                # hostfwd connects under bulk load / a held unpumped serial (own boot).
                "host:inloadtest.py",
                # e1000e link monitor on ICR.LSC (set_link flaps, idle wake
                # bound) + netcheck --put TX bulk (own boot): linkmontest.py.
                "host:linkmontest.py",
                # Bulk upload over a Wi-Fi-like shaped link: no spinning on a
                # full driver TX queue (own boot): nettxtest.py.
                "host:nettxtest.py --tx --shape eth0:20000:20000:64:256:2",
                # Shaped bulk download (tail-dropping AP queue): the guest's
                # SACK must let the sender repair a burst of losses (shapedl).
                "host:nettxtest.py --shape eth0:20000:20000:64:256:2",
                # IP fragmentation, ICMP errors, IGMP, broadcast, cmsgs against
                # a frame-level host peer on a second NIC: udppeertest.py.
                "host:udppeertest.py"],
    # netroamtest.py alone.
    "netroam": ["host:netroamtest.py"],
    "netmetric": ["host:netmetrictest.py"],
    "ec2init": ["host:ec2inittest.py"],
    # GICv3 ITS MSI/MSI-X on aarch64 (own boots, ~6 min TCG; aarch64 only, x86_64 skips).
    "a64its": ["host:a64itstest.py"],
    # udppeertest.py alone (udptest peer against the raw-Ethernet host peer).
    "udppeer": ["host:udppeertest.py"],
    # linkmontest.py alone.
    "linkmon": ["host:linkmontest.py"],
    # netidletest.py alone.
    "netidle": ["host:netidletest.py"],
    "inload": ["host:inloadtest.py"],
    # nettxtest.py alone (shaped upload).
    "nettx": ["host:nettxtest.py --tx --shape eth0:20000:20000:64:256:2",
              "host:nettxtest.py --shape eth0:20000:20000:64:256:2"],
    # gpuprobe gl is what gpu-env runs before COSMIC; RC=0 only on a hardware
    # renderer (2 = software, 139 = the strict-PAN DRM ioctl SIGSEGV of L11.13).
    # i915 UAPI end to end on the fake device (soft command streamer), the
    # path Mesa iris takes on the laptop: catches a DRM-layer change that
    # breaks i915 ioctl argument access (lane irisseg).
    "i915fake": ["/bin/i915test"],
    "i915crtc2": ["/bin/drmsmoke --crtc2"],
    # The same with every soft-engine fence pending for 2 ms after submit, so
    # CPU fence waits really wait: the fence-wait spin bound (lane rddsys).
    "i915fence": ["/bin/i915test"],
    "drm": ["/bin/drmsmoke", "/bin/gpuprobe gl"],
    # Synthetic sysfs: /sys/class/{drm,input} and the /sys/dev/char node
    # identity libdrm/udev consumers read (lane sysfs).
    "sysfs": [SYSFS_CHECK],
    # The guest's sshd, driven from the host with the real OpenSSH client
    # (sshsmoke.py: banner, password exec, exit status, stdin EOF, pty).
    # sshfull adds key auth, root refusal, window change, sftp/scp, and git
    # over ssh and https.
    "ssh": ["host:sshsmoke.py"],
    "straysweep": ["host:straysweeptest.py"],
    "sshfull": ["host:sshsmoke.py --full"],
}


# Host scripts that boot their own VM and take longer than one command:
# their timeout (seconds, before scaling) when it exceeds --timeout.
HOST_TIMEOUTS = {"sshsmoke.py": 900, "churn127live.py": 900, "podbridgetest.py": 2400, "podpausetest.py": 900, "podpastatest.py": 2400, "netroamtest.py": 1500, "udppeertest.py": 1500, "pcibridgetest.py": 600, "kexecacpitest.py": 900, "usbisochtest.py": 900,
                 "netwatchtest.py": 1200, "powerctltest.py": 600, "espresettest.py": 900, "trialwatchtest.py": 1200, "usbunplugtest.py": 1200}


def find_fails(out):
    """Failing case names from every guest print format:
    `name: FAIL`, `FAIL name: detail`, `FAIL: name`, `[FAIL] name`."""
    pats = (r"^\s*(\S+): FAIL\b", r"^\s*FAIL:?\s+([^\s:]+)", r"^\s*\[FAIL\]\s+([^\s:]+)")
    seen = []
    for pat in pats:
        for n in re.findall(pat, out, re.M):
            if n not in seen:
                seen.append(n)
    return seen


def host_run(c, timeout, arch=None, scale=1.0):
    """`host:<script> [args]`: run a script of this directory ON THE HOST
    (same LEANDROS_RUN_ID, so it finds this VM), in serial_run's result shape.
    LEANDROS_TEST_ARCH tells a script the arch when this VM is already gone
    (a script that boots its own VM stops it)."""
    script = c.split()[0][len("host:"):]
    argv = [sys.executable, os.path.join(HERE, script)] + c.split()[1:]
    timeout = max(timeout, HOST_TIMEOUTS.get(script, 0) * scale)
    env = dict(os.environ)
    if arch:
        env["LEANDROS_TEST_ARCH"] = arch
    t0 = time.time()
    try:
        r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, env=env)
        return {"rc": r.returncode, "status": "ok", "output": r.stdout + r.stderr,
                "secs": round(time.time() - t0, 1)}
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        return {"rc": None, "status": "timeout", "output": out, "secs": round(time.time() - t0, 1)}


def self_boots(c):
    """True for a `host:` script that starts (and stops) its own VM under its own
    LEANDROS_RUN_ID. Such a script must not run while runtests' VM is up: both
    would open the same disk image and QEMU's write lock fails the second boot."""
    if not c.startswith("host:"):
        return False
    try:
        with open(os.path.join(HERE, c.split()[0][len("host:"):])) as f:
            return '"start"' in f.read()
    except OSError:
        return False


def main(argv):
    if len(argv) < 2 or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 2
    arch, tag = argv[0], argv[1]
    rest = argv[2:]
    driver.claim_run_id("runtests.py " + tag)
    gpu = "virgl" if os.environ.get("RUNTESTS_VIRGL") else "none"
    repeat, base_timeout, mode, keep = 1, 300.0, "uefi", False
    cmds, suites = [], []
    cmdline = None
    i = 0
    while i < len(rest):
        a = rest[i]
        if a == "--gpu":
            gpu = rest[i + 1]; i += 2
        elif a in ("--virgl", "--venus"):
            gpu = a[2:]; i += 1
        elif a == "--suite":
            suites.append(rest[i + 1]); i += 2
        elif a == "--repeat":
            repeat = int(rest[i + 1]); i += 2
        elif a == "--timeout":
            base_timeout = float(rest[i + 1]); i += 2
        elif a == "--mode":
            mode = rest[i + 1]; i += 2
        elif a == "--keep":
            keep = True; i += 1
        elif a == "--cmdline":
            cmdline = rest[i + 1]; i += 2
        else:
            cmds.append(a); i += 1
    if not cmds and not suites:
        suites = ["regress"]
    for s in suites:
        if s not in SUITES:
            print(f"unknown suite {s!r}; known: {', '.join(SUITES)}")
            return 2
        cmds += SUITES[s]
    if "nitrox86" in suites:
        os.environ["LEANDROS_NITRO"] = "1"  # x86_64: no PIT/HPET/i8042/display device
    if "i915fake" in suites and cmdline is None:
        cmdline = "i915.fake=2 i915.kms=1"
    if "i915crtc2" in suites and cmdline is None:
        cmdline = "i915.fake=2 i915.kms=1 i915.extout=1 i915.pipeb_stub=1"
    if "i915fence" in suites and cmdline is None:
        cmdline = "i915.fake=2 i915.kms=1 i915.fake_lat_us=2000"
    # GPU-accounting suites (venustest/drmsmoke count window spans, blobs and
    # free context slots) need to be the only GPU client: with a GPU device
    # greetd starts COSMIC, whose cosmic-comp/wgpu contexts allocate the same
    # resources concurrently and turn every count check into a false leak.
    if gpu in ("venus", "virgl") and any(("venustest" in c or "drmsmoke" in c) for c in cmds) \
            and "nvrm.desktop" not in (cmdline or ""):
        cmdline = " ".join(x for x in (cmdline, "nvrm.desktop=0") if x)

    scale = driver.wait_scale(arch, mode)
    accel = driver.accel_kind(arch, mode)
    timeout = base_timeout * scale
    out = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), f"tests-{tag}")
    os.makedirs(out, exist_ok=True)
    if cmdline is not None:
        # A private copy (clonefile on APFS) with the requested cmdline; the
        # driver boots LEANDROS_DISK_IMAGE instead of the build's image.
        import shutil
        src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
        img = os.path.join(out, f"cmdline-{arch}.img")
        if os.path.exists(img): os.unlink(img)
        if subprocess.run(["cp", "-c", src, img]).returncode != 0:
            shutil.copyfile(src, img)
        subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, cmdline],
                       check=True, stdout=subprocess.DEVNULL)
        os.environ["LEANDROS_DISK_IMAGE"] = img
        print(f"runtests: cmdline {cmdline!r}", flush=True)
    print(f"runtests: arch={arch} accel={accel} gpu={gpu} scale={scale} "
          f"timeout={timeout:.0f}s repeat={repeat} out={out}", flush=True)

    summary = {"arch": arch, "accel": accel, "gpu": gpu, "runs": []}
    all_ok = True
    for run in range(repeat):
        rdir = os.path.join(out, f"run-{run}") if repeat > 1 else out
        os.makedirs(rdir, exist_ok=True)
        entry = {"run": run, "boot": None, "results": []}
        summary["runs"].append(entry)
        if driver._qemu_pid() is not None:
            subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
        booted = False
        if cmds and all(self_boots(c) for c in cmds):
            booted = True  # every command boots its own VM: runtests boots none
        start = [sys.executable, DRV, "start", arch, mode] + ([f"--{gpu}"] if gpu != "none" else [])
        for attempt in range(2):
            if booted:
                break
            r = subprocess.run(start, capture_output=True, text=True, timeout=900)
            if r.returncode == 0:
                lg = subprocess.run([sys.executable, DRV, "login", "root", "root",
                                     str(int(60 * scale))],
                                    capture_output=True, text=True, timeout=900)
                if lg.returncode == 0:
                    booted = True
                    break
                print(f"run {run}: login failed (attempt {attempt}): {lg.stdout[-300:]}{lg.stderr[-300:]}", flush=True)
            else:
                print(f"run {run}: boot failed (attempt {attempt}): {r.stdout[-300:]}{r.stderr[-300:]}", flush=True)
                try:  # QEMU's own error (missing image, lock, bad drive) is what explains an instant death
                    with open(driver.QEMU_STDERR_LOG, "rb") as ef:
                        print("[qemu-stderr] " + ef.read()[-600:].decode(errors="replace").strip(), flush=True)
                except OSError:
                    pass
            subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
        entry["boot"] = "ok" if booted else "failed"
        if not booted:
            all_ok = False
            continue
        miss_seen = False
        with open(os.path.join(rdir, "results.txt"), "w") as res:
            for c in cmds:
                try:
                    if self_boots(c) and driver._qemu_pid() is not None:
                        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
                    if c.startswith("host:"):
                        r = host_run(c, timeout, arch, scale)
                    else:
                        r = driver.serial_run(c, timeout=timeout, arch=arch)
                except Exception as e:  # noqa: BLE001
                    r = {"rc": None, "status": f"error {e}", "output": "", "secs": 0}
                fails = find_fails(r["output"])
                rc = r["rc"] if r["rc"] is not None else "?"
                ok = r["rc"] == 0 and r["status"] in ("ok", "rc-file")
                # Belt and braces for session/gate scripts: a RESULT line that
                # reports a failed gate is a failure whatever the exit status.
                if ok and c.startswith("host:") and re.search(
                        r'RESULT.*("pass": false|\bFAIL\b)', r["output"]):
                    ok = False
                    r["rc"] = 1
                    fails.append("RESULT-gate")
                all_ok &= ok
                if fails:
                    fcount = f"{len(fails)} {','.join(fails)}"
                elif not ok:
                    fcount = "? rc-only"  # failed with no parsable FAIL line
                else:
                    fcount = "0"
                print(f"=== {c}: RC={rc} [{r['status']}, {r['secs']}s] fails={fcount}", flush=True)
                res.write(f"=== {c}: RC={rc} [{r['status']}, {r['secs']}s]\n{r['output']}\n")
                res.flush()
                entry["results"].append({"cmd": c, "rc": r["rc"], "status": r["status"],
                                         "secs": r["secs"], "fails": fails})
                # sig_kick guard: any writer of signal state that forgot sig_kick
                # shows up as `miss` > 0 in /proc/rqhold (cumulative since boot).
                # Checked after each command that ran on this VM; the first
                # command after which it is non-zero fails the run.
                if not miss_seen and not self_boots(c) and driver._qemu_pid() is not None:
                    try:
                        q = driver.serial_run("grep '^# sigfast' /proc/rqhold", timeout=60, arch=arch)
                        m = re.search(r"\bmiss (\d+)\b(?: miss_pid (\S+))?", q["output"])
                        if m and int(m.group(1)) > 0:
                            miss_seen = True
                            all_ok = False
                            print(f"=== sig_kick guard: FAIL miss={m.group(1)} (after {c}): {q['output'].strip()[-200:]}", flush=True)
                            res.write(f"=== sig_kick guard: FAIL after {c}\n{q['output']}\n")
                            entry["results"].append({"cmd": "sig_kick-guard", "rc": 1, "status": "ok",
                                                     "secs": 0, "fails": ["sig_kick_miss"]})
                    except Exception:  # noqa: BLE001
                        pass
        if os.path.exists(driver.SERIAL_LOG):
            subprocess.run(["cp", driver.SERIAL_LOG, os.path.join(rdir, "serial.log")])
        if os.path.exists(driver.SERIAL_FULL_LOG):
            subprocess.run(["cp", driver.SERIAL_FULL_LOG, os.path.join(rdir, "serial-full.log")])
            # A shared interrupt line masked by the kernel's storm guard starves
            # every device on it (L5.13): treat it as a failure of the run.
            with open(os.path.join(rdir, "serial-full.log"), "rb") as lf:
                full = lf.read()
            storms = re.findall(rb"\[IRQ\] line \S+ fired \d+ times[^\n]*", full)
            # init's stray sweep after a greeter exit must not take sshd with
            # it: its tokio threads report init as their parent (lane pihw2).
            if re.search(rb"sshd exited \(status 9\)", full):
                all_ok = False
                print("=== kernel-log: sshd was SIGKILLed (init stray sweep?)", flush=True)
            if storms:
                all_ok = False
                print(f"=== kernel-log: {len(storms)} IRQ line(s) masked by the storm guard: "
                      f"{storms[0].decode(errors='replace')[:200]}", flush=True)
        if keep and run == repeat - 1:
            break
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    with open(os.path.join(out, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)
    # Per-command tally over all runs.
    print("--- summary")
    for c in cmds:
        rows = [x for e in summary["runs"] for x in e["results"] if x["cmd"] == c]
        good = sum(1 for x in rows if x["rc"] == 0 and x["status"] in ("ok", "rc-file"))
        unknown = sum(1 for x in rows if x["rc"] is None)
        print(f"{c}: {good}/{len(rows)} RC=0, {unknown} unknown")
    bad_boots = sum(1 for e in summary["runs"] if e["boot"] != "ok")
    print(f"boots failed: {bad_boots}/{repeat}; overall: {'PASS' if all_ok else 'FAIL'}")
    return 0 if all_ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
