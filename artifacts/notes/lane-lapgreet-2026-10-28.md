# lane lapgreet 2026-10-28: first greetd login after boot sometimes fails

Status: PARTIAL. Instrumentation merged, failure NOT reproduced; cause still open.

## Delivered (diagnose-first, no behaviour change)
- init: greetd.log keeps 4 boots (`greetd.log`, `.1`..`.3`), rotated when the logger starts; each file capped at 4 MB
  (past the cap the file restarts instead of renaming, so kept boots survive); `=== boot (uptime N s) ===` header.
- ports/greetd/patches/0004-trace-leandros.patch: `greetd: ipc <type>` (+ `cmd=` argv0), `greetd: session exited status=.. after N ms`,
  `greetd: greeter exited after start_session (ran N ms); starting session`.
- pam-leandros: `pam-leandros: auth user=.. rc=.. in N ms` (no secret).
- start-cosmic-leandros: `start-cosmic: <uptime> step ...` lines (begin, home ok, runtime dir ok, exec).
- greeter-env: `RUST_LOG=info,cosmic_greeter::time=off`.
- kernel: klog `drm: SET_MASTER EBUSY open_id N (holder open_id M)` and `drm: master released open_id N (last close)`, 8 lines/boot.
  (pid/comm of the holder is not available at that site; open_id only.)

## Laptop evidence (trial 51b2a3192339 on main d78a0844, userland pushed with deploy-files, originals restored)
- Run 1 (3 boots, logins ~150 s after greeter start because the readiness poll was broken: no grep on the laptop): 3/3 success.
- Run 2 (4 boots, `greetd-login` launched 1 s after the greeter line, boot uptime ~14 s): 4/4 reach cosmic-comp + cosmic-panel as uid 1000 within 6 s.
- Healthy order in every boot: `create_session, cancel_session, create_session, post_auth_response, auth rc=0 in ~100 ms,
  start_session cmd=/bin/sh`, greeter exit, `start-cosmic: begin / home ok / runtime dir ok / dbus-run-session`,
  `drm: master released open_id 6 (last close)` BEFORE start-cosmic runs. No `SET_MASTER EBUSY` in any boot.
- H-A (DRM master race): not seen in 7 boots. H-B (home/runtime dir not ready): refuted for scripted logins (both ok at step time).
  H-D (PAM first call): refuted (rc=0 in ~100 ms every time).
- H-C (greeter-side race: submit before the session list loads) is NOT exercised by `greetd-login` (it speaks the IPC directly and
  supplies its own command). It remains the best-fitting open hypothesis; it needs a real GUI first login, or a trace of what
  cosmic-greeter sends (`greetd: ipc start_session cmd=` is now logged, so the next failed GUI login shows `<empty>` or an error).

## Next
Leave the instrumented build in place; after the user's next failed first login read `greetd.log.1..3` (`greetd: ipc` lines, `cmd=`, `exited status`).
If it is an empty/odd cmd: fix per design H-C (cosmic-greeter Submit disabled until sessions load).

## Process notes
- The old init O_TRUNCs greetd.log at logger start: booting `good` (old init) after a trial destroys the trial's log. Pull the log before returning to good.
- The laptop has no grep/dmesg (brush); filter locally.
- greetd.srchash in ~/code/leandros-artifacts/greetd-lane is shared by all worktrees; a concurrent build.sh from another tree makes the image build fail with "stale greetd".
