# lane credxlate 2026-11-09

Status: FIXED (SCM_CREDENTIALS), REFUTED (timer / SIGSYS translation).

## Findings
- SCM_CREDENTIALS and SO_PASSCRED did not exist at all in servers/net (only SCM_RIGHTS). Implemented, not just translated:
  - setsockopt/getsockopt SO_PASSCRED (AF_UNIX): per-description flag (SockOpts.passcred) plus per-end flags (pc_a/pc_b) on UnixConn.
  - sendmsg (stream and connected dgram/seqpacket): explicit SCM_CREDENTIALS checked as Linux scm_check_creds (own pid/ruid,euid,suid/rgid,egid,sgid, else CAP_SYS_ADMIN/CAP_SETUID/CAP_SETGID in the sender's userns; EPERM, EINVAL for unmapped ids, ESRCH for an invisible pid with CAP_SYS_ADMIN). With no explicit cmsg, the sender's creds are attached when the peer end has SO_PASSCRED.
  - Credentials ride in PendingFdBatch.cred (host pid/uid/gid) and are translated at recvmsg into the receiver's pidns (invisible pid reads 0) and userns (unmapped id reads 65534). Only delivered if the receiving description has SO_PASSCRED.
- Id translation structure (coordinator request): sender-supplied ids go through the new `sched::ns::kid_from_view` (view -> kuid) before the permission check; receive uses `view_id`. Maps are identity-only so no test can distinguish; a non-identity map only needs to change these two helpers.
- SO_PEERCRED was already translated (pid via pidns, ids via view_id). Unchanged.
- Timer signals: `_timer` carries si_tid/si_overrun/si_value, no pid/uid. SIGSYS/seccomp `_sigsys` carries call_addr/syscall/arch, no pid/uid. Nothing to translate; guards added (sigsys_in_pidns, timer_sig_in_pidns) and pass on main too, as expected.

## Tests (userland/nstest)
scm_cred_into_pidns (pid 0, uid 1000), scm_cred_from_pidns (real pid), scm_cred_self_in_pidns (explicit pid 1), scm_cred_unmapped (root sender -> 65534), scm_cred_forge (EPERM), sigsys_in_pidns, timer_sig_in_pidns.
Unfixed main: nstest 77 pass / 5 fail (the five scm cases). Fixed: 82/0 on aarch64 (Mac).

## Open
- Plain send()/write() on a PASSCRED peer attaches no credentials (only sendmsg does).
- Accepted sockets do not inherit SO_PASSCRED from the listener; set it on the accepted fd.
- If SO_PASSCRED is enabled after the data was queued, that data has no credentials (Linux would report the sender's).
- No SCM_CREDENTIALS for AF_UNIX dgram without a connect (sendto).
