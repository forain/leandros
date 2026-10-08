# lane brushdup 2026-10-01 (Mac)

Fix: brush `TryFrom<OpenFile> for Stdio` now dups Stdin/Stdout/Stderr (try_clone_to_owned) instead of Stdio::inherit().
- brush: branch `brushdup` @ 9ec1a86 in worktree ~/code/brush-brushdup (base a8006e7 = brush main HEAD). Local only: ~/code/brush remote is upstream reubeno/brush, not pushed.
- leandros: branch `lane/brushdup` (pushed): scripts/build-all.sh and scripts/mkfs-f2fs-populated.py honour LEANDROS_BRUSH_DIR (default ../brush; unchanged behaviour when unset).
- Test build used brushdup + the user's UNCOMMITTED ~/code/brush diff (crossterm patch, jobs.rs, signal.rs, kill.rs) applied as a temporary overlay (removed afterwards), since the worktree base lacks them.
Evidence (aarch64 HVF, x86_64 TCG on Mac): `sh -c 'echo hi >&2' 2>/tmp/f; cat /tmp/f` -> hi; `2>&1 | cat` -> A,B; `2>&1 >/dev/null | cat` -> B only; `echo Y >&2 | cat` with 2>file -> Y in file; exectest 28/28 PASS + EXECTEST: PASS on both; COSMIC session (virgl, login leandro, Super+T cosmic-term, ls -la /bin) rendered on both, screenshots in session scratchpad (cosmic-a64.png, cosmic-x64.png). x86_64 smoke is TCG, not KVM.
Adopt: cd ~/code/brush && git merge brushdup (your dirty files are untouched by it); then LEANDROS build as usual.
