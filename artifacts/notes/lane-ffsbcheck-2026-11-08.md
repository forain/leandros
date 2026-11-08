# lane/ffsbcheck 2026-11-08 - Firefox sandbox acceptance harness

DELIVERED. Script: `.claude/skills/run-leandros/ffsandbox.py <arch> <tag> [--mode on|off|both] [--secs 60]`
(wraps ffsession.py; page with a looping webm so RDD starts; guest collector via --post).
Run: `lock.sh qemu python3 .claude/skills/run-leandros/ffsandbox.py aarch64 sb1`. Exit 0 only if mode "on" has
Seccomp: 2 in every child type (forkserver exempt) and no sandbox error lines in /tmp/ff.log.
Knob: `LEANDROS_FIREFOX_SANDBOX=1` in ports/firefox/firefox.sh (no MOZ_DISABLE_*_SANDBOX, MOZ_SANDBOX_LOGGING=1).
Also firefox.desktop Exec=/bin/firefox %u.

## BASELINE (aarch64, main 5afeaf80, sandbox knob on)
- Firefox SURVIVES: 9 processes (parent, socket, forkserver, 4x tab, rdd, utility), video plays, no crash/EXIT.
- No "unsupported"/error lines; MOZ_SANDBOX_LOGGING only prints policy-building chatter (380 lines, `Sandbox: policy for ...`),
  all from the parent (pid 771). Off mode: only "Couldn't open video device /dev/video0".
- /proc/<pid>/status has NO Seccomp:, Seccomp_filters:, NoNewPrivs:, NSpid: lines (no "Seccomp" string in the
  tree); /proc/<pid>/root reads "/" for every process. The sandbox is not verifiably active in any child: harness FAILS
  (all "?") until the kernel lanes expose these fields.
- The noise filter in the script was refined after the recorded run (that run had no real error lines).
