# lane/ffdefault 2026-11-08 - PARTIAL (harness merged, default flip NOT merged)

Default flip (firefox.sh no MOZ_DISABLE_*, LEANDROS_FIREFOX_SANDBOX=0 opt-out, SANDBOX_LOG=1 logging; also removed
`security.sandbox.content.level=0` from leandros-prefs.js, which silently disabled the content sandbox) was built and run
on main 30174ec2+nspidproc+nsudp; held back because acceptance on=False on both arches. Commit 2nd-parent of this lane
has the flip; reapply with `git show lane/ffdefault~1 -- ports docs` pieces if wanted.

Per-process (all: Seccomp 2, filters 1, NoNewPrivs 1, NSpid single value, root /.leandros-void):
| type | aarch64 | x86_64 (TCG) | differs from parent |
| socket | 1 | 1 | user,ipc (net shared) |
| rdd | 1 | 1 | user,net (ipc shared) |
| utility | 1 | 1 | user,net,ipc |
| tab | 4 | 4 | user,net,ipc |
Video progressing (SBV lines: x86 74). forkserver and parent unsandboxed as expected. No cpu/present broker denial (0).

Counted lines (only blocker): tab `Sandbox: Failed errno -13 op open path /usr/local/lib/libsqlite3.so.0`
(1-2 tabs per run, both arches; other tabs get -2 for the same path). It is an ld.so probe of a nonexistent file
(libsqlite3 lives in /usr/lib); broker returns EACCES since /usr/local/lib is not in that tab's policy. Fix options: allow
/usr/local/lib read in the content policy, or ignore ENOENT-equivalent probes. Relative-path denials (IO_USE_VFS=local/...)
are ignored by the harness as designed. Harness also fixed to ignore the "[fslock]" stats line spliced into log output.
