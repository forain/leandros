//! Per-session cgroup, as /bin/login does for console logins (CG6):
//! `/sys/fs/cgroup/user.slice/user-<uid>.slice/session-sshd<N>.scope`, owned
//! by the user. sshd creates the scope before the fork (allocation is fine
//! there) and the forked child only writes "0" to its `cgroup.procs`
//! (syscalls only). Every step is best effort: a failure leaves the session
//! in sshd's own cgroup, as before. The empty scope is removed when the
//! session ends.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{chown, DirBuilderExt};
use std::sync::atomic::{AtomicU32, Ordering};

const ROOT: &str = "/sys/fs/cgroup/user.slice";
static NEXT: AtomicU32 = AtomicU32::new(1);

pub struct Scope {
    dir: String,
    procs: CString,
}

impl Scope {
    /// The `cgroup.procs` path, for the forked child to write "0" into.
    pub fn procs(&self) -> CString {
        self.procs.clone()
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}

pub fn prepare(uid: u32, gid: u32) -> Option<Scope> {
    if uid == 0 {
        return None;
    }
    let slice = format!("{ROOT}/user-{uid}.slice");
    if std::fs::DirBuilder::new().mode(0o755).create(&slice).is_ok() {
        let _ = chown(&slice, Some(uid), Some(gid));
        let _ = std::fs::write(format!("{slice}/cgroup.subtree_control"), "+cpu +io +memory +pids");
    }
    for _ in 0..64 {
        let dir = format!("{slice}/session-sshd{}.scope", NEXT.fetch_add(1, Ordering::Relaxed));
        if std::fs::DirBuilder::new().mode(0o755).create(&dir).is_ok() {
            let _ = chown(&dir, Some(uid), Some(gid));
            let procs = CString::new(std::ffi::OsStr::new(&format!("{dir}/cgroup.procs")).as_bytes()).ok()?;
            return Some(Scope { dir, procs });
        }
    }
    None
}

/// Child side, after fork and before the uid drop. Syscalls only.
pub unsafe fn join(procs: &CString) {
    let fd = libc::open(procs.as_ptr(), libc::O_WRONLY);
    if fd >= 0 {
        libc::write(fd, b"0".as_ptr().cast(), 1);
        libc::close(fd);
    }
}
