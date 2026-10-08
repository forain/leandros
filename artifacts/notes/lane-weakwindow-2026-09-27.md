# Lane weakwindow — 2026-09-27

Task: port the `weak_window` fix for the closed-window texture leak to cosmic-comp. This is a COSMIC patch that the user approved as an exception, for this leak only. It follows lanes compfdleak and smithayfix.

Branch `lane/weakwindow` from `b03939ce`, head `828d50fd`, pushed. Worktree removed.

## Verdict: the leak is FIXED on both arches. Open risk: two cosmic-workspaces symptoms seen only with the patch (see Regressions).

## What changed (commit 828d50fd)
- `ports/cosmic-comp/0001-toplevel-info-weak-window.patch`, against cosmic-comp `dec1ee86` (3 files, +61/−17):
  - `toplevel_info::Window` gains `type Weak`, `downgrade()` and `upgrade()`. `CosmicSurface` implements them with the existing `WeakCosmicSurface`.
  - `ToplevelHandleStateInner.window` changes from `Option<W>` to `Option<W::Weak>`. `from_window()` stores `downgrade()`, and `window_from_handle()` upgrades.
  - `toplevel_management.rs`: its 12 `window_from_handle(..).unwrap()` sites become `let Some(window) = .. else { return; }`, so a request on a stale handle is ignored.
  - smithay is not patched. `efeb597` already has `WeakWindow`, and the #1921 smithay patch is dropped.
- `ports/cosmic-comp/build.sh <arch>..`: applies the patch idempotently to `leandros-artifacts/m6-session-bins/src/cosmic-comp`, builds with the m6 toolchain (`--no-default-features`), checks that the binary exists, and stages it at `m6-session-bins/out/cosmic-comp-<arch>`.
- `ports/cosmic-comp/README.md`: upstream links (Smithay#1562 and cmeissl's comment, cosmic-comp#2084, MartinKavik's branch) and "remove when upstream merges".
- `scripts/mkfs-f2fs-populated.py`: `LEANDROS_COSMIC_COMP=<file>` wins. Otherwise it uses `m6-session-bins/out/cosmic-comp-<arch>`, then `m3-gl-stack/out`. The mkfs log prints which binary it packed.

Staged binaries: `~/code/leandros-artifacts/m6-session-bins/out/cosmic-comp-{aarch64,x86_64}` (both built rc=0).
- The source tree `m6-session-bins/src/cosmic-comp` is left with the patch applied. `build.sh` is idempotent.

## Leak numbers: comp fence eventfds / comp GEM handles (`[DRMH]` comp open), cosmic-term open/close cycles
| run | N=0 | 1 | 4 | 8 | 16 | idle30 | +1 extra |
|---|---|---|---|---|---|---|---|
| aarch64/HVF virgl, **patched** | 22/29 | 22/29 | 21/28 | 21/28 | **21/28** | 21/28 | 21/28 |
| aarch64/HVF virgl, patched, rerun (fence only) | 22 | — | 22 | 21 | **21** | 21 | 21 |
| aarch64/HVF virgl, unpatched, same tree (b03939ce) | 20/27 | — | 24/31 | — | — | — | — |
| aarch64/HVF virgl, unpatched (compfdleak) | 21/28 | 22/29 | 25/32 | 29/36 | 37/44 | | |
| x86_64 **TCG** virgl (Mac), **patched** | 23/30 | 23/30 | 22/29 | **21/28** | — | 21/28 | 21/28 |
| x86_64 KVM virgl (laptop, compfdleak), unpatched | 15/23 | 17/23 | 21/26 | 25/30 | | | |

- In every run, all terminals really opened: 16/16 lines in `/tmp/typed` on aarch64 and 8/8 on x86_64.
- **x86_64 was only possible under TCG on the Mac**: the laptop and the desktop were unreachable (LAN and Tailscale time out). So Venus was not re-measured, and x86_64 ran 8 cycles instead of 16.

Overview-heavy use (8 cycles: open 2 terminals, close one from the overview, Super+Q the other). Figures are fence / GEM:

| build | c0 | end |
|---|---|---|
| unpatched | 22/29 | 59/57 |
| patched | 22/29 | 34/39 |

- The patched remainder is held by the running cosmic-workspaces' capture sessions.
- It is released when the overview is reopened: 34/38 went to 29/29.
- It does not accumulate.

## Regressions (5-minute mixed-use pass, both arches)
Paths exercised:
- minimize (title bar), then restore from the panel's minimize applet (toplevel_manager unminimize/activate)
- the overview, and close from the overview ✕ (toplevel_manager Close)
- workspace switch, from both the overview thumbnails and Super+1/2
- the launcher's window search: the pop-launcher `cosmic_toplevel` result, then Enter (Activate)
- **stale-handle Activate**: the launcher lists the window, the terminal is killed from the shell, then Enter. comp survives with the same pid on both arches, and the request is ignored.
- `window_from_handle`'s `None` path. Unpatched, it would panic via `unwrap()` on an `empty()` handle.

Results:
- **0 cosmic-comp panics** in `/var/log/greetd.log` on both arches.
- The same comp pid for the whole session.

Suites:
- drmsmoke failed=0 on aarch64 (HVF) and x86_64 (TCG).
- polltest RC=0 on aarch64.
- Session smoke: greeter login → COSMIC → terminals, OK on both arches.

Two symptoms were seen only with the patch. Small samples:
1. **cosmic-workspaces panics**: `screencopy.rs:75 conn.flush().unwrap()` returns `WouldBlock` (EAGAIN), followed by PoisonError and SendError. Then cosmic-session restarts it, and the overview works again.
   - Patched: 3 occurrences (aarch64 5-minute pass ×2, x86_64 ×1).
   - Unpatched: 0 in 4 runs.
   - Mechanism:
     - With the fix, the closed window is really dropped. So its image-copy `Session`s drop, and smithay sends `failed` and `stopped` to cosmic-workspaces. Before the fix this never happened, because the window leaked.
     - That exercises a client path where a thread-pool capture `flush().unwrap()` hits EAGAIN.
     - This is a client bug (it unwraps a non-fatal error). Our unix-socket EAGAIN behaviour may make it likelier than on Linux. This is unverified.
2. **Host GL context loss** (ANGLE/Metal `EGL_CONTEXT_LOST`, then vrend GL errors 1286/1287) in 2 of 4 patched overview-stress runs.
   - One was fatal: QEMU exited in cycle 7.
   - It was seen in 0 of 2 unpatched runs whose QEMU stderr was saved.
   - Not seen in the patched 16-cycle terminal-only rerun (`a64-virgl2`, stderr saved), so it appears tied to overview capture use, not to the window-close path alone.
   - It could be host flakiness: the user's own GPU QEMU was running concurrently. It could also be the new path where comp releases the last reference to a dead client's imported resource. Not settled.

## Not done / open
- The x86_64 KVM and Venus re-measurement, once the laptop is reachable.
- Settle symptom 2 with more runs, or with a host-side trace.
- Symptom 1: consider a cosmic-workspaces patch (handle `WouldBlock` in `attach_buffer_and_commit`). That would be another COSMIC patch and needs the user's approval.
- Sibling leaks from MartinKavik's report were not taken: the shader cache and `pending_activations`, both CPU-RAM only.

Scratch: `scratchpad/ww/` holds the run logs, screenshots, `cycle.py`, `wwlib.py`, `ovtest.py` and the QEMU stderr copies.
