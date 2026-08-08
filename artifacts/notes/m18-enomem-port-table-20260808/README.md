# "Out of memory" was 64 IPC ports, and the fix is not more RAM

2026-08-08, Linux box (`forain@172.16.158.150`), **x86_64/KVM only**, QEMU 11.0.1,
release builds, fresh f2fs images. aarch64 is the Mac lane's to confirm.

Harnesses: `artifacts/m17_census.py` (unchanged, inherited), `artifacts/m18_regress.py`,
`artifacts/m18_repro.py`. Positive control `nosuchbinary_xyz42` confirmed **failing**
as the first command of every boot in this note.

## The answer

**The ENOMEM that fires is `servers/vfs`'s `call_port`, and the resource it ran out
of is `ipc::port::LIVE_BUCKETS` — 64 IPC ports for the whole system.**

Every task that reaches a server through `call_port` lazily allocates one port as
its reply port and holds it until the task exits. `call_port` is on the path of
**every** operation on a mounted filesystem — `open`, `stat`, `getdents64`, and the
`execve` that opens the binary. When `port::create` returns `None`, `call_port`
returns `-12`, and userspace gets errno 12 and nothing else.

The consumers are **threads**, not processes, which is why `procs=20` in
`sysinfo(2)` looked like plenty of headroom and was not.

## The chain, in one boot, in order, on one console

Serial log of run B (`run2-patched-64buckets-serial.log`), lines 88 / 91 / 98:

```
[IPC] port table FULL: 64/64 live buckets -- port::create now fails, and every caller
[IPC] turns that into ENOMEM; this is ipc::port::LIVE_BUCKETS, not RAM

[VFS] ENOMEM: no reply port for this task -- every call to a mounted
[VFS] filesystem (open/stat/getdents64/exec) now returns errno 12. Not RAM.
...
user page fault RIP=0x000000008ADE5174 CR2=0x0000000000000880 CR3=0x0000000061B28000 err=0x4: task killed
```

The last line is the null dereference the whole investigation started from. The
intermediate step is visible in the session log of the predecessor's run 4:

```
xkbcommon: ERROR: failed to add default include path /usr/share/X11/xkb
```

`xkb_context_new` calls `xkb_context_include_path_append_default`, which `stat`s the
path and returns false if the `stat` fails; `xkb_context_new` then **returns NULL**.
The `xkbcommon` crate wraps it with no null check and SCTK hands the NULL to
`xkb_compose_table_new_from_locale`, which reads `ctx` at offset `0x880`.

So the recorded proximate cause — "a failed `calloc`" — was **wrong**. Nothing
allocated. `stat("/usr/share/X11/xkb")` returned errno 12 because the task could not
get a reply port. The same substitution is visible on the other side: on a healthy
session `cosmic-files-applet` reports `failed to read directory /root/Desktop: No
such file or directory (os error 2)`; under port exhaustion the same call reports
`Out of memory (os error 12)`.

## The measurement that makes it obvious

`ipc::port::PORT_STATS` (committed `false`) prints one line per new occupancy
high-water mark — at most `LIVE_BUCKETS` lines, because the mark only rises.

| run | busd | `LIVE_BUCKETS` | port high water | `port table FULL` | `CR2=0x880` | panel bar |
|---|---|---|---|---|---|---|
| A | stock | 64 | **61/64** at settle (t+48 s), **64/64** at probe (t+228 s) | 0 | 0 | present, clock ticking |
| B | ServiceUnknown | 64 | 64/64 during startup | **1** | **1, then the session wedged** | absent |
| C | ServiceUnknown | 512 | **84/512** | 0 | **0** | present, clock ticking |
| D (final, flags `false`) | ServiceUnknown | 512 | not traced | 0 | **0** | present in all 9 shots |

**A stock session was already living three ports from the ceiling.** That is the
whole finding. Unblocking four more multithreaded iced components — which is exactly
what the busd `ServiceUnknown` reply does — takes it over, during startup.

Run A and run B differ only in the busd binary; run B and run C differ only in
`LIVE_BUCKETS`. Free RAM was 1078-1197 MiB at every sample of every run.

## The mmap/brk hypothesis is refuted too

`kernel::syscall::DBG_ENOMEM` (committed `false`) names every ENOMEM return in the
memory, address-space and exec paths, and `mm::vmm::last_map_fail()` says *why* the
mapping call refused — `vma-overlap`, `buddy-order-unavailable`, `va-overflow`,
`pte-install`. Across two fully instrumented COSMIC sessions:

* **not one** `mmap/*` site fired. Not anonymous, not file-backed, not device, not
  shared-VMO, not `mremap`.
* the only memory-path site that fired at all is `brk/refused reason=vma-overlap`
  (10 lines in run A, 18+ in run B) — and `brk` has no errno. It answers with the
  **old** break, musl compares and falls back to `mmap`, and nothing fails. It is
  noise, not the bug, and it is only visible because `sys_brk` now reports a refusal
  that the ABI otherwise hides completely.

`[SCHED] task table FULL` never fired either, as the predecessor already found.

## The reproducer: no compositor, no Wayland, no D-Bus

`artifacts/m18_repro.py`. `ls` of a directory that certainly exists, before and after
`N` background `sleep`s:

```
brush-0.5# ls /usr/share/X11/xkb
compat	geometry  keycodes  rules  symbols  types
...  (20 background sleeps) ...
[IPC] port table FULL: 24/24 live buckets -- port::create now fails, and every caller
[VFS] ENOMEM: no reply port for this task -- every call to a mounted
brush-0.5# ls /usr/share/X11/xkb
error: failed to execute command 'ls': Out of memory (os error 12)
```

**Caveat, stated rather than hidden:** that run is against a kernel built with
`LIVE_BUCKETS = 24`. At the shipped 512 the shell cannot supply the load — brush runs
out of its own descriptors (`MAX_FDS = 128`, `No file descriptors available (os error
24)`) at roughly 40 background jobs, long before 512 ports. Scaling the constant down
keeps the mechanism and the errno exactly, and costs one boot instead of five minutes
of COSMIC session. A load generator that is not the shell — one process, many threads,
each doing one `open` — would reproduce it at the shipped constant; it is not written.

## The fix, and its falsification

`ipc::port::LIVE_BUCKETS` 64 → 512. A bucket is a 16-deep queue of 440-byte inline
messages, ~7.5 KiB, so the static goes from ~0.5 MiB to ~3.8 MiB of BSS — in a kernel
that already spends ~4.2 MiB on the tmpfs pool. 512 is 6x the measured peak of 84.

**Falsified by mutation twice**, in both directions:

* set it back to 64 with the patched busd and everything returns: `port table FULL`,
  `[VFS] ENOMEM: no reply port`, `CR2=0x880`, no panel bar, session wedged (run B).
* set it to 24 and 20 background `sleep`s are enough to make `ls` say "Out of memory"
  (the reproducer above).

`report_table_full` is **not** gated and stays in the shipped build. It costs two
lines, once per boot, and only when the table is actually full. The next time this is
the ceiling, it says so instead of costing an investigation.

`release_by_owner` also lost a silent cap: it collected closed ports into a fixed
64-entry batch and *tombstoned* the surplus without ever waking its waiters. It now
loops until a pass closes fewer than a batch.

## The busd patch is landable now

`ports/busd/proposed/service-unknown-reply.patch` moves to `ports/busd/`, where
`build.sh`'s `*.patch` glob applies it.

The control is run B against run D: the same patched busd binary, the kernel as the
only delta. Run B loses the panel and takes a null dereference; run D keeps the panel
and its ticking clock across all nine screenshots, takes **0** faults, **0**
`Out of memory (os error 12)`, and still prints `Another instance is running` /
`Successfully activated another instance` — so single-instance behaviour is preserved
and the four components own their APP_IDs.

The session also reaches names it never used to: `org.freedesktop.portal.Desktop` x6
and `org.freedesktop.login1` appear in the census only with the patch, and at
`probe-t45` a **16,509-pixel** region of the screen changes and non-background
coverage moves 0.971 → 0.859 — a hand-started component drawing a window.

**Note for whoever picks this up elsewhere:** `ports/busd/build.sh` only applies
patches when it first extracts the crate, so `rm -rf ports/busd/.work` is required
before the moved patch takes effect, and the staged binary under
`~/code/leandros-artifacts/m5-session-ship/<arch>/usr/libexec/busd` must be rebuilt.
Only the x86_64 binary was rebuilt here.

## Regression

One fresh image, `vfstest` exactly once, each binary read by its own trailer or by
its `: FAIL` lines rather than by counting passes.

| binary | result |
|---|---|
| `vfstest` | 0 FAIL, `--- vfstest done ---` |
| `scmtest` | 0 FAIL, `--- scmtest done ---` |
| `wakepolltest`, `forktest`, `epolltest`, `polltest`, `sigtest`, `timertest`, `memtest` | 0 FAIL each |
| `waittest` | 0 FAIL on the final boot; an earlier boot of the same build showed **1 FAIL, `wait_on_process_group`** — the flake already recorded in the open-issues list, behaving as a flake |
| `venustest` | **`--- venustest done, failures = 0 ---`** |

`venustest` scored `failures = 32` on the first attempt **because that boot had no
`--venus`**, so the `virtio-gpu-gl-pci` device was absent and `host_advertises_venus_capset`
and its dependents failed as designed. An invocation artifact, not a result, and worth
recording precisely because it arrives in the shape of a regression.

The reproducer script run against the **fixed** kernel is its own control: the same
60-job burst there stops at brush's own descriptor limit and reports
`No file descriptors available (os error 24)`, never errno 12, and no kernel ceiling
is named.

## What surprised

1. **The smallest table in the system was the one nobody was looking at.** The search
   was scoped to `mmap`/`brk` by the errno, and the errno was produced by an IPC
   table two layers away from memory management.
2. **`procs` was the wrong denominator.** The instrument the predecessor built to
   measure pressure reports processes; the resource is spent by threads.
3. **The failing `stat` was on a path that exists.** `ls /usr/share/X11/xkb` prints
   six real entries seconds before the same call returns errno 12.
4. **`brk` cannot report failure at all.** It answers with the old break by contract,
   so a heap that will not grow is invisible on the way out — `DBG_ENOMEM` is the
   only way to see it, and it turned out to be the loudest non-event in the log.
5. **A 4 GiB guest is still separately unusable** (`mm::buddy::free` reading an
   unmapped HHDM address). Untouched here; it is not this bug and never was.
