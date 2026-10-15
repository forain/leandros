//! One SSH connection: authentication, session channels and their children.
//!
//! Every shell, exec and subsystem runs in a child that, between fork and
//! exec, becomes a session leader (setsid; with a pty also TIOCSCTTY on it),
//! sets the user's supplementary groups, setresgid, setresuid, then reads
//! the ids back and refuses to exec if the drop did not take (the same rule
//! as greeter-launch and claude-run). Interactive shells get a pty from
//! /dev/ptmx; exec and subsystems get pipes, stderr on extended data 1.

use std::collections::HashMap;
use std::ffi::CString;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PublicKey;
use russh::server::{Auth, Handle, Msg, Session};
use russh::{Channel, ChannelId, Pty, Sig};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::accounts::{self, User};
use crate::{log, Config, RootLogin};

enum Input {
    Data(Vec<u8>),
    Eof,
}

#[derive(Clone)]
struct PtyReq {
    term: String,
    cols: u16,
    rows: u16,
    px: u16,
    py: u16,
}

#[derive(Default)]
struct Chan {
    pty: Option<PtyReq>,
    env: Vec<(String, String)>,
    input: Option<mpsc::UnboundedSender<Input>>,
    master: Option<Arc<OwnedFd>>,
    /// Pid of the child (a session and process-group leader), 0 before it starts.
    pid: Arc<AtomicI32>,
    started: bool,
}

pub struct Conn {
    cfg: Arc<Config>,
    peer: SocketAddr,
    local: Option<SocketAddr>,
    user: Option<User>,
    authed: Arc<AtomicBool>,
    chans: HashMap<ChannelId, Chan>,
}

pub async fn run_connection(
    rcfg: Arc<russh::server::Config>,
    cfg: Arc<Config>,
    stream: TcpStream,
    peer: SocketAddr,
    local: Option<SocketAddr>,
) {
    log(&format!("connection from {peer}"));
    let authed = Arc::new(AtomicBool::new(false));
    let grace = Duration::from_secs(cfg.login_grace_secs);
    let conn = Conn { cfg, peer, local, user: None, authed: authed.clone(), chans: HashMap::new() };
    let running = match russh::server::run_stream(rcfg, stream, conn).await {
        Ok(r) => r,
        Err(e) => {
            log(&format!("{peer}: handshake failed: {e:?}"));
            return;
        }
    };
    // LoginGraceTime: a connection that has not authenticated in time is dropped.
    if !grace.is_zero() {
        let handle = running.handle();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            if !authed.load(Ordering::Acquire) {
                log(&format!("{peer}: login grace time exceeded"));
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "login grace time exceeded".into(), "".into())
                    .await;
            }
        });
    }
    match running.await {
        Ok(()) => log(&format!("{peer}: connection closed")),
        Err(e) => log(&format!("{peer}: connection ended: {e:?}")),
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // The client is gone: hang up whatever its sessions still run, as a
        // pty hangup would.
        for ch in self.chans.values() {
            let pid = ch.pid.load(Ordering::Acquire);
            if pid > 0 {
                unsafe { libc::kill(-pid, libc::SIGHUP) };
            }
        }
    }
}

fn sig_number(s: &Sig) -> Option<i32> {
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

fn sig_from_number(n: i32) -> Sig {
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
fn env_allowed(name: &str) -> bool {
    name == "LANG" || name.starts_with("LC_") || name == "COLORTERM" || name == "TZ"
}

enum Kind {
    Shell,
    Exec(String),
    Subsystem(String),
}

impl Conn {
    fn may_login(&self, user: &User, by_password: bool) -> Result<(), &'static str> {
        if user.uid == 0 {
            match self.cfg.permit_root_login {
                RootLogin::No => return Err("root login refused (PermitRootLogin no)"),
                RootLogin::ProhibitPassword if by_password => {
                    return Err("root password login refused (PermitRootLogin prohibit-password)")
                }
                _ => {}
            }
        }
        if !accounts::shell_usable(user) {
            return Err("account has no usable shell");
        }
        if accounts::locked(user) {
            return Err("account is locked");
        }
        Ok(())
    }

    fn pubkey_ok(&self, user: &str, key: &PublicKey) -> Option<User> {
        if !self.cfg.pubkey_auth {
            return None;
        }
        let u = accounts::lookup_user(user)?;
        if let Err(why) = self.may_login(&u, false) {
            log(&format!("{}: publickey for {user}: {why}", self.peer));
            return None;
        }
        match accounts::key_authorized(&u, &self.cfg.authorized_keys_file, key) {
            Ok(true) => Some(u),
            Ok(false) => None,
            Err(why) => {
                log(&format!("{}: authorized_keys of {user} ignored: {why}", self.peer));
                None
            }
        }
    }

    fn base_env(&self, user: &User, pty: Option<&PtyReq>, tty: Option<&str>) -> Vec<(String, String)> {
        let path = if user.uid == 0 {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        } else {
            "/usr/local/bin:/usr/bin:/bin"
        };
        let mut env = vec![
            ("HOME".to_string(), user.home.clone()),
            ("USER".to_string(), user.name.clone()),
            ("LOGNAME".to_string(), user.name.clone()),
            ("SHELL".to_string(), user.shell.clone()),
            ("PATH".to_string(), path.to_string()),
            ("XDG_RUNTIME_DIR".to_string(), format!("/run/user/{}", user.uid)),
        ];
        let (lip, lport) = match self.local {
            Some(a) => (a.ip().to_string(), a.port()),
            None => ("0.0.0.0".to_string(), self.cfg.port),
        };
        env.push(("SSH_CLIENT".into(), format!("{} {} {}", self.peer.ip(), self.peer.port(), lport)));
        env.push((
            "SSH_CONNECTION".into(),
            format!("{} {} {} {}", self.peer.ip(), self.peer.port(), lip, lport),
        ));
        if let Some(p) = pty {
            env.push(("TERM".into(), if p.term.is_empty() { "vt100".into() } else { p.term.clone() }));
        }
        if let Some(t) = tty {
            env.push(("SSH_TTY".into(), t.to_string()));
        }
        env
    }

    fn start(&mut self, id: ChannelId, kind: Kind, session: &mut Session) -> Result<(), String> {
        let user = self.user.clone().ok_or("not authenticated")?;
        let handle = session.handle();
        let (pty, extra_env) = {
            let ch = self.chans.get(&id).ok_or("no such channel")?;
            if ch.started {
                return Err("channel already has a program".into());
            }
            (ch.pty.clone(), ch.env.clone())
        };
        // Subsystems never get a pty, whatever the client asked.
        let pty = if matches!(kind, Kind::Subsystem(_)) { None } else { pty };

        let shell_base = std::path::Path::new(&user.shell)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("sh")
            .to_string();
        let (prog, argv0, args): (String, String, Vec<String>) = match &kind {
            // Like /bin/login: the shell itself, argv[0] its path (a non-login
            // shell, so the console and ssh get the same environment).
            Kind::Shell => (user.shell.clone(), user.shell.clone(), vec![]),
            Kind::Exec(cmd) => (user.shell.clone(), shell_base.clone(), vec!["-c".into(), cmd.clone()]),
            Kind::Subsystem(path) => (path.clone(), path.clone(), vec![]),
        };
        let what = match &kind {
            Kind::Shell => "shell".to_string(),
            Kind::Exec(c) => format!("exec {:?}", c),
            Kind::Subsystem(p) => format!("subsystem {p}"),
        };
        let groups = accounts::groups_of(&user);
        let home_c = CString::new(user.home.clone()).map_err(|_| "bad home")?;
        let (uid, gid) = (user.uid, user.gid);

        let mut cmd = tokio::process::Command::new(&prog);
        cmd.arg0(&argv0).args(&args).env_clear().kill_on_drop(false);

        let mut master_fd: Option<Arc<OwnedFd>> = None;
        let tty_name: Option<String>;
        if let Some(p) = &pty {
            let (master, slave, name) = open_pty(p).map_err(|e| format!("openpty: {e}"))?;
            // The tty belongs to the user while the session runs (OpenSSH:
            // pty_setowner). Best effort: a pty that cannot be chowned still works.
            unsafe {
                libc::fchown(slave.as_raw_fd(), uid, gid);
                libc::fchmod(slave.as_raw_fd(), 0o620);
            }
            let s1 = slave.try_clone().map_err(|e| e.to_string())?;
            let s2 = slave.try_clone().map_err(|e| e.to_string())?;
            cmd.stdin(Stdio::from(slave)).stdout(Stdio::from(s1)).stderr(Stdio::from(s2));
            master_fd = Some(Arc::new(master));
            tty_name = Some(name);
        } else {
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            tty_name = None;
        }
        let mut env = self.base_env(&user, pty.as_ref(), tty_name.as_deref());
        for (k, v) in extra_env {
            env.retain(|(ek, _)| *ek != k);
            env.push((k, v));
        }
        cmd.envs(env);
        let with_tty = pty.is_some();
        unsafe {
            cmd.pre_exec(move || {
                // Runs in the forked child: syscalls only, nothing allocates.
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if with_tty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgroups(groups.len() as _, groups.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setresgid(gid, gid, gid) != 0 || libc::setresuid(uid, uid, uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getuid() != uid || libc::geteuid() != uid || libc::getgid() != gid || libc::getegid() != gid {
                    return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                }
                if uid != 0 && libc::setuid(0) == 0 {
                    return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                }
                if libc::chdir(home_c.as_ptr()) != 0 {
                    libc::chdir(c"/".as_ptr());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("{prog}: {e}"))?;
        let pid = child.id().unwrap_or(0) as i32;
        log(&format!(
            "{}: {} for {} (uid {}) pid {}{}",
            self.peer,
            what,
            user.name,
            uid,
            pid,
            tty_name.as_deref().map(|t| format!(" on {t}")).unwrap_or_default()
        ));

        let (tx, mut rx) = mpsc::unbounded_channel::<Input>();
        let ch = self.chans.get_mut(&id).ok_or("no such channel")?;
        ch.started = true;
        ch.input = Some(tx);
        ch.master = master_fd.clone();
        ch.pid.store(pid, Ordering::Release);
        let pid_slot = ch.pid.clone();

        if let Some(master) = master_fd {
            // SAFETY: DupFd keeps the master open (an Arc) for as long as the
            // AsyncFd lives, and nothing replaces or closes it meanwhile.
            let afd = Arc::new(unsafe { AsyncFd::register(DupFd(master.clone())) }.map_err(|e| format!("epoll: {e}"))?);
            // client -> pty
            let wfd = afd.clone();
            tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    match m {
                        Input::Data(d) => {
                            if write_all_fd(&wfd, &d).await.is_err() {
                                break;
                            }
                        }
                        // A pty has no half-close; OpenSSH ignores EOF here too.
                        Input::Eof => {}
                    }
                }
            });
            // pty -> client
            let rfd = afd.clone();
            let h = handle.clone();
            let mut reader = tokio::spawn(async move { pump_fd(&rfd, &h, id).await });
            tokio::spawn(async move {
                let status = child.wait().await;
                pid_slot.store(0, Ordering::Release);
                // Drain what the shell wrote before it exited; a background
                // job still holding the tty must not keep the channel open.
                if tokio::time::timeout(Duration::from_millis(500), &mut reader).await.is_err() {
                    reader.abort();
                }
                finish(&handle, id, status).await;
            });
        } else {
            let mut stdin = child.stdin.take();
            let mut stdout = child.stdout.take().ok_or("no stdout")?;
            let mut stderr = child.stderr.take().ok_or("no stderr")?;
            tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    match m {
                        Input::Data(d) => {
                            let Some(w) = stdin.as_mut() else { continue };
                            if w.write_all(&d).await.is_err() {
                                stdin = None;
                            }
                        }
                        Input::Eof => stdin = None,
                    }
                }
            });
            let h1 = handle.clone();
            let out = tokio::spawn(async move {
                let mut buf = vec![0u8; 32768];
                loop {
                    match stdout.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if h1.data(id, buf[..n].to_vec()).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            let h2 = handle.clone();
            let err = tokio::spawn(async move {
                let mut buf = vec![0u8; 32768];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if h2.extended_data(id, 1, buf[..n].to_vec()).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            tokio::spawn(async move {
                let status = child.wait().await;
                pid_slot.store(0, Ordering::Release);
                // Output already written must reach the client; a daemon the
                // command left behind holding the pipes must not hold the
                // channel forever.
                let both = async {
                    let _ = out.await;
                    let _ = err.await;
                };
                let _ = tokio::time::timeout(Duration::from_secs(5), both).await;
                finish(&handle, id, status).await;
            });
        }
        Ok(())
    }
}

async fn finish(handle: &Handle, id: ChannelId, status: std::io::Result<std::process::ExitStatus>) {
    match status {
        Ok(st) => {
            if let Some(code) = st.code() {
                let _ = handle.exit_status_request(id, code as u32).await;
            } else if let Some(sig) = st.signal() {
                let _ = handle
                    .exit_signal_request(id, sig_from_number(sig), st.core_dumped(), String::new(), String::new())
                    .await;
            }
        }
        Err(e) => log(&format!("wait: {e}")),
    }
    let _ = handle.eof(id).await;
    let _ = handle.close(id).await;
}

/// An fd shared with the window-change path: AsyncFd needs an owner, and the
/// master must stay open until both halves are done.
struct DupFd(Arc<OwnedFd>);
impl AsRawFd for DupFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

async fn pump_fd(afd: &AsyncFd<DupFd>, h: &Handle, id: ChannelId) {
    let mut buf = vec![0u8; 32768];
    loop {
        let mut guard = match afd.readable().await {
            Ok(g) => g,
            Err(_) => return,
        };
        let r = guard.try_io(|inner| {
            let n = unsafe { libc::read(inner.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match r {
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => {
                if h.data(id, buf[..n].to_vec()).await.is_err() {
                    return;
                }
            }
            // EIO: the last holder of the slave side closed it.
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(Err(_)) => return,
            Err(_would_block) => {}
        }
    }
}

async fn write_all_fd(afd: &AsyncFd<DupFd>, mut data: &[u8]) -> std::io::Result<()> {
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

fn set_winsize(fd: RawFd, p: &PtyReq) {
    let ws = libc::winsize { ws_row: p.rows, ws_col: p.cols, ws_xpixel: p.px, ws_ypixel: p.py };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) };
}

/// A pty pair from /dev/ptmx: (master, nonblocking + close-on-exec; slave; slave path).
fn open_pty(p: &PtyReq) -> std::io::Result<(OwnedFd, OwnedFd, String)> {
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

impl russh::server::Handler for Conn {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if !self.cfg.password_auth {
            return Ok(Auth::reject());
        }
        let Some(u) = accounts::lookup_user(user) else {
            log(&format!("{}: failed password for unknown user {user:?}", self.peer));
            return Ok(Auth::reject());
        };
        if let Err(why) = self.may_login(&u, true) {
            log(&format!("{}: password for {user}: {why}", self.peer));
            return Ok(Auth::reject());
        }
        if accounts::verify_password(&u, password) {
            log(&format!("{}: accepted password for {user}", self.peer));
            self.user = Some(u);
            self.authed.store(true, Ordering::Release);
            Ok(Auth::Accept)
        } else {
            log(&format!("{}: failed password for {user}", self.peer));
            Ok(Auth::reject())
        }
    }

    async fn auth_publickey_offered(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if self.pubkey_ok(user, key).is_some() { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        match self.pubkey_ok(user, key) {
            Some(u) => {
                log(&format!(
                    "{}: accepted publickey for {user}: {} {}",
                    self.peer,
                    key.algorithm(),
                    key.fingerprint(Default::default())
                ));
                self.user = Some(u);
                self.authed.store(true, Ordering::Release);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.user.is_none() {
            return Ok(()); // dropping `reply` rejects
        }
        self.chans.insert(channel.id(), Chan::default());
        reply.accept().await;
        // Data arrives through the Handler callbacks below; the Channel
        // object is not needed (russh ignores sends to a dropped one).
        drop(channel);
        Ok(())
    }

    async fn channel_close(&mut self, id: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(ch) = self.chans.remove(&id) {
            let pid = ch.pid.load(Ordering::Acquire);
            if pid > 0 {
                unsafe { libc::kill(-pid, libc::SIGHUP) };
            }
        }
        Ok(())
    }

    async fn channel_eof(&mut self, id: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(tx) = self.chans.get(&id).and_then(|c| c.input.as_ref()) {
            let _ = tx.send(Input::Eof);
        }
        Ok(())
    }

    async fn data(&mut self, id: ChannelId, data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(tx) = self.chans.get(&id).and_then(|c| c.input.as_ref()) {
            let _ = tx.send(Input::Data(data.to_vec()));
        }
        Ok(())
    }

    async fn pty_request(
        &mut self,
        id: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        pix_width: u32,
        pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.chans.get_mut(&id) {
            Some(ch) if !ch.started => {
                ch.pty = Some(PtyReq {
                    term: term.to_string(),
                    cols: col_width.min(u16::MAX as u32) as u16,
                    rows: row_height.min(u16::MAX as u32) as u16,
                    px: pix_width.min(u16::MAX as u32) as u16,
                    py: pix_height.min(u16::MAX as u32) as u16,
                });
                session.channel_success(id)?;
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }

    async fn env_request(
        &mut self,
        id: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.chans.get_mut(&id) {
            Some(ch) if env_allowed(name) && !ch.started => {
                ch.env.push((name.to_string(), value.to_string()));
                session.channel_success(id)?;
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }

    async fn shell_request(&mut self, id: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        self.reply_start(id, Kind::Shell, session)
    }

    async fn exec_request(&mut self, id: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).into_owned();
        self.reply_start(id, Kind::Exec(cmd), session)
    }

    async fn subsystem_request(&mut self, id: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        match (&self.cfg.sftp_server, name) {
            (Some(path), "sftp") if std::path::Path::new(path).exists() => {
                let path = path.clone();
                self.reply_start(id, Kind::Subsystem(path), session)
            }
            _ => {
                log(&format!("{}: refused subsystem {name:?}", self.peer));
                session.channel_failure(id)?;
                Ok(())
            }
        }
    }

    async fn window_change_request(
        &mut self,
        id: ChannelId,
        col_width: u32,
        row_height: u32,
        pix_width: u32,
        pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let mut ok = false;
        if let Some(ch) = self.chans.get_mut(&id) {
            if let Some(p) = ch.pty.as_mut() {
                p.cols = col_width.min(u16::MAX as u32) as u16;
                p.rows = row_height.min(u16::MAX as u32) as u16;
                p.px = pix_width.min(u16::MAX as u32) as u16;
                p.py = pix_height.min(u16::MAX as u32) as u16;
                if let Some(m) = &ch.master {
                    set_winsize(m.as_raw_fd(), p);
                }
                ok = true;
            }
        }
        if ok {
            session.channel_success(id)?;
        } else {
            session.channel_failure(id)?;
        }
        Ok(())
    }

    async fn signal(&mut self, id: ChannelId, signal: Sig, _session: &mut Session) -> Result<(), Self::Error> {
        if let (Some(ch), Some(n)) = (self.chans.get(&id), sig_number(&signal)) {
            let pid = ch.pid.load(Ordering::Acquire);
            if pid > 0 {
                unsafe { libc::kill(-pid, n) };
            }
        }
        Ok(())
    }
}

impl Conn {
    fn reply_start(&mut self, id: ChannelId, kind: Kind, session: &mut Session) -> Result<(), russh::Error> {
        match self.start(id, kind, session) {
            Ok(()) => session.channel_success(id)?,
            Err(e) => {
                log(&format!("{}: cannot start the session: {e}", self.peer));
                session.channel_failure(id)?;
            }
        }
        Ok(())
    }
}
