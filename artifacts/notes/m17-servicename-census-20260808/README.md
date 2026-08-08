# The census, and why the ServiceUnknown reply is right and still not landable

2026-08-08, Linux box (`forain@172.16.158.150`), **x86_64/KVM only**, QEMU 11.0.1,
fresh f2fs images per boot, release builds. aarch64 is the Mac lane's to confirm.

Harness: `artifacts/m17_census.py` + `artifacts/m6-session-data/m17-census`.
Positive control `nosuchbinary_xyz42` confirmed FAILING as the first command of
every boot.

## 1. The census — the prediction holds, and it was too small

One boot, stock `busd`, stock kernel, a complete session log rather than a tail.

| n | name |
|---|---|
| 6 | `com.system76.CosmicSettingsDaemon` |
| 1 | `com.system76.CosmicLauncher` |
| 1 | `com.system76.CosmicOnScreenDisplay` |
| 1 | `com.system76.CosmicWorkspaces` |
| 1 | `com.system76.CosmicAppLibrary` |
| 1 | `com.system76.PowerDaemon` |
| 1 | `org.a11y.Bus` |
| 1 | `org.freedesktop.UPower` |
| 1 | `org.freedesktop.locale1` |
| 1 | `org.freedesktop.timedate1` |

**All four autostarted single-instance components appear exactly once each**, in
the first 900 ms of session startup — `CosmicLauncher` at t+8.434, the OSD at
t+8.436, `CosmicWorkspaces` at t+8.454, `CosmicAppLibrary` at t+8.635. The
prediction is confirmed and item 8's "blamed wholly on a missing keybinding"
attribution is superseded: all three of the components it names have been parked
in libcosmic's blocking D-Bus probe at startup, every boot, since they were
staged.

(The table shows 1 each because it is the pre-probe half of the run. The
harness's own second measurement re-runs the same four by hand, which is why the
raw log holds 2 of each.)

**The class is bigger than the four.** Six of the ten names are not
single-instance probes at all. Every blocking call to an absent service on this
system hangs the same way, and `com.system76.CosmicSettingsDaemon` is asked for
six times inside 170 ms by components that are themselves part of the session.

**The OSD's APP_ID is `com.system76.CosmicOnScreenDisplay`, not `CosmicOsd`.**
Predicting the wrong string would have scored a real hit as a miss.

## 2. busd's log does not go where the record assumed

`tracing_subscriber`'s `fmt` layer writes to **stdout**. Redirecting only `2>`
captures nothing, and every busd line lands on the console instead — at ~0.19 s
per newline, with busd blocking on each one. The guest half redirects both.

`EnvFilter::from_default_env()` with `RUST_LOG` unset enables **ERROR only**, and
`unknown destination` is a `warn!`. `start-cosmic-leandros` exporting
`RUST_LOG=info` is the only reason that line has ever been seen; busd is started
before that script runs, so the census script exports it itself.

## 3. The patch works. The proof is not a log line, it is an exit status.

With `ports/busd/proposed/service-unknown-reply.patch` staged, a hand-started
second `cosmic-launcher` prints

```
INFO cosmic-launcher (com.system76.CosmicLauncher)
INFO Version: 1.0.12 (release)
INFO Successfully activated another instance
INFO Another instance is running
```

and exits 0. That is only reachable if the **autostarted** copy got through the
same probe, owns `com.system76.CosmicLauncher` and is serving `DbusActivation` —
i.e. the reply unblocks the caller *and* preserves single-instance behaviour,
which `COSMIC_SINGLE_INSTANCE=false` would not. With stock busd the same probe
writes 140 B and never returns.

The census corroborates it from the other side: with the patch the session
reaches names it never used to reach — `org.freedesktop.portal.Desktop` (six
calls), `org.freedesktop.login1`, and more `UPower`/`a11y` — because the
components now get an error and carry on instead of stopping at the first
unowned name.

## 4. And it is still not landable

| run | busd | kernel | `-m` | panel bar | `CR2=0x880` deaths |
|---|---|---|---|---|---|
| 1 | stock | pre | 2G | **present, clock ticking** | 0 |
| 2 | patched | pre | 2G | **absent** | 0 (3x `exec` ENOMEM instead) |
| 4 | patched | instrumented | 2G | absent | **4** |
| 5 | patched | instrumented | 4G | absent | **2** |
| 6 | stock | instrumented | 2G | **present, clock ticking** | 0 |

Run 6 is the control that makes this attributable rather than merely correlated:
same kernel as run 4, same images, the busd binary as the sole delta. It also
shows 0 `[SCHED] task table FULL`, 0 `Out of memory (os error 12)`, `procs` 20 →
24 as the four hand-started probes all start cleanly, and probe stderr byte
counts of 140 / 1317 / 1321 / 1157 — byte-for-byte the run-1 signature of four
processes blocked in the D-Bus probe.

`m17-x86_64-settle2-t22.ppm` from runs 1 and 2 are the photograph: the same
Orion wallpaper, with a panel bar and a legible ticking clock in run 1 and
nothing at all in run 2.

The deaths are the **exact aarch64 signature**, now reproduced on x86_64:
`CR2=0x0000000000000880`, `err=0x4` (user-mode read of a not-present page), the
same low bits of `RIP` (`...174`) at four different library load addresses, and
`sched::exit_group(1)` — which `launch_pad` sees as `failed with code 1` and
restarts. `cosmic-files-applet` is one of them by name.

## 5. Step 2's answer: **memory is not the constraint, and 4 GiB does not help**

This is the headline correction. The recorded hypothesis was that the reply
unblocks four more iced applications into a 2 GiB guest, RAM runs out,
`calloc(1, 2184)` fails, and libxkbcommon's unchecked `xkb_context_new` NULL
becomes a null dereference.

The first half is right and the second half is wrong:

* the guest reports **1234 MiB free** at the moment three `exec`s fail with
  `Out of memory (os error 12)` (run 2), and **1186-1243 MiB free** across the
  whole of run 4, which is where the four null dereferences are;
* **at `-m 4G` the `CR2=0x880` deaths are still there** (run 5). Whatever
  allocation is failing, doubling physical memory does not satisfy it.

`sysinfo(2)` is the instrument — `sys_sysinfo` already filled `totalram`/`freeram`
from the buddy allocator and nothing in the image had ever called it. `/bin/meminfo`
now does, once per phase boundary, because an allocation failure is a question
about the trough and a single reading cannot answer it.

**So "Out of memory" on this system does not mean "out of RAM."** That is the
generalisable lesson, and it has now cost two investigations. `fork`/`clone`
return ENOMEM the moment `runqueue::MAX_TASKS` is reached; `mmap` has a dozen
ENOMEM returns of its own; every one of them reaches userspace as errno 12 and
nothing else. The kernel now says so out loud at one of those sites
(`[SCHED] task table FULL: n/256 ... this is runqueue::MAX_TASKS, not RAM`) —
that print did **not** fire in any run here, so the task table is not the ceiling
either, and the remaining suspects are all in the `mmap`/`brk` path.

**A 4 GiB guest is separately not a usable configuration right now.** Run 3 died
with a kernel page fault in `mm::buddy::free` — `Vector=0xE`, `ErrCode=0` (a
kernel-mode *read* of a not-present page), `RIP=0xFFFFFFFF8014043A`,
`CR2=0xFFFF80000EAC8000`, symbolised with `addr2line` against
`target/final-x86_64/kernel`. The buddy's free lists are intrusive — `next` at
byte 0 and `prev` at byte 8 of the free block itself, read through the HHDM — so
a read fault there is a corrupted or out-of-range link, not exhaustion. It did
not recur in run 5, so it is intermittent; it has never been seen at 2G.

## 6. What this leaves

The proximate cause is not in dispute and is not ours: the `xkbcommon` crate
wraps `xkb_context_new` without a null check and SCTK hands the result straight
through at `wl_keyboard` bind, so *any* allocation failure at that point is a
null dereference rather than an error. What is ours is the allocation that fails
with 1.2 GB free.

Next measurement, and it is cheap: a gated print at every ENOMEM return in
`sys_mmap`/`sys_brk` naming the site. One instrumented boot with the patched busd
then says which one, and the fix follows from that rather than from a guess.

**Do not reach for `COSMIC_SINGLE_INSTANCE=false` as the safe alternative.** It
is inherited by every child of `cosmic-session`, unblocks exactly the same four
components, and therefore carries exactly the same crash — with the added cost
that the apps no longer own their APP_IDs. It is not the cautious option.
