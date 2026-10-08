# lane relibcpoll — 2026-10-02 (Mac only; aarch64 HVF, x86_64 TCG)

Outcome: FIXED (relibc poll). Follow-up from lane epollerr "Open".

## relibc (local only, never pushed)
- Worktree `~/code/relibc-pollfix`, branch `pollfix`, commit `fd1967e1` on top of `1746b13a`. `~/code/relibc` itself untouched.
- `src/header/poll/mod.rs` `poll_epoll`:
  - Each distinct fd is registered once, with the union of its entries' events. This avoids EEXIST.
  - Results are fanned out to the duplicates, each masked by its own events (+ERR/HUP/NVAL).
  - EPERM from ADD (regular file, dir, ...) means always ready: `revents = requested & (POLLIN|POLLRDNORM|POLLOUT|POLLWRNORM)`.
  - EBADF gives POLLNVAL per entry, duplicates included.
  - epoll_pwait uses timeout 0 when any entry is already reportable. Previously only the all-closed case returned early.
  - Negative fds are ignored (revents 0).
- select()/pselect: no change needed. It iterates fd numbers (no duplicates) and already treats EPERM as ready.
- Worktree needs the submodules: `dlmalloc-rs` and `openlibm` were copied from `~/code/relibc` (the .git files were removed).

## leandros branch `lane/relibcpoll` @ 07ba51a (pushed)
- `LEANDROS_RELIBC_DIR` (default `../relibc`, unchanged when unset) is honoured by:
  - `scripts/build-all.sh` (`build_relibc`)
  - all 48 `userland/*/build.rs` that link relibc (the link-search path)
  - `scripts/mkfs-f2fs-populated.py` (the staged `libc.a`)
- `userland/polltest`: new case `poll_linux_semantics` covers a regular file (POLLIN|POLLOUT and POLLOUT-only), a negative fd, duplicate fds with different events, a pipe, a dup'd regular file and a closed fd (POLLNVAL).
- relibc-linked binaries are the `RELIBC_LINKED` list in `scripts/build-userland.sh` (pthreadtest ... uptrtest, plus scmtest etc.). musl userland (brush, cosmic, ...) is unaffected.

## Evidence (build: `LEANDROS_RELIBC_DIR=~/code/relibc-pollfix ./scripts/build-all.sh`, rc 0)
| suite | aarch64 HVF | x86_64 TCG |
|---|---|---|
| polltest | 18 PASS / 0 FAIL (new case: `poll_sem` = 1 1 1 1 1 1 1) | 18 / 0 |
| epolltest | 11/0 | 11/0 |
| wakepolltest | 17/0 | 17/0 |
| scmtest | 47 PASS lines/0 FAIL, RC 0 | same |
| forktest | 8/0 | 8/0 |
| exectest | 29/0 | 29/0 |
| vfstest | 59/0 | 59/0 |

All RC=0.

COSMIC smoke (virgl, login as leandro, 3x Super+T with `ls`, launcher open/close): desktop with 3 terminals, screenshots OK on both arches. Serial log SEGV/[PF]/panic/WDOG/`[EPOLL] ctl err` = 0 on both. The smoke used `driver log`, which is the serial log only.

Not measured: a negative control (old relibc failing the new case). The failure mode is taken from the lane epollerr note (poll returns -1 on EPERM/EEXIST).

## Adopt
    git -C ~/code/relibc merge pollfix        # fast-forward; local only
    ./scripts/build-all.sh
(or merge lane/relibcpoll to get `LEANDROS_RELIBC_DIR` + the test first). `~/code/relibc-pollfix` is kept for the merge; remove with `git -C ~/code/relibc worktree remove --force ~/code/relibc-pollfix` after merging. The `--force` is needed because the submodule dirs were copied in.
