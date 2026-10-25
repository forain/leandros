# lane/pwlog 2026-10-25 (L12 #59) FIXED

- Current scripts/desktop-login.sh already feeds the password on stdin (`pwsrc | remote.py run`); the GC_PW=... argv leaks were from older ad-hoc login scripts. No script in scripts/, .claude/ or the lane scratchpad still uses GC_PW.
- scripts/remote.py redact(): also masks GC_PW / *PASS(WORD|WD) / *SECRET / *TOKEN assignments.
- userland/sshd session.rs: exec requests are logged as first word + byte count only (takes effect at next laptop sync; not deployed). Compile-check of sshd not run (ring needs the cross C toolchain); the change is a single format! call.
- Scrubbed: laptop /var/log/sshd.log (44 lines) and 33 Mac copies of sshd.log; recount 0. Scripted login re-verified (session up in 6 s).
- Note: until the new sshd is deployed, any `remote.py run` command is still logged in full (including this lane's own scan commands).
