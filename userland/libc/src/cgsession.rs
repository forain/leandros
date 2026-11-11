//! Session cgroup membership, shared by /bin/login and greeter-launch.

use crate::io::{chown, close, mkdir, open, write, O_WRONLY};
use crate::process::getpid;

/// CG6: put the calling process (a login, the greeter launcher) into its own delegated cgroup,
/// `/user.slice/user-<uid>.slice/session-<pid>.scope`. The user slice is
/// owned by the user (so it can mkdir siblings and move its own processes),
/// controllers are enabled for it, and the session leaf is created owned by
/// the user too. Needs root; every step is best effort (a failure leaves the
/// session in the root cgroup, as before).
pub unsafe fn join(uid: u32, gid: u32) {
    if uid == 0 { return; }
    let mut slice = [0u8; 96];
    let mut n = 0usize;
    let push = |b: &mut [u8; 96], n: &mut usize, s: &[u8]| { b[*n..*n + s.len()].copy_from_slice(s); *n += s.len(); };
    let push_u = |b: &mut [u8; 96], n: &mut usize, mut v: u32| {
        let mut t = [0u8; 10]; let mut i = 10;
        if v == 0 { i -= 1; t[i] = b'0'; }
        while v > 0 { i -= 1; t[i] = b'0' + (v % 10) as u8; v /= 10; }
        b[*n..*n + 10 - i].copy_from_slice(&t[i..]); *n += 10 - i;
    };
    push(&mut slice, &mut n, b"/sys/fs/cgroup/user.slice/user-");
    push_u(&mut slice, &mut n, uid);
    push(&mut slice, &mut n, b".slice");
    let slice_len = n;
    let first = mkdir(slice.as_ptr(), 0o755) == 0;
    if first {
        chown(slice.as_ptr(), uid, gid);
        let mut f = slice; let mut m = slice_len;
        push(&mut f, &mut m, b"/cgroup.subtree_control");
        let fd = open(f.as_ptr(), O_WRONLY, 0);
        if fd >= 0 { let c = b"+cpu +io +memory +pids"; write(fd, c.as_ptr(), c.len()); close(fd); }
    }
    push(&mut slice, &mut n, b"/session-");
    push_u(&mut slice, &mut n, getpid() as u32);
    push(&mut slice, &mut n, b".scope");
    if mkdir(slice.as_ptr(), 0o755) != 0 { return; }
    chown(slice.as_ptr(), uid, gid);
    let mut f = slice; let mut m = n;
    push(&mut f, &mut m, b"/cgroup.procs");
    let fd = open(f.as_ptr(), O_WRONLY, 0);
    if fd >= 0 { write(fd, b"0".as_ptr(), 1); close(fd); }
}
