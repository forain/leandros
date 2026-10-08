# Lane coldstart (2026-10-02): REFUTED

**Verdict.** At 7d6b7e4, the slow first launch does not reproduce on either arch. Cold-start cost is not a general problem. First and second launches take the same time to within noise. The "60 s first file-chooser dialog" seen on the Mac comes in at 0.87 s on a fresh image and a fresh boot. No fix was needed. One thing was committed: the measurement harness.

**Branch.** `lane/coldstart` @ `95a040d` (base 7d6b7e4), pushed. It adds `.claude/skills/run-leandros/coldstart-bench.py` and a SKILL.md entry. No kernel, server or image change.

## Method
Each of the 3 boots per arch used a fresh pristine f2fs image (no `~/.cache`) and logged in through the greeter as leandro. The harness then timed the following steps from trigger to the first screen change, by polling VNC to within ±0.3 s:
- **term and settings:** launcher (Super, type, Enter), then the window. Closed with Super+Q.
- **wallpaper:** cosmic-settings, then Wallpaper, then "Add image", which opens the portal FileChooser. This is the user's path.
- **portal:** `leandros-sysbus portal open-file` from the root serial shell.

A digit suffix (`term2`, `wallpaper2`, ...) is the second launch in the same session. Every step also recorded a Ctrl-T dump with `sched::pcsample` on, giving per-process user/kernel/idle samples, plus host QEMU CPU at 1 Hz.

## Numbers (median of 3 boots; first / second launch, seconds)
| | term | settings | Add-image dialog | probe dialog | launcher opens |
|---|---|---|---|---|---|
| Mac aarch64 HVF, virgl gles31 | 1.37 / 1.42 | 1.38 / 1.34 | **0.87 / 0.89** | 1.17 / 1.16 | 0.93 |
| desktop x86_64 KVM, virgl | 1.10 / 1.12 | 1.46 / 1.45 | 0.95 / 0.64 | 1.02 / 1.04 | 1.02 |

- **Mac individual runs.** Add-image dialog 0.86–0.90 s, 9/9 runs including the load boot. Probe dialog 1.10–1.18 s.
- **x86 Add-image dialog:** 0.95–0.96 s.
- **pcs during each step.** The guest is about 92 % idle. The busy processes were cosmic-launcher, cosmic-comp, the launched app and xdg-desktop-portal-cosmic, with nothing spinning.
- **Host load is the only thing that moved the numbers.** On the Mac:
  - Boot 3 overlapped another lane's build (load average about 9). First launches were term 3.32 s and settings 3.67 s.
  - A harness check at load average 45 (several lanes' `build-all` running) gave 3.1 s for both the probe dialog and term.
  - On an idle host the same steps take 1.1–1.4 s.

## Candidates, checked
| Candidate | Result |
|---|---|
| Mesa shader cache missing or unwritable | Works. `~/.cache/mesa_shader_cache` is created, about 630 KB after one session, and persists. |
| fontconfig cache | Not used. cosmic-text/fontdb scan `/usr/share/fonts` in every process, so it is a constant cost with no first-run penalty. |
| Icon theme cache | Same as fontconfig: per-process lookups, no first-run penalty. |
| MoltenVK / ANGLE / Metal host shader compile | No first-run penalty measured. The 1 Hz Metal compiler CPU (summed over every MTLCompilerService) stayed at 0 throughout. The host Metal cache is warm by now. |
| D-Bus activation waits | None on the dialog path. The backend receives FileChooser in the same second the probe sends it. |
| f2fs/page-fault cost of big binaries | First launch equals second launch. |
| The Screenshot portal leaving the system slow (portal-lane order) | Refuted. After screenshot + Allow, dialogs still opened in 1.16 s and 1.13 s. |
| Heavy serial output during the dialog | Refuted. 1.17 s with a concurrent `cat` of greetd.log. |

## What the original 60 s was
In the portal lane's session (pt/a64d, 14:12 UTC), the slow dialog was the **probe** call, not Add-image. Its own log shows the backend running about 10× slow on CPU-only steps:
- 10 s from "No valid locale" to the `/proc/self` tab warning.
- 12 s from window creation to loading the libcosmic translations.

All of these take under 1 s here. In the same session, cosmic-workspaces took 18.7 s for wgpu init, against 0.5–0.7 s at that session's login. So that whole session was slow, not just the first dialog. The most likely cause is host contention: that wave ran many lanes' builds and QEMUs on the Mac. This is consistent with the 2.6–3× slowdowns measured here at load average 9–45.

The portal note's "later dialogs <9 s" came from a different boot (a64e), where the first Add-image dialog was already under 9 s. That 9 s is just its screenshot-poll interval.

## Harness pitfall (cost an hour)
- **The bug.** Do not match the brush prompt with a regex anchored at the end of the buffer.
- **Why it fails.** Kernel `[FORK]` lines print after the prompt, so the match never fires. Every serial command then waits out its full 10 s timeout.
- **What it caused.** A constant "11.0 s probe dialog".
- **The fix.** `coldstart-bench.py` looks for the prompt after the echoed command.

## Verification
- polltest 18 PASS / 0 FAIL, drmsmoke failed=0 (5 skipped), on both aarch64 HVF and x86_64 KVM (no-GPU boots, this build).
- COSMIC sessions: 0 panic, 0 [WDOG] over 7 Mac and 5 x86 sessions.

## Open (not this lane)
- **Exit SEGV on close (both arches).** cosmic-term and cosmic-settings segfault on Super+Q: 0–3 SEGVs per boot, a write to NULL+0x254/0x25c in libwayland `wl_map_insert_at` while the main thread is in munmap/exit_group. This is the residual use-after-free that lane termsegv documented in the app. It is harmless to the timings.
- **Disk space on the desktop.** The desktop's samsung970pro512 is at 98 %. The x86 mkfs ran out of space (ENOSPC), so this lane put its images in `/tmp` (tmpfs, now removed), as lane privcheck does.
- **Data.** In `artifacts/notes/lane-coldstart-files/` (results.json and events.log per boot, `summ.py`).
