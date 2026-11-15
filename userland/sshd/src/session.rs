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
    /// Options of the authorized_keys line that authenticated this connection.
    key_opts: crate::accounts::KeyOpts,
    /// MaxStartups slot, held until the client authenticates or disconnects.
    startup: Option<crate::ChildStartup>,
    chans: HashMap<ChannelId, Chan>,
    /// Active `-R` listeners, keyed by (requested address, port).
    fwds: HashMap<(String, u32), tokio::task::AbortHandle>,
}

pub async fn run_connection(
    rcfg: Arc<russh::server::Config>,
    cfg: Arc<Config>,
    stream: TcpStream,
    peer: SocketAddr,
    local: Option<SocketAddr>,
    startup: crate::ChildStartup,
) {
    log(&format!("connection from {peer}"));
    let authed = Arc::new(AtomicBool::new(false));
    let grace = Duration::from_secs(cfg.login_grace_secs);
    let conn = Conn { cfg, peer, local, user: None, authed: authed.clone(), key_opts: Default::default(), startup: Some(startup), chans: HashMap::new(), fwds: HashMap::new() };
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
        for (_, h) in self.fwds.drain() {
            h.abort();
        }
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
    /// This connection's configuration for `name`: sshd_config plus the Match blocks that apply.
    fn eff_for(&self, name: &str) -> Config {
        if self.cfg.matches.is_empty() {
            return (*self.cfg).clone();
        }
        let groups = accounts::lookup_user(name).map(|u| accounts::group_names(&u)).unwrap_or_default();
        self.cfg.effective(name, &groups, self.peer, self.local)
    }

    /// Does the TCP forwarding policy (sshd_config AllowTcpForwarding and the
    /// key's no-port-forwarding) allow a `-L` (local) or `-R` request?
    fn forwarding_allowed(&self, local: bool) -> bool {
        use crate::TcpFwd::*;
        if self.key_opts.no_port_forwarding {
            return false;
        }
        match self.cfg.allow_tcp_forwarding {
            All => true,
            No => false,
            Local => local,
            Remote => !local,
        }
    }

    fn may_login(&self, cfg: &Config, user: &User, by_password: bool) -> Result<(), &'static str> {
        if user.uid == 0 {
            match cfg.permit_root_login {
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

    /// The password check shared by `password` and `keyboard-interactive`.
    fn check_password(&mut self, user: &str, password: &str, method: &str) -> bool {
        let eff = self.eff_for(user);
        if !eff.password_auth {
            return false;
        }
        let Some(u) = accounts::lookup_user(user) else {
            log(&format!("{}: failed {method} for unknown user {user:?}", self.peer));
            return false;
        };
        if let Err(why) = self.may_login(&eff, &u, true) {
            log(&format!("{}: {method} for {user}: {why}", self.peer));
            return false;
        }
        if password == user && !dev_password_allowed(eff.dev_password_auth, self.peer) {
            log(&format!("{}: {method} for {user}: dev password refused from the network (use a key)", self.peer));
            return false;
        }
        if accounts::verify_password(&u, password) {
            log(&format!("{}: accepted {method} for {user}", self.peer));
            accounts::rehash_legacy(&u, password);
            self.user = Some(u);
            self.cfg = Arc::new(eff);
            self.startup = None;
            self.authed.store(true, Ordering::Release);
            true
        } else {
            log(&format!("{}: failed {method} for {user}", self.peer));
            false
        }
    }

    /// expiry-time= and from= of an authorized_keys line (also a cert-authority line).
    fn opts_ok(&self, user: &str, o: &crate::accounts::KeyOpts) -> bool {
        if let Some(exp) = o.expiry {
            if now_secs() >= exp {
                log(&format!("{}: publickey for {user}: expiry-time passed", self.peer));
                return false;
            }
        }
        if let Some(from) = &o.from {
            if !accounts::from_matches(from, self.peer.ip()) {
                log(&format!("{}: publickey for {user}: from= does not allow this address", self.peer));
                return false;
            }
        }
        true
    }

    fn pubkey_ok(&self, user: &str, key: &PublicKey) -> Option<(User, crate::accounts::KeyOpts, Config)> {
        let eff = self.eff_for(user);
        if !eff.pubkey_auth {
            return None;
        }
        let u = accounts::lookup_user(user)?;
        if let Err(why) = self.may_login(&eff, &u, false) {
            log(&format!("{}: publickey for {user}: {why}", self.peer));
            return None;
        }
        match accounts::key_authorized(&u, &eff.authorized_keys_file, key) {
            Ok(Some(o)) => {
                if !self.opts_ok(user, &o) {
                    return None;
                }
                Some((u, o, eff))
            }
            Ok(None) => None,
            Err(why) => {
                log(&format!("{}: authorized_keys of {user} ignored: {why}", self.peer));
                None
            }
        }
    }

    /// Could a certificate for `user` be accepted at all (a CA is configured)?
    /// Used for the "would this key do" probe: russh hands that probe the
    /// certificate's inner key, so it can only be answered by "a CA exists";
    /// the real decision is `cert_ok`.
    fn cert_possible(&self, user: &str) -> bool {
        let eff = self.eff_for(user);
        if !eff.pubkey_auth {
            return false;
        }
        let Some(u) = accounts::lookup_user(user) else { return false };
        eff.trusted_user_ca_keys.as_deref().map_or(false, |f| !accounts::trusted_cas(f).is_empty())
            || !accounts::cert_authorities(&u, &eff.authorized_keys_file).is_empty()
    }

    /// Verify an OpenSSH user certificate: signed by a trusted CA, inside its
    /// validity window, `user` (or the line's principals=) among its
    /// principals, only known critical options. Returns the key options the
    /// certificate and the CA line impose.
    fn cert_ok(&self, user: &str, cert: &russh::keys::Certificate) -> Option<(User, crate::accounts::KeyOpts, Config)> {
        let eff = self.eff_for(user);
        let deny = |why: &str| -> Option<(User, crate::accounts::KeyOpts, Config)> {
            log(&format!("{}: certificate for {user} (key id {:?}, serial {}): {why}", self.peer, cert.key_id(), cert.serial()));
            None
        };
        if !eff.pubkey_auth {
            return None;
        }
        let u = accounts::lookup_user(user)?;
        if let Err(why) = self.may_login(&eff, &u, false) {
            return deny(why);
        }
        if cert.cert_type() != russh::keys::ssh_key::certificate::CertType::User {
            return deny("not a user certificate");
        }
        let now = now_secs();
        if cert.verify_signature().is_err() || now < cert.valid_after() || now >= cert.valid_before() {
            return deny("bad signature or outside its validity window");
        }
        // Who vouches for it: TrustedUserCAKeys (principal = the user name) or a cert-authority line.
        let mut line_opts: Option<crate::accounts::KeyOpts> = None;
        let mut principals: Option<String> = None;
        let trusted = eff
            .trusted_user_ca_keys
            .as_deref()
            .map_or(false, |f| accounts::trusted_cas(f).iter().any(|k| k.key_data() == cert.signature_key()));
        if !trusted {
            for (ca, o) in accounts::cert_authorities(&u, &eff.authorized_keys_file) {
                if ca.key_data() == cert.signature_key() {
                    if !self.opts_ok(user, &o) {
                        return None;
                    }
                    principals = o.principals.clone();
                    line_opts = Some(o);
                    break;
                }
            }
            if line_opts.is_none() {
                return deny("signed by an untrusted CA");
            }
        }
        // An empty principal list would match anyone: refuse it.
        let allowed = match &principals {
            Some(list) => cert.valid_principals().iter().any(|p| crate::pattern_list_match(list, p)),
            None => cert.valid_principals().iter().any(|p| p == user),
        };
        if !allowed {
            return deny("no matching principal");
        }
        let mut o = line_opts.unwrap_or_default();
        o.cert_authority = false;
        for (k, v) in cert.critical_options().iter() {
            match k.as_str() {
                "force-command" => {
                    if o.command.as_deref().map_or(false, |c| c != v) {
                        return deny("force-command conflicts with command=");
                    }
                    o.command = Some(v.clone());
                }
                "source-address" => {
                    if !accounts::from_matches(v, self.peer.ip()) {
                        return deny("source-address does not allow this address");
                    }
                }
                other => return deny(&format!("unsupported critical option {other}")),
            }
        }
        if !cert.extensions().contains_key("permit-port-forwarding") {
            o.no_port_forwarding = true;
        }
        if !cert.extensions().contains_key("permit-pty") {
            o.no_pty = true;
        }
        Some((u, o, eff))
    }

    fn base_env(&self, user: &User, pty: Option<&PtyReq>, tty: Option<&str>) -> Vec<(String, String)> {
        let path = if user.uid == 0 {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        } else {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
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

        // authorized_keys command=: run it whatever the client asked for; the
        // client's own command line is passed in SSH_ORIGINAL_COMMAND.
        let mut kind = kind;
        let mut forced_env: Option<String> = None;
        // sshd_config ForceCommand wins over the key's command= (OpenSSH).
        let adm = self.cfg.force_command.clone();
        if adm.as_deref() == Some("internal-sftp") {
            match (&self.cfg.sftp_server, &kind) {
                (Some(p), Kind::Subsystem(_)) => kind = Kind::Subsystem(p.clone()),
                _ => return Err("ForceCommand internal-sftp: only the sftp subsystem is allowed".into()),
            }
        } else if let Some(fc) = adm.or_else(|| self.key_opts.command.clone()) {
            match &kind {
                Kind::Subsystem(_) => return Err("subsystems are refused for a command= key".into()),
                Kind::Exec(orig) => forced_env = Some(orig.clone()),
                Kind::Shell => {}
            }
            kind = Kind::Exec(fc);
        }

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
            // Never log the full command line: scripted logins have put secrets there.
            Kind::Exec(c) => format!("exec {:?} ({} bytes, args redacted)", c.split_whitespace().next().unwrap_or(""), c.len()),
            Kind::Subsystem(p) => format!("subsystem {p}"),
        };
        let groups = accounts::groups_of(&user);
        let home_c = CString::new(user.home.clone()).map_err(|_| "bad home")?;
        let (uid, gid) = (user.uid, user.gid);
        let scope = crate::cgsession::prepare(uid, gid);
        let scope_procs = scope.as_ref().map(|s| s.procs());

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
        if self.cfg.permit_user_environment {
            for (k, v) in self.key_opts.environment.clone() {
                env.retain(|(ek, _)| *ek != k);
                env.push((k, v));
            }
        }
        for (k, v) in extra_env {
            env.retain(|(ek, _)| *ek != k);
            env.push((k, v));
        }
        if let Some(orig) = forced_env {
            env.push(("SSH_ORIGINAL_COMMAND".into(), orig));
        }
        cmd.envs(env);
        let with_tty = pty.is_some();
        unsafe {
            cmd.pre_exec(move || {
                // Runs in the forked child: syscalls only, nothing allocates.
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                if let Some(p) = &scope_procs {
                    crate::cgsession::join(p);
                }
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
        // utmp/wtmp/lastlog for pty sessions (the session leader's exit ends it).
        if let Some(t) = tty_name.as_deref() {
            let line = t.strip_prefix("/dev/").unwrap_or(t);
            crate::utmpx::login(pid, line.as_bytes(), user.name.as_bytes(), self.peer.ip().to_string().as_bytes(), uid as u32);
        }

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
                crate::utmpx::logout_pid(pid);
                drop(scope);
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
                drop(scope);
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
        Ok(if self.check_password(user, password, "password") { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        if !self.eff_for(user).password_auth {
            return Ok(Auth::reject());
        }
        // One hidden "Password: " prompt, answered through the conversation and
        // checked as a password login is (unknown users get the prompt too, so
        // the method does not reveal which accounts exist).
        match response {
            None => Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: std::borrow::Cow::Owned(vec![("Password: ".into(), false)]),
            }),
            Some(mut r) => {
                let pw = r.next().map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                Ok(if self.check_password(user, &pw, "keyboard-interactive") { Auth::Accept } else { Auth::reject() })
            }
        }
    }

    async fn auth_publickey_offered(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if self.pubkey_ok(user, key).is_some() || self.cert_possible(user) { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        match self.pubkey_ok(user, key) {
            Some((u, opts, eff)) => {
                log(&format!(
                    "{}: accepted publickey for {user}: {} {}",
                    self.peer,
                    key.algorithm(),
                    key.fingerprint(Default::default())
                ));
                self.user = Some(u);
                self.key_opts = opts;
                self.cfg = Arc::new(eff);
                self.startup = None;
                self.authed.store(true, Ordering::Release);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn auth_openssh_certificate(&mut self, user: &str, cert: &russh::keys::Certificate) -> Result<Auth, Self::Error> {
        match self.cert_ok(user, cert) {
            Some((u, opts, eff)) => {
                log(&format!(
                    "{}: accepted certificate for {user}: {} id {:?} serial {} CA {}",
                    self.peer,
                    cert.algorithm(),
                    cert.key_id(),
                    cert.serial(),
                    cert.signature_key().fingerprint(Default::default())
                ));
                self.user = Some(u);
                self.key_opts = opts;
                self.cfg = Arc::new(eff);
                self.startup = None;
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

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        orig_addr: &str,
        orig_port: u32,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let peer = self.peer;
        if self.user.is_none() {
            return Ok(());
        }
        if !self.forwarding_allowed(true) {
            log(&format!("{peer}: direct-tcpip to {host}:{port} refused (port forwarding disabled: AllowTcpForwarding or the key)"));
            return Ok(()); // dropping `reply` rejects
        }
        if !accounts::forward_permitted(&self.key_opts.permit_open, host, port) {
            log(&format!("{peer}: direct-tcpip to {host}:{port} refused (permitopen)"));
            return Ok(());
        }
        let (host, port16) = (host.to_string(), port as u16);
        let (orig_addr, orig_port) = (orig_addr.to_string(), orig_port);
        tokio::spawn(async move {
            let conn = tokio::time::timeout(Duration::from_secs(15), TcpStream::connect((host.as_str(), port16))).await;
            let mut sock = match conn {
                Ok(Ok(s)) => s,
                other => {
                    log(&format!("{peer}: direct-tcpip to {host}:{port} failed: {other:?}"));
                    return; // dropping `reply` rejects
                }
            };
            log(&format!("{peer}: direct-tcpip {orig_addr}:{orig_port} -> {host}:{port}"));
            reply.accept().await;
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
        });
        Ok(())
    }

    async fn tcpip_forward(&mut self, address: &str, port: &mut u32, session: &mut Session) -> Result<bool, Self::Error> {
        let peer = self.peer;
        if self.user.is_none() {
            return Ok(false);
        }
        if !self.forwarding_allowed(false) {
            log(&format!("{peer}: tcpip-forward {address}:{port} refused (port forwarding disabled: AllowTcpForwarding or the key)"));
            return Ok(false);
        }
        // Like OpenSSH: only root may bind a privileged port (0 = server picks).
        if *port != 0 && *port < 1024 && self.user.as_ref().map(|u| u.uid != 0).unwrap_or(true) {
            log(&format!("{peer}: tcpip-forward {address}:{port} refused (privileged port, not root)"));
            return Ok(false);
        }
        let shown = if address.is_empty() { "localhost" } else { address };
        if !accounts::forward_permitted(&self.key_opts.permit_listen, shown, *port) {
            log(&format!("{peer}: tcpip-forward {shown}:{port} refused (permitlisten)"));
            return Ok(false);
        }
        // GatewayPorts no: remote forwards listen on loopback only.
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", *port as u16)).await {
            Ok(l) => l,
            Err(e) => {
                log(&format!("{peer}: tcpip-forward {shown}:{port}: bind failed: {e}"));
                return Ok(false);
            }
        };
        let bound = listener.local_addr().map(|a| a.port() as u32).unwrap_or(*port);
        let key = (address.to_string(), bound);
        if self.fwds.contains_key(&key) {
            return Ok(false);
        }
        *port = bound;
        log(&format!("{peer}: tcpip-forward listening on 127.0.0.1:{bound} for {shown}"));
        let handle = session.handle();
        let addr = address.to_string();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, from)) = listener.accept().await else { return };
                let (handle, addr) = (handle.clone(), addr.clone());
                tokio::spawn(async move {
                    let ch = match handle
                        .channel_open_forwarded_tcpip(addr, bound, from.ip().to_string(), from.port() as u32)
                        .await
                    {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let mut stream = ch.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
                });
            }
        });
        self.fwds.insert(key, task.abort_handle());
        Ok(true)
    }

    async fn cancel_tcpip_forward(&mut self, address: &str, port: u32, _session: &mut Session) -> Result<bool, Self::Error> {
        match self.fwds.remove(&(address.to_string(), port)) {
            Some(h) => {
                h.abort();
                Ok(true)
            }
            None => Ok(false),
        }
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
        let no_pty = self.key_opts.no_pty;
        match self.chans.get_mut(&id) {
            Some(ch) if !ch.started && !no_pty => {
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

/// May a dev password (equal to the user name) log in from `peer`? See
/// `DevPasswordAuthentication` in sshd_config.
fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn dev_password_allowed(policy: crate::DevPasswords, peer: SocketAddr) -> bool {
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
