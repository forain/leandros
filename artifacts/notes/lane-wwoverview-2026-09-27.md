# Lane wwoverview — 2026-09-27 → 2026-10-01

**Task:** find out why the weak-window cosmic-comp patch (`ports/cosmic-comp/0001-toplevel-info-weak-window.patch`, lane weakwindow) made cosmic-workspaces panic, and settle the host GPU context-loss question.

**Branch:** `lane/wwoverview` from `cbe99478`. Head is **`ca4e7fc`** (pushed). Worktrees are removed.

## Verdict

**Symptom 1 (ws panic): FIXED. The weakwindow patch did not cause it.**
- The cause was our kernel: AF_UNIX stream buffers were a fixed 4 KiB.
- No COSMIC source was patched. The comp patch is unchanged.

**Symptom 2 (host GPU context loss): REFUTED as patch-related.**
- It happens with the unpatched comp too.
- It also happens with no other QEMU running.

## Root cause of `screencopy.rs:75 conn.flush().unwrap()` → `WouldBlock`

`servers/net` gave each AF_UNIX direction a fixed 4096-byte ring. Linux allows about 208 KiB (`net.core.wmem_default`) before EAGAIN.

To measure it, I added temporary instrumentation (not committed) that logged every stream-send EAGAIN, plus a dump of the Wayland message headers on hot connections. It showed:

| Sender | Peer | Events in one 300 s run | Detail |
|---|---|---|---|
| cosmic-workspaces | cosmic-comp | 7–172 | Ring full while the overview is open. Comp had last read 0.3–12 ms earlier (`since_read_us`). |
| cosmic-panel | cosmic-comp | — | Same pattern. |

- Comp was not hung. The overview writes 30–45 KB/s: `damage_buffer` runs, subsurface and viewport commits, and capture requests. That fills 4 KiB within a few ms of comp not reading.
- When the screencopy thread-pool `flush()` lands on a full ring, it gets EAGAIN and its `unwrap()` panics.

**The unpatched comp panics too.** On 2026-09-28, run `unp1`: unpatched comp, same tree, mixed-use loop, `screencopy.rs:75 WouldBlock`. The weakwindow lane's "0/4 unpatched" was luck with a rare event (about 1 per 2–3 runs).

## Fix

**Commit `1a29378`, `servers/net/src/lib.rs`:**
- `UnixRing.buf` is now a `Vec`. It starts at 4 KiB and doubles on demand, copying out the queued bytes in order.
- It is capped at `RING_MAX = 212992` and freed once it drains.
- If allocation fails, the current buffer is kept.
- poll's writable space (`space()`) and the prefault budgets use the cap.

**Commit `ca4e7fc`:** scmtest `full_ring_eagain` now expects exactly 212992 bytes accepted, then EAGAIN.

## Evidence

All runs use virgl, and the comp is patched unless marked otherwise.

**aarch64/HVF on the Mac:**
- **Instrumented ring-fix image, 2 mixed runs:** 0 EAGAIN events, against 7–172 per run on the 4 KiB ring.
- **Mixed-use loop (5 min; minimize/restore, close from overview, workspace switch, launcher search + Enter):**
  - `mixA` and `mixC`: 4/4 rounds each.
  - **0 ws panics and 0 comp panics** across 5 runs (mixA/B/C, mixI1/I2). mixB and mixI2 lost QEMU to the host GPU after 2 rounds.
- **Overview stress, 8 cycles:**
  - `ovA`, `ovB`: 0 panics.
  - `ovC`: QEMU died to GPU context loss.

**x86_64/KVM on the linux desktop (image built on the Mac, run with KVM):**
- Mixed loop: `xmixC`, `xmixD` 4/4 rounds.
- Overview stress: `xovA`, `xovB` 8/8 cycles.
- 0 ws panics, 0 comp panics, 0 context loss.

**Regression suites (both arches):**

| Suite | Result |
|---|---|
| polltest | rc=0 (`poll_timeout_wake_latency` PASS) |
| epolltest | 11/0 |
| wakepolltest | 45/0 |
| vfstest | rc=0 |
| ptytest | rc=0 |
| scmtest | all PASS except `scm_import_emfile_single_release` |
| scmtest `full_ring_eagain` | total=212992, then EAGAIN |

- `scm_import_emfile_single_release` fails **before this lane too**. Its `dup` loop stops at 256, and `63fd24e` raised the VFS fd table to 512, so it never reaches EMFILE (dup errno=0, rfd=261). This is a test-bound bug and is not touched here.

## Leak numbers (comp fence eventfds, start→end)

| Run | Patched | Unpatched |
|---|---|---|
| aarch64 overview ×8 | 22→32, 22→30 | 22→50, 22→50 |
| aarch64 mixed | 23→35, 22→31 | 21→39 |
| x86 KVM overview ×8 | 21→42, 20→35 | 22→54 |
| x86 KVM mixed | 21→24, 21→24 | 23→32 |

- The leak fix still holds.
- The x86 overview remainder is larger than on aarch64.
- Per weakwindow, this remainder is held by the open capture sessions. I did not re-verify that it is released when the overview is reopened.

## Host GPU context loss (ANGLE/Metal `EGL_CONTEXT_LOST`; QEMU often dies)

Tally of runs on the Mac, aarch64 HVF:

| comp | Runs | With context loss | QEMU died |
|---|---|---|---|
| Patched | 13 | 8 | 6 |
| Unpatched | 7 | 3 | 3 |

- The unpatched runs include **`ovU1`, with 0 other QEMUs running**: 65 context-lost lines, and QEMU died.
- Other QEMUs (other lanes and the user's GPU QEMU) were running in most runs. They varied between 0 and 3, with no correlation.
- **x86 KVM on the linux desktop: 0 context losses in 7 runs.** This includes the unpatched `xovU1` and `xmixU1`; `xovU2` failed to start.
- Conclusion: this is Mac host-side ANGLE/Metal flakiness under overview capture load. It is neither the weak-window patch nor this lane.

## What the user must rebuild

1. Merge `lane/wwoverview`. It touches the kernel net server and scmtest only.
2. Run `./scripts/build-all.sh` for both arches.
3. cosmic-comp does **not** need rebuilding. The staged `m6-session-bins/out/cosmic-comp-*` are byte-identical to before (sha checked).

## Housekeeping and open items

- **Laptop worktree:** `172.16.149.179:~/Projects/leandros-wwoverview` was created on 09-28 and could not be removed, because the laptop is unreachable. Remove it, and its local branch `lane/wwoverview`, when the laptop is back.
- **Desktop disk:** the desktop `samsung970pro512` disk is 100% full. I could not build on the desktop at all: relibc failed with ENOSPC. That is why the x86 image was built on the Mac.
- **Unpatched cosmic-comp tree:** `m6-session-bins/src/cosmic-comp/target/*/release/cosmic-comp` currently holds the unpatched builds from the comparison runs. The patch is applied in the source tree. `ports/cosmic-comp/build.sh` rebuilds and re-stages correctly.
- **Shared file touched:** `servers/net/src/lib.rs` (AF_UNIX rings).
- **Lost scratch:** a Mac reboot on 09-28 → 10-01 wiped the scratch. The 09-28 figures above (`unp1`–`unp3`, `mix2`–`mix6`, `pat1`) are from my session notes, not saved files.
