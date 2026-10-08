# Lane privcheck — 2026-10-02

Branch `lane/privcheck` (base origin/main `1b23677`). Machine: linux desktop (x86_64 KVM) for all
development; Mac HVF for the final aarch64 check.

## Method

`userland/privtest/privtest.c` is one plain C file. It builds for the host (`cc privtest.c`) and for
the guest (`zig cc -target <arch>-linux-musl -static`, which `scripts/build-userland.sh` does; it is
installed as `/bin/privtest`). It runs in two modes:
- As a normal user, it runs the unprivileged cases. The "other user" target is pid 1.
- As root, it runs the root positives first. It then forks a root "victim" in the caller's session,
  plus a root pgrp leader, and creates root-owned files in sticky `/tmp`. Finally it drops to
  uid 1000 with no supplementary groups and runs the unprivileged cases against those targets. It
  also runs a mixed-pgrp kill(-pgrp) case and a kill(-1) case as uid 4242.

Every expectation was checked first on the desktop host (Arch, kernel 7.2.7) as forain and under
sudo:
- Host: user 94 PASS, 4 SKIP (need root mode); root 123 PASS, 1 SKIP (the host has no /dev/dri/card0).
- Guest x86_64 after the fix: user 94 PASS (same set), root 124 PASS. `diff` of the sorted
  result lines between host and guest: user identical. For root, the only difference is
  card0 SKIP→PASS.
- Guest x86_64 before the fix (baseline origin/main): 62 FAIL in root mode.

Note: there is no `su` and no `grep` in the guest. The user run is a serial login as leandro.

## Before → after (unprivileged caller unless noted)

| syscall / object | before | after (= Linux) |
|---|---|---|
| kill(pid of other uid, sig / 0) | delivered / 0 | EPERM |
| tkill/tgkill other uid | ESRCH (bogus) | EPERM; tgkill with wrong tgid → ESRCH, tgid≤0 → EINVAL |
| tgkill(.., 0) as root | EINVAL | 0 |
| rt_sigqueueinfo / rt_tgsigqueueinfo | ENOSYS | implemented; perm check; si_code≥0 or SI_TKILL to another process → EPERM; SI_QUEUE sigval delivered |
| SIGCONT to a root process in own session | ESRCH | 0 (session exemption) |
| kill(-pgrp) of another uid | delivered / ESRCH on probe | EPERM; mixed group → 0, only permitted members hit |
| kill(-1) | ESRCH if nothing permitted | 0 when any other process exists (kill_something_info) |
| getsid(pid) | always the caller's sid | sid of pid / ESRCH |
| mount (non-remount), umount2, pivot_root | EINVAL/ENOENT (unchecked) | EPERM (root checked first); chroot was already EPERM |
| setreuid / setregid | ENOSYS | implemented with Linux rules |
| setuid/setgid/setresuid/setresgid/setgroups | already correct | unchanged (now tested) |
| sethostname / setdomainname | ENOSYS | EPERM; root sets uname nodename/domainname |
| acct, vhangup, swapon/off, init/finit/delete_module, kexec_load/file_load, iopl/ioperm (x86) | ENOSYS | EPERM (root: ENOSYS) |
| capget | all caps for everyone | euid 0: bits 0..40; others: none; v1/v2/v3 headers, bad version → EINVAL + v3 |
| capset | 0 | raise → EPERM; empty → 0; root → 0 |
| setpriority other uid | ESRCH/applied | EPERM (owner = target ruid/euid) |
| setpriority lowering nice | EPERM | EACCES (RLIMIT_NICE, default 0) |
| sched_setscheduler/setparam | 0 (no check) | FIFO/RR → EPERM unless RLIMIT_RTPRIO (default 0) or root; other owner → EPERM; bad policy/prio → EINVAL. Still nothing stored. Note: musl's wrapper is an ENOSYS stub, so the test uses raw syscall |
| sched_setaffinity other uid | 0 | EPERM |
| setrlimit/prlimit | ignored, all ∞ | stored per process (fork-inherited, kept over exec); raise hard → EPERM; cur>max → EINVAL; another uid's limits → EPERM. Defaults are still ∞, except NICE/RTPRIO = 0 |
| setpgid | anyone could move any pid | not own/child → ESRCH; child in another session or session leader → EPERM; foreign-session group → EPERM; pgid<0 → EINVAL |
| link(2) to another user's 0600 file (sticky /tmp, f2fs) | 0 | EPERM (protected_hardlinks, tmpfs and f2fs) |
| chmod/chown of /tmp (tmpfs mount root) | ENOENT | EPERM (root: no-op only if unchanged) |
| open /dev/vd* /dev/loop* /dev/loop-control | **opened: uid 1000 read /dev/vdb raw** | EACCES (root:disk 0660 enforced at open and access) |
| /dev/input/* , /dev/dri/card*, /dev/fb0 | 0666 | root:input / root:video 0660; renderD*, pipewire stay 0666. cosmic-greeter added to video+input in /etc/group (leandro already was) |
| bind TCP/UDP port <1024 | 0 | EACCES |
| socket(AF_INET, SOCK_RAW) | 0 | EPERM; `ping` falls back to SOCK_DGRAM/IPPROTO_ICMP (which is now excluded from the UDP sendmsg/recvmsg path) |
| /proc/<pid>/status Uid/Gid | always `0 0 0 0` | real/effective/saved/fs ids |
| mknod chr/blk | EPERM | unchanged (already right) |
| settimeofday/clock_settime, chroot, reboot | EPERM | unchanged |
| chmod/chown/utimes/sticky unlink+rename of others' files | covered by permtest | unchanged, re-tested |

## Evidence

x86_64 KVM (desktop), final build:
- privtest root 124/0 and user 94/0.
- As root: vfstest, sigtest, polltest, jobtest, exectest, permtest and f2fstest all rc=0.
- As leandro: sigtest, polltest and jobtest rc=0.
- ping works as root and as leandro (4/4).
- COSMIC session (virgl): greeter, then leandro. 23 session processes, all cosmic-* at Uid 1000
  (proven by the new /proc status). Super+T opened 4 of 4 terminals and all closed. cosmic-settings
  ran. Screenshot: 4 terminals at a `$` prompt.
- poweroff lane harness `gui-restart`: `reboot requested by pid 232 (uid 1000): authorised`.
  All processes stopped in 55 ms, clean f2fs unmount on both volumes, marker present after reboot.

aarch64 HVF (Mac), same commit `e1ee68a`:
- privtest root 122/0 and user 92/0. Those are the x86 sets minus iopl/ioperm, which are x86-only.
- As root: vfstest, sigtest, polltest, jobtest, exectest and permtest rc=0.
- As leandro: sigtest, polltest and jobtest rc=0. `head /dev/vdb` → Permission denied.
- COSMIC session (virgl, Mac GPU QEMU): 26 processes, cosmic-session/comp/panel at Uid 1000.
  4 of 4 terminals opened and closed; cosmic-settings ran.
- `ping 10.0.2.2` as ROOT printed its header and then hung waiting for replies on the Mac (slirp).
  The root ping path (SOCK_RAW, sendto/recvfrom) is untouched by this lane, so this is believed to
  be host networking, not a regression. Not A/B'd. On x86_64/KVM it is 4/4 for root and for
  leandro (the leandro run used the new DGRAM fallback).

One permission-flavoured session log line: smithay `Failed to destroy old mode property blob:
Operation not permitted`. It comes from the greeter's compositor at boot. The DRM blob-destroy path
consults no credentials; it is drm_errno's `_ => -1` fallback. I believe it is pre-existing, but I
did not A/B it.

## Open

- setsid() still succeeds for a process-group leader (Linux: EPERM). Not changed: there is
  session-startup risk and no privilege gain.
- The private IPC syscalls (SYS_IPC_SEND to any port id), SYS_SPAWN and SYS_DBG_SERIAL_WRITE have no
  credential checks.
- BLKPG and loop ioctls are only gated by the device open (root:disk). Linux additionally wants
  CAP_SYS_ADMIN for BLKPG.
- mknod S_IFSOCK is EPERM for everyone; Linux allows it unprivileged.
- No real capability sets: a root capset drop is accepted but has no effect.
- The rlimits are stored but not enforced, except NICE/RTPRIO.
- No ptrace exists (ENOSYS), so there is no hole.
- protected_regular / protected_fifos (O_CREAT on another user's file in sticky /tmp) are not
  implemented.
- `ls /dev/input` fails even though event nodes open (pre-existing).
