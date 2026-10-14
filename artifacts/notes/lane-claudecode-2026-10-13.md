# lane/claudecode: Claude Code CLI inside LeandrOS (2026-10-13/14)

Branch `lane/claudecode` off main 38c06d3. Not merged, not pushed. STATUS: DONE, all three
success criteria met on both arches, verified on freshly regenerated images.

## What runs
| | aarch64 (HVF) | x86_64 (TCG) |
|---|---|---|
| `claude --version` | `2.1.296 (Claude Code)` | `2.1.296 (Claude Code)` |
| TUI in a pty (tmux capture-pane) | theme picker, Down, Enter -> login method | same |
| TUI on the text VT (tty2, QMP keys) | renders incl. block art/❯/✔, keys work | (not shot) |
| HTTPS | 401 authentication_error, invalid x-api-key | same |

Proof: `scripts/claudecode-smoke.sh <arch>` (0 failures both arches, fresh images);
transcripts in `artifacts/claudecode/*-tmux-pty-screens.txt`, `*-network.txt`;
VT screenshots `artifacts/claudecode/aarch64-vt2-*.png` (gitignored, local only).
The OAuth flow was also driven: Console login -> URL + paste prompt -> bogus code ->
"OAuth error: Request failed with status code 400" (the server's answer).

## How
- npm `@anthropic-ai/claude-code` 2.1.296 ships no JS; linux-{arm64,x64}-musl = one Bun 1.4.3
  ELF (~250 MB) needing only musl. No node/npm. Runs in an Alpine root.
- `/usr/sbin/claude-code-install` (ports/apk/claude-code-install.sh): alpine-sysroot /opt/alpine
  (bash curl ca-certificates ripgrep git), npm tarball + sha512 pin, relative link. ~20 s / ~50 s.
- `/usr/bin/claude` (ports/apk/claude-code-run.sh): mounts proc, bind /dev, bind /tmp; chroot.

## Kernel / system bugs fixed (commits)
1. 5d18502 preadv/pwritev/preadv2/pwritev2 ENOSYS -> Linux semantics (Bun stdin: TUI deaf to keys).
2. f59b16d setfsuid/setfsgid ENOSYS (tmux) -> answer euid/egid.
3. 62842e7 /dev/tty after pty hangup opened the serial console -> ENXIO (orphaned claude stole
   serial keystrokes; brush died on DSR timeout). ptytest 16/16.
4. c644ffd VT console drew '?' for all non-ASCII -> generated box/block/shade glyphs + UI symbols.
5. 96854fe AF_UNIX sun_path used verbatim (no cwd/chroot/alias) -> resolved like open();
   unix getsockname returned unnamed -> bound name. chroottest chroot_unix_socket.
6. 10f55ba ports/apk install + launcher, smoke script, docs/userland/ports.md section.

Regress (runtests.py --suite regress + ptytest): aarch64 21/21, x86_64 21/21.

## Open issues
- Launcher needs root (chroot/mount; image has no su/sudo), so cosmic-term as `leandro`
  cannot start it. cosmic-term not tried.
- LeandrOS drops a pending AF_UNIX connection whose connector closed before accept (Linux
  still hands it out); chroottest keeps connectors open to avoid it.
- setfsuid does not keep a separate fsuid for privileged callers.
- uname release is "1.0.0"; programs gating features on kernel version may misjudge.
- VT console: TAB (0x09) drawn as a glyph instead of advancing (pre-existing); wide chars
  (emoji/CJK) take one cell.
- brush on the serial console sometimes dies with "cursor position could not be read" when a
  DSR reply is lost (seen once on x86 before fix 3; may be independent).
- Only the 2.1.296 release is pinned; newer releases need CLAUDE_CODE_VERSION + _SHA512.

## Follow-up 2026-10-14: non-root users ("Mount failed" as leandro) -- DONE
Commits: 83563e7 exec set-ID + no_new_privs, 76957d8 statx/tmpfs fstat owner, de6a855 claude-run.

- **Launcher.** It is a set-user-ID root Rust program, `userland/claude-run`, installed as
  `/bin/claude` (mode 04755). `/bin/alpine-run` is a hard link to it that runs any command
  of the root, for example tmux.
  - It mounts proc, `/dev`, `/tmp` and `/home` into `/opt/alpine`, plus the caller's home
    when it lies outside `/home`.
  - It refuses unless the root and its `etc` are root-owned and writable only by root.
  - It appends the caller's passwd and group lines to the root.
  - It chroots, then sets the caller's supplementary groups, calls setresgid and setresuid,
    and checks that `setuid(0)` and `seteuid(0)` fail.
  - It changes to the caller's working directory if the root can see it, otherwise to the
    home, and passes the environment minus `LD_*`.
- **Kernel bug: set-ID bits ignored.** execve ignored S_ISUID and S_ISGID. Fixed:
  - The saved ids now follow the effective ids at exec.
  - auxv carries the real ids and AT_SECURE.
  - PR_SET_NO_NEW_PRIVS and PR_GET_NO_NEW_PRIVS are honoured.
  - f2fs chown drops the set-ID bits.
- **Kernel bug: wrong owner in stat results.**
  - statx left uid, gid, the times and the device numbers zero.
  - tmpfs fstat reported uid 0.
  - Effect: as leandro, Bun refused `/tmp/claude-1000` as "owned by uid 0".
- **Tests.** Six new privtest root cases and the syscalltest case `statx_matches_stat`.
  All of them pass on Linux 7.2 first.
- **Verified as leandro, both arches.** The smoke run passed with 0 failures on aarch64 and
  on x86_64. It covers the root pass plus the leandro pass:
  - version and identity inside the root: `inside: 1000 leandro /home/leandro /home/leandro`;
  - the TUI in a tmux pty, with Down and Enter working;
  - the 401 check;
  - `~/.claude.json` written to the real `/home/leandro`.
- **Verified as leandro on screen (aarch64).**
  - Text VT2 login as leandro: `artifacts/claudecode/aarch64-vt2-leandro.png`.
  - cosmic-term on the COSMIC desktop (virgl), greeter login as leandro:
    `artifacts/claudecode/aarch64-cosmic-term-leandro-{theme,login}.png`. These PNGs are
    gitignored and kept only locally.
- **Regression results.** `--suite regress` plus ptytest and privtest: aarch64 22/22,
  x86_64 22/22.
- **Open issues.**
  - Files are not stripped of their set-ID bits when someone else writes to them (Linux
    `file_remove_privs`); only chown drops them.
  - Mounts have no nosuid option.
  - The launcher only appends passwd and group lines; it never updates them.
  - fstat of a pty does not report the owner either.
