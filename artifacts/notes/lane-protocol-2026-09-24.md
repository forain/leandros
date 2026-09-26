# Lane protocol — wave 2026-09-24

Base: `main` @ `fc5a6bc` (origin/main). Open list: TODO.md "Open work (2026-09-18 reconciliation)".
Prior wave write-up: `artifacts/notes/wave-2026-09-18.md` + `lane-<name>-2026-09-18.md`.

## Machines
- **Mac** (this host): aarch64 **HVF** (fast), x86_64 TCG (slow). Worktree: `git worktree add ~/code/leandros-<lane> -b lane/<lane> origin/main`.
- **linux desktop** `ssh forain@172.16.158.150` (Tailscale fallback `100.106.210.103`): x86_64 **KVM**, host GPU (Venus/Zink). Repo `/home/forain/Projects/leandros`; create worktrees under `/run/media/forain/samsung970pro512/leandros-siblings/leandros-<lane>` (`/` is nearly full). Run `git fetch origin` there first.
- **linux laptop** `ssh forain@172.16.149.179`: x86_64 **KVM** (i5-8350U, 15 GB — one QEMU at a time), aarch64 TCG slow. Repo `~/Projects/leandros`, worktrees `~/Projects/leandros-<lane>`. sudo needs a password. zsh non-interactive PATH from `~/.zshenv`. Owned by lane `sessmisc` this wave.
- Pick machine by accelerator, not arch. `pkill` over ssh: use `qemu-system-aarch6[4]` / `qemu-system-x86_6[4]` patterns, and only kill QEMUs YOU started (by pid) — other lanes share the machine.

## Rules
- Follow `CLAUDE.md`: release builds only; Limine revision ≥ 6; never mention Claude/AI in commits or authorship. No session/attribution trailers in commits.
- Work only in your own worktree/branch `lane/<lane>`. Do not touch `main`, other lanes' branches, or the main checkout. Push your branch to origin when done (`git push -u origin lane/<lane>`).
- **Shared `../doomgeneric` build collision**: `build-all.sh` runs `make clean` in the shared sibling. Until the orchestrator says lane/buildobj is merged, wrap every `build-all.sh` in a lock: `until mkdir ~/.leandros-build.lock 2>/dev/null; do sleep 20; done; ./scripts/build-all.sh ...; rmdir ~/.leandros-build.lock` (per machine; always release it, even on failure — use a trap). Prefer narrower builds (kernel/server only) when the script allows.
- Long builds/QEMU runs: run in background and poll; never block a foreground call > 10 min.
- Test both arches before claiming done (CLAUDE.md). If one arch is only feasible on TCG, say so explicitly.
- Measure, don't argue. If the premise of your lane is wrong, say REFUTED with the numbers — that is a valid outcome.
- Named overlaps: note in your report if you touched a file another lane owns (see list below).
- Write a lane note to the Mac path `~/code/leandros/artifacts/notes/lane-<lane>-2026-09-24.md` (artifacts are hand-synced, not committed).

## Lanes and file ownership
| lane | machine | scope | primary files |
|---|---|---|---|
| buildobj | Mac | per-worktree/arch doomgeneric OBJDIR, drop shared `make clean`, atomic mv (plan in lane-pollout-2026-09-18 note) | scripts/build-all.sh, ../doomgeneric/Makefile.leandros |
| polltimer | Mac | one-shot timer armed to NEXT_POLL_DEADLINE; fix known-red `polltest poll_timeout_wake_latency`; make POSIX timers/setitimer non-tick-bound if cheap | kernel timer/poll/sched code |
| execleak | Mac | ~121 KiB/exec (order-5 block) in killmt exec_worker; ~100–160 pages per plain process death | kernel exec/process teardown, mm |
| forkcow | Mac | fork copies ~210 MiB eagerly for cosmic-comp → lazy COW | mm/src/cow.rs, fork path |
| runqlock | Mac | `lock_leader_address_space` top RUN_QUEUE site (~30k/s); `pick_next` O(256) | scheduler |
| vfsmisc | Mac | access(2) ruid not euid; atime on read; greeter-launch `initgroups`; vfstest header comment; greetd EBADF self-pipe | servers/vfs, greeter launch |
| greeterleak | desktop | ~55 MiB per greeter-chain death on x86_64/KVM (kernel heap / f2fs suspects) | kernel heap accounting, f2fs |
| sessmisc | laptop | Super+T in serial-started session; `[WDOG] cosmic-comp mmap ~2 s`; greeter keystroke lag re-measure; laptop worktree housekeeping | session/compositor launch, input |
| zinkverify | desktop | re-run `scripts/zinkbench.py` (gpuirq on KVM/Zink) and `audio-glitch-test.sh`; fix regressions found | virtio-gpu, audio |

Overlaps to watch: execleak ↔ greeterleak ↔ forkcow (mm/process teardown); polltimer ↔ runqlock (scheduler). Coordinate by keeping diffs minimal and noting touched shared files.

## Report (final message, ≤ 300 words)
Outcome (FIXED / PARTIAL / REFUTED), branch + head commit, what changed (files), test evidence per arch (numbers), shared files touched, anything left open.

## Addenda (orchestrator)
- **Mac SDK**: default SDKROOT resolves to a broken MacOSX27.0.sdk ("tapi error: malformed file"). Use `export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk` for Mac builds.
- **Never kill a process you did not start.** Record your own PIDs (`$!`) and kill only those. The build lock may be held by another lane for a long time — that is normal; wait.
