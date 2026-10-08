# Lane pwrace — 2026-10-02: FIXED (root cause is a stale brush checkout on the Mac)

Branch `lane/pwrace` @ `e5e17d3` (base origin/main 207f928), pushed. Machines: Mac aarch64 HVF, desktop x86_64 KVM.

## Root cause
`start-cosmic-leandros` does `/bin/sh -c 'echo $$ > pidfile; exec pipewire' > pipewire.log 2>&1 &` and then `exec dbus-run-session …`.
brush runs a `&` list as a tokio task inside the shell process, not as a forked child. Upstream brush (and patches 0001+0002 alone) continue straight to the next command. When `exec` replaces the image before that task has spawned anything, the job is gone. The redirect has already been opened by then, so the signature is: empty pipewire.log, no pidfile, no pipewire process. That is exactly the failing runs (avm-2/3/4 on main: rundir has a 0-byte pipewire.log and no pipewire.pid).

`ports/brush/patches/0003` (lane brush, 2026-09-18, desktop commit 3423c0e) makes the parent wait at `&` until the task has spawned its first process. That is bash's fork-at-`&` ordering. The desktop's `leandros-siblings/brush` has 0003. **The Mac's `~/code/brush` never got it**: it is a8006e7 plus an uncommitted diff, and its content equals f02a328 = pin + 0001 + 0002. The 2026-09-18 note had already said "Mac still needs sync.sh apply". So every aarch64 image built on the Mac shipped the racy /bin/sh. The x86_64 images were built on the desktop and never showed it. It was never the arch and never the kernel. Load (aplay spooling) only widens the window.

Host proof (macOS, same brush sources, `X > log & exec true`, 200 iterations):
- Mac tree (f02a328): child lost 199/200 idle and 179/200 under `yes` load.
- 0003: 0/200 and 0/200.
- `brush -c 'cmd &'`, where exit loses cmd: 0/50 started before the fix, 50/50 after.
- bash: 0/50 lost.

## Change (leandros)
- `scripts/build-all.sh` `build_brush`: runs `ports/brush/sync.sh check` once per build. On a mismatch it stops and prints the apply command. `LEANDROS_BRUSH_UNCHECKED=1` overrides it for local brush work.
- `ports/brush/sync.sh`: accepts a git worktree (`.git` is a file there).
- `ports/brush/README.md`: documents the check.
- No kernel, launcher or sink change. Nothing in them was wrong.

## Evidence (image built with LEANDROS_BRUSH_DIR = pin+0001..0003)
audio-session-test.py with AV_PRECLAIM=5 and steps probe, tone, console, report (virgl, greeter login as leandro):

| | aarch64 HVF (Mac) | x86_64 KVM (desktop) |
|---|---|---|
| PipeWire + WirePlumber + sink running, pidfile present | **10/10** (main image: 1/4; polish1002: 3/7) | 3/3 |
| sink "busy … waiting", then streamed | 10/10 | 3/3 |
| tone1 | 10.0–10.1 s, RMS 5771.6–5788.4 (ideal 5792), no in-body holes | 10.0–10.1 s, RMS 5782–5788 |
| console aplay during session | EBUSY rc 1, 10/10 | 3/3 |
| PF/SEGV/panic/WDOG/SND stall | 0 in every run | 0 |

In the tone analysis, the "holes" (≤2) are only the onset/offset partial 50 ms windows; in-body rms_min is ≥5305. Runs 1–9 have 2–4 sine discontinuities. Run 10 had 9, all within the first 3.3 s, with rms_min 4375. This is the known first-playback client xrun from lane pipewire, not this bug.

Regressions:

| | aarch64 HVF | x86_64 KVM |
|---|---|---|
| polltest | 18/0 | 18/0 |
| privtest root | 131/0 | 133/0 |
| privtest leandro | 92/0 | 94/0 |
| shjobs.py | 36/36 | 36/36 |
| COSMIC session | panel and wallpaper up in all 13 runs | same |

## Before merging (ORDER MATTERS)
On the Mac, run `cd ~/code/brush && git stash && ~/code/leandros/ports/brush/sync.sh apply`. The stash is exactly 0001+0002, which the series already contains. Otherwise every Mac build-all stops at the new check, which is intended. Until then, `LEANDROS_BRUSH_UNCHECKED=1` keeps the old (racy) behaviour.

## Open
- The probe's `read c < /proc/N/comm` prints empty names in-guest on both old and new images (pre-existing). The test's `cmdlines` step works.
- Alternative brush fix not taken: `exec`/exit wait for unstarted background tasks. It is redundant with 0003 and saved as `lane-pwrace-brush-bgspawn-alt.patch`.
- Desktop logs: `leandros-siblings/pwr-keep/`. Mac runs: the session scratchpad (`runs/`, `regress-a64/`).
