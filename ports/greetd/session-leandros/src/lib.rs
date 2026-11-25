//! `session-leandros` -- the LeandrOS session bookkeeping greetd does around a
//! real PAM session: utmpx/wtmp/lastlog records and the per-session cgroup.
//!
//! Authentication, account checks and the environment all come from real
//! Linux-PAM now (/etc/pam.d/greetd). What PAM has no module for on LeandrOS is
//! this, so the greetd patch (0005) calls it right after `pam_open_session`
//! and after `pam_close_session`.

// A copy of lib/utmpx/utmpx.rs. Keep the two identical.
mod utmpx;
use std::ffi::CStr;
use std::fs;

/// The greeter's own account is not a login.
pub fn session_open(user: &str) {
    let Ok(c) = std::ffi::CString::new(user) else { return };
    unsafe {
        join_session_cgroup(&c);
        if user != "cosmic-greeter" {
            let uid = libc::getpwnam(c.as_ptr()).as_ref().map(|p| p.pw_uid).unwrap_or(0);
            utmpx::login(libc::getpid(), b"greetd", user.as_bytes(), b"", uid);
        }
    }
}

pub fn session_close() {
    utmpx::logout_pid(unsafe { libc::getpid() });
}

/// Put the calling process (greetd's session worker, which forks the user's
/// session) into `/sys/fs/cgroup/user.slice/user-<uid>.slice/session-<pid>.scope`,
/// owned by the user, as /bin/login does. Best effort.
unsafe fn join_session_cgroup(user: &CStr) {
    use std::os::unix::fs::{chown, DirBuilderExt};
    let pw = libc::getpwnam(user.as_ptr());
    if pw.is_null() || (*pw).pw_uid == 0 {
        return;
    }
    let (uid, gid) = ((*pw).pw_uid, (*pw).pw_gid);
    let slice = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice");
    if fs::DirBuilder::new().mode(0o755).create(&slice).is_ok() {
        let _ = chown(&slice, Some(uid), Some(gid));
        let _ = fs::write(format!("{slice}/cgroup.subtree_control"), "+cpu +io +memory +pids");
    }
    let scope = format!("{slice}/session-{}.scope", libc::getpid());
    if fs::DirBuilder::new().mode(0o755).create(&scope).is_ok() {
        let _ = chown(&scope, Some(uid), Some(gid));
        let _ = fs::write(format!("{scope}/cgroup.procs"), "0");
    }
}
