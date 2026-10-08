# Lane macmagenta — 2026-10-01

Branch `lane/macmagenta`, base `origin/main` e66225d, head **9fc4442** (pushed). Worktrees removed.
Harness: session scratchpad `mm/` (`mmlib.py`, `sess_up.py`, `steps2.py` persistent-serial step runner, `soak.py` stall watcher, `fvmag2.py`, `hostprobe/uninit.c`, `hog/efdhog.c`).

## 1. Magenta windows — ROOT-CAUSED; already fixed on main by lane multiterm (no code change here)

**What magenta is.** On this host stack (QEMU gles31 → virglrenderer → ANGLE-Vulkan → MoltenVK 0.2.2210, M4 Max), a texture that was never written reads back as exactly `#FF00FF`. Host probe `hostprobe/uninit.c` shows this for 9/9 allocations (glTexStorage2D, glTexImage2D NULL, BGRA, fresh and reused). So a magenta window or screen means "a buffer nobody rendered into". On Linux hosts the same buffer is black or garbage, which is why it looked Mac-only.

**Why nobody rendered into it.** When a guest virgl context gets a host error, virglrenderer puts the context in the error state and drops every later command it sends. The integ-wave-0924 image (2f4214bd, the one finalverify used) does this at the first Super+W (workspaces overview) in **3/3 boots**. QEMU stderr shows:
- `vrend_set_single_sampler_view: context error reported 1 "leandros-venus" Illegal handle 598/603` and `context 1 failed to dispatch DRAW_VBO`. Context 1 is the session cosmic-comp. The clock froze and new scanout buffers came up 95–100 % magenta.
- Boot 3 also showed `context 9 … Unknown 1286` (GL_INVALID_FRAMEBUFFER_OPERATION on CLEAR) and `Illegal command buffer` in the workspaces client.
- An "Illegal handle" means a CREATE_OBJECT the guest sent never reached the host. That is a dropped EXECBUFFER.
- The finalverify desktop virgl flake ("Illegal surface 205 … ctx 2" after Esc on the overview, dock gone) is the same class, seen on a Linux host.

**Bisect.** Run with the same harness (`steps2.py`: terminal, app library, Super+W, settings ×4–8) and the old July cosmic-comp in every image, so only the kernel changes:

| image | host context errors | magenta |
|---|---|---|
| integ 2f4214bd | at Super+W, 3/3 boots | yes |
| fc3e998 (integ + multiterm merge) | 0 | 0 |
| ff7f5cd (… + procpool + compfdleak) | 0 | 0 |
| e66225d main with the old cosmic-comp | 0 | 0 |
| e66225d main (shipped, weakwindow comp) | 0 in ~80 settings cycles plus 3 Super+W | 0 |

The fix is the multiterm merge: 060c20a and 8864245.
- It raised cosmic-comp's per-process VFS fd table from 128 to 256. Cosmic-comp holds about 100 fds at idle plus about 8 per client window.
- It raised the global eventfd pool from 256 to 1024 (it held 235/256 on an empty desktop).
- It also raised the tmpfs pool from 128 to 512 and epoll from 256 to 1024.

This ties to `kernel/src/syscall.rs` EXECBUFFER: with FENCE_FD_OUT, the kernel reserves the out-fence eventfd **before** it submits. If that fails (EMFILE/ENFILE), it returns without submitting, as upstream Linux does. Mesa then logs "expect bad rendering" and discards the stream, which loses the CREATE_OBJECTs, and the host context dies.
- **Inferred, not traced:** I did not catch the EMFILE live on integ, because integ printed nothing on a full pool.
- **Direct test on main came back negative:** with efdhog leaving 0 or about 20 free global eventfd slots, clients failed to start (ENFILE) but no host context error appeared. So the global pool alone does not reproduce it. The per-process cosmic-comp table is the likelier ceiling.

**Open (the hardening this points at).** The failure mode still exists, just further away: 63fd24e notes 20 terminals reached cosmic-comp's 256 fds. Options:
- On an out-fence fd failure, submit the stream anyway and return the error, so host state stays consistent.
- Or keep the upstream behaviour and add a one-line serial report.

Either needs a check of Mesa virgl's error path. Not done here.

## 2. Mac 6–8 minute stalls — ROOT-CAUSED: host idle sleep. FIXED in the launchers

`pmset -g log` on 2026-09-27, during finalverify:
- Idle Sleep at 11:57:26 for 320 s, then maintenance sleeps until FullWake at 12:15:34.
- Idle Sleep at 12:27:39 for 368 s, woken at 12:33:47.

The finalverify transcript gaps match: 14:57:23Z→15:02:46Z, 15:07:16Z→15:15:41Z and 15:23:42Z→15:33:47Z. Its `mag0-20s.ppm` is 12:26:59 and `mag1-4s.ppm` is 12:33:47, which brackets the second sleep to the second. The Mac was on battery with displaysleep 2 min, and the only sleep blockers were the tool harness's short `caffeinate -t 300` assertions.

**Reproduced today, unprompted.** My own 100-minute soak (QEMU started before the fix) had event-log gaps of 344, 334 and 430 s. Those equal the pmset Idle Sleeps at 16:58:14 and 17:10:48, plus their maintenance sleeps. One HMP probe "took" 181 s of wall time.
- With those gaps excluded, the soak had 841 probes: serial p99 0.000 s (max 0.058), HMP p50 0.613 s, the clock never stale, and 0 STALL events.
- The x86_64/KVM soak on the desktop had 963 probes, serial max 0.30 s, HMP max 0.607 s, and 0 real stalls. Its one "stall" was COSMIC's idle screen blank (~15 min, black frame), which ended on the first click.

**Fix (9fc4442, user-facing).** On macOS, `scripts/run-qemu.sh` and `.claude/skills/run-leandros/driver.py` (`_hold_host_awake`) start `caffeinate -i -w <qemu pid>`:
- It takes PreventUserIdleSystemSleep, which is honoured on battery, for exactly QEMU's lifetime.
- Display sleep is still allowed.
- `LEANDROS_ALLOW_HOST_SLEEP=1` opts out.

Verified:
- driver: `caffeinate -i -w 5522` and later `-w 72164` attached to the QEMU and disappeared when it stopped.
- run-qemu.sh: `caffeinate -i -w 71725`, where 71725 is the exec'd qemu-system-aarch64. After `kill`, the assertion was released.

## Regressions (main + 9fc4442)
- **aarch64/HVF virgl (Mac):** drmsmoke `failed=0`; polltest 7/7 PASS, including poll_timeout_wake_latency. COSMIC session: greeter → login → panel, dock and wallpaper; settings opened and closed about 80×; Super+T, Super+A and Super+W all fine. QEMU stderr was empty.
- **x86_64/KVM virgl (desktop):** drmsmoke `failed=0`; polltest 7/7 PASS. COSMIC session up; settings ×10 and a 100-minute soak. QEMU stderr had 0 vrend lines.
- The guest image did not change: the commit is host-side launchers only.

## Notes
- `driver.py` puts the VNC clock crop and clicks at 1280×800. x86 runs at 1920×1080, so the `magloop2` click coordinates were wrong there; the `steps2` and soak checks are resolution-independent.
- The desktop has a leftover pid 1135309 and a term20 dir; per the preamble I left them alone.
