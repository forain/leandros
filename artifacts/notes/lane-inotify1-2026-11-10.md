# lane/inotify1 (L1 inotify) - 2026-11-10

Status: DELIVERED behind a knob. Default `vfs.inotify=0` keeps the old stub.

## Evidence
- Knob off, `--suite inotifyforce`: 9 real cases FAIL against the stub
  (t_create, t_delete, t_move_pair, t_moved_to, t_create_waitfile, t_layout,
  t_einval_small, t_fionread, t_rmwatch_ignored). This shows the guards bite.
- `--suite inotify --cmdline vfs.inotify=1`: PASS.
- Full aarch64 box regress with default knob (0): see merge commit message / orch log.

## Not done
- Per-(userns, kuid) limits are NOT done (L4). Required before L5 flips the default to 1.
