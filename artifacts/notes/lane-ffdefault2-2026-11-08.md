# lane/ffdefault2 2026-11-08 - DELIVERED (sandboxes on by default)

1. /etc/ld.so.conf and /etc/localtime (54-byte UTC TZif) now ship on disk (scripts/mkfs-f2fs-populated.py etc_files); the dead
   RamEntries in servers/vfs/src/lib.rs are removed. Guest check (ffsandbox SBETC lines): `/lib /usr/lib /usr/local/lib`,
   `-rw-r--r-- 1 root root 54 /etc/localtime`, both arches.
2. Default flip reapplied from e4f00680 (firefox.sh, leandros-prefs.js, docs/userland/ports.md).
3. Socket process namespaces: NOT a deviation. The findings read SandboxLaunch.cpp on `main`, which has no Socket case. The
   FIREFOX_150/149 release branches have `case GeckoProcessType_Socket: if (level>=1) { canChroot = true; flags |= CLONE_NEWIPC; }`
   (+ CLONE_NEWUSER implied by canChroot): user+ipc namespaces, shared net, chroot - exactly what we observe. ffsandbox now prints
   each process's trailing args (type is the last arg) and user/net/ipc ids; the process with "... 1 socket" is the socket process.
4. Acceptance: ffsandbox `--mode on` on=True, video progressing (SBV 30 a64 / 26 x86 lines), 0 Sandbox log lines, both arches
   (x86_64 on Mac TCG). `--mode off` aarch64: runs, video=True (RESULT off=False is by design). The prior /usr/local/lib EACCES is gone.

Per-process (on): all children Seccomp 2, filters 1, NNP 1, root /.leandros-void; parent and forkserver Seccomp 0, NNP 0, root /.
| type | aarch64 pids | x86_64 pids | user | net | ipc (vs parent) |
| socket | 836 | 544 | differs | shared | differs |
| rdd | 885 | 588 | differs | differs | shared |
| utility | 989 | 709 | differs | differs | differs |
| tab | 876,939,997,1016 | 581,629,679,721 | differs | differs | differs |
| forkserver | 845 | 556 | same | same | same |
