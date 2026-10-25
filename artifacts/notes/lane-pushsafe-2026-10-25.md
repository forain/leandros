# lane/pushsafe - openbugs #87: remote.py push over a lossy link (FIXED)

`push` / `deploy-files` / `deploy-kernel` (all via `upload()`) now:
- write only `<dest>.part.<sha12>` (never the target); old file stays until the final `mv`;
- send 4 MiB chunks over `ssh ... cat >> part` (remote checks the offset first, returns the chunk sha256);
- stall watchdog (no byte moved for `LEANDROS_PUSH_STALL`=30 s kills ssh), bounded consecutive retries
  (`LEANDROS_PUSH_RETRIES`=10) with exponential backoff (max 30 s), control commands have timeouts+retries;
- resume: after a failure wait for the remote size to settle, sha256-compare the unverified tail with the
  local file (full prefix hash when starting from an existing part), continue from there; a bad tail is truncated;
- progress lines (MB, %, MB/s, retries) on stderr and a final `<dest>: sha256 <hex> OK` line;
- full-file sha256 on the remote; mismatch => part deleted, target untouched, 2 restarts then die;
  success also removes stale `<dest>.part.*`. Giving up keeps the part so a rerun resumes.

Test: `scripts/lossyproxy.py` (random kill/black-hole per connection) + `scripts/pushsafe-test.sh <run-id> new|old|naive N`,
aarch64 QEMU VM, 200 MB file, proxy mean 8 MB/connection, 40% of drops are silent stalls.
- new (stall 6 s): 3/3 target=new-verified, 158 s / 225 s / 379 s, 23+ retries per run, no .part left.
- retries=1 (forced give-up): 3/3 target=old-intact, part kept for resume.
- naive in-place `scp` x3 retry loop (the incident): 3/3 CORRUPT/PARTIAL target.
- old remote.py (sftp, fixed .remdbg-tmp, no timeout/resume): fails on the first reset (rc 1, target intact but no progress).
(`old` mode of pushsafe-test.sh needs a copy of the old script at scripts/remote_old.py.)
Not tested on the real laptop. Each drop re-sends up to 4 MiB from the verified offset.
