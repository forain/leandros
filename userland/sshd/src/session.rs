//! One SSH connection: authentication, session channels and their children.
//!
//! Every shell, exec and subsystem runs in a child that, between fork and
//! exec, becomes a session leader (setsid; with a pty also TIOCSCTTY on it),
//! sets the user's supplementary groups, setresgid, setresuid, then reads
//! the ids back and refuses to exec if the drop did not take (the same rule
//! as greeter-launch and claude-run). Interactive shells get a pty from
//! /dev/ptmx; exec and subsystems get pipes, stderr on extended data 1.

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use russh::Sig;
use tokio::io::unix::AsyncFd;


pub(crate) enum Input {
    Data(Vec<u8>),
    Eof,
}

#[derive(Clone)]
pub(crate) struct PtyReq {
    pub(crate) term: String,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) px: u16,
    pub(crate) py: u16,
}

pub(crate) fn sig_number(s: &Sig) -> Option<i32> {
    Some(match s {
        Sig::ABRT => libc::SIGABRT,
        Sig::ALRM => libc::SIGALRM,
        Sig::FPE => libc::SIGFPE,
        Sig::HUP => libc::SIGHUP,
        Sig::ILL => libc::SIGILL,
        Sig::INT => libc::SIGINT,
        Sig::KILL => libc::SIGKILL,
        Sig::PIPE => libc::SIGPIPE,
        Sig::QUIT => libc::SIGQUIT,
        Sig::SEGV => libc::SIGSEGV,
        Sig::TERM => libc::SIGTERM,
        Sig::USR1 => libc::SIGUSR1,
        Sig::Custom(n) => match n.as_str() {
            "USR2" => libc::SIGUSR2,
            "WINCH" => libc::SIGWINCH,
            "CONT" => libc::SIGCONT,
            "STOP" => libc::SIGSTOP,
            "TSTP" => libc::SIGTSTP,
            _ => return None,
        },
    })
}

pub(crate) fn sig_from_number(n: i32) -> Sig {
    match n {
        libc::SIGABRT => Sig::ABRT,
        libc::SIGALRM => Sig::ALRM,
        libc::SIGFPE => Sig::FPE,
        libc::SIGHUP => Sig::HUP,
        libc::SIGILL => Sig::ILL,
        libc::SIGINT => Sig::INT,
        libc::SIGKILL => Sig::KILL,
        libc::SIGPIPE => Sig::PIPE,
        libc::SIGQUIT => Sig::QUIT,
        libc::SIGSEGV => Sig::SEGV,
        libc::SIGTERM => Sig::TERM,
        libc::SIGUSR1 => Sig::USR1,
        libc::SIGUSR2 => Sig::Custom("USR2".into()),
        _ => Sig::Custom(format!("SIG{n}")),
    }
}

/// Variables a client may set with an `env` request (OpenSSH's AcceptEnv in
/// Debian's default config: the locale).
pub(crate) fn env_allowed(name: &str) -> bool {
    name == "LANG" || name.starts_with("LC_") || name == "COLORTERM" || name == "TZ"
}
/// An fd shared with the window-change path: AsyncFd needs an owner, and the
/// master must stay open until both halves are done.
pub(crate) struct DupFd(pub(crate) Arc<OwnedFd>);
impl AsRawFd for DupFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

pub(crate) async fn write_all_fd(afd: &AsyncFd<DupFd>, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let mut guard = afd.writable().await?;
        match guard.try_io(|inner| {
            let n = unsafe { libc::write(inner.as_raw_fd(), data.as_ptr().cast(), data.len()) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Ok(Ok(n)) => data = &data[n..],
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => {}
        }
    }
    Ok(())
}

pub(crate) fn set_winsize(fd: RawFd, p: &PtyReq) {
    let ws = libc::winsize { ws_row: p.rows, ws_col: p.cols, ws_xpixel: p.px, ws_ypixel: p.py };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) };
}

/// A pty pair from /dev/ptmx: (master, nonblocking + close-on-exec; slave; slave path).
pub(crate) fn open_pty(p: &PtyReq) -> std::io::Result<(OwnedFd, OwnedFd, String)> {
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if m < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master = OwnedFd::from_raw_fd(m);
        if libc::grantpt(m) != 0 || libc::unlockpt(m) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut name = [0 as libc::c_char; 64];
        if libc::ptsname_r(m, name.as_mut_ptr(), name.len()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let path = std::ffi::CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned();
        let s = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if s < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave = OwnedFd::from_raw_fd(s);
        set_winsize(s, p);
        let fl = libc::fcntl(m, libc::F_GETFL);
        libc::fcntl(m, libc::F_SETFL, fl | libc::O_NONBLOCK);
        Ok((master, slave, path))
    }
}

/// May a dev password (equal to the user name) log in from `peer`? See
/// `DevPasswordAuthentication` in sshd_config.
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub(crate) fn dev_password_allowed(policy: crate::DevPasswords, peer: SocketAddr) -> bool {
    use crate::DevPasswords::*;
    match policy {
        Yes => true,
        No => false,
        Local => {
            let ip = match peer.ip() {
                std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                    Some(v4) => std::net::IpAddr::V4(v4),
                    None => return v6.is_loopback(),
                },
                ip => ip,
            };
            match ip {
                // Loopback; QEMU user networking (10.0.2.0/24, where a hostfwd
                // connection appears from 10.0.2.2); and the macOS host under
                // socket_vmnet's shared network, which is exactly 192.168.105.1
                // (the bridge address the test harness connects from; see
                // driver.py _socket_vmnet_prefix). Only the host itself, not
                // the rest of that /24, since other VMs share it.
                std::net::IpAddr::V4(v4) => {
                    v4.is_loopback()
                        || v4.octets()[..3] == [10, 0, 2]
                        || v4.octets() == [192, 168, 105, 1]
                }
                _ => false,
            }
        }
    }
}
