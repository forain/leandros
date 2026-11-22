//! Privilege-separation monitor (sshd P3/P4). This root process owns the host
//! key, every authentication decision, and every privileged action (pty open,
//! fork+exec as the user, utmp, cgroup scope, privileged-looking binds). The
//! network side runs in an unprivileged, chrooted, seccomp'd child (see
//! `netchild.rs`) that can only talk to it through two SEQPACKET sockets:
//! `sign` (host-key signatures) and `ctl` (everything else).
//!
//! The monitor is single-threaded with nonblocking sockets and deadlines: a
//! child that stalls, speaks out of turn, or attaches fds is killed.

use std::collections::HashMap;
use std::ffi::CString;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::keys::ssh_key::{Algorithm, HashAlg};
use russh::keys::{PrivateKey, PublicKey};
use russh::PublicKeyOrCertificate;

use crate::accounts::{self, User};
use crate::rpc::{self, Frame, R, W};
use crate::session::{dev_password_allowed, env_allowed, now_secs, open_pty, PtyReq};
use crate::{log, ChildStartup, Config, RootLogin};

const EMPTY_DIR: &str = "/var/empty";
const SSHD_UID: u32 = 22;
const SSHD_GID: u32 = 22;
const FRAME_STALL: Duration = Duration::from_secs(30);
const HELPER_DEADLINE: Duration = Duration::from_secs(15);
const MAX_SESSIONS: usize = 64;
const MAX_HELPERS: usize = 16;

/// The privilege-separation prerequisites: an unprivileged `sshd` account and
/// an empty root-owned `/var/empty` that nobody else can write.
pub fn check_environment() -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let u = accounts::lookup_user("sshd").ok_or("privilege separation: no \"sshd\" user in /etc/passwd")?;
    if u.uid != SSHD_UID || u.gid != SSHD_GID {
        return Err(format!("privilege separation: the sshd user must be {SSHD_UID}:{SSHD_GID}, found {}:{}", u.uid, u.gid));
    }
    let md = std::fs::metadata(EMPTY_DIR).map_err(|e| format!("privilege separation: {EMPTY_DIR}: {e}"))?;
    if !md.is_dir() || md.uid() != 0 || md.mode() & 0o022 != 0 {
        return Err(format!("privilege separation: {EMPTY_DIR} must be a root-owned directory not writable by group/other"));
    }
    let empty = std::fs::read_dir(EMPTY_DIR).map(|mut d| d.next().is_none()).unwrap_or(false);
    if !empty {
        return Err(format!("privilege separation: {EMPTY_DIR} must be empty"));
    }
    Ok(())
}

struct Sess {
    child: std::process::Child,
    pid: i32,
    tty: bool,
    _scope: Option<crate::cgsession::Scope>,
}

struct Helper {
    pid: i32,
    sock: OwnedFd,
    req: u32,
    deadline: Instant,
}

struct Mon {
    cfg: Arc<Config>,
    peer: SocketAddr,
    local: Option<SocketAddr>,
    user: Option<User>,
    key_opts: accounts::KeyOpts,
    startup: Option<ChildStartup>,
    authed: bool,
    host_key: PrivateKey,
    session_id: Option<Vec<u8>>,
    signs: u32,
    last_sign: Option<Instant>,
    tries: u32,
    child_pid: i32,
    sign: RawFd,
    ctl: RawFd,
    logr: RawFd,
    logbuf: Vec<u8>,
    sessions: HashMap<u32, Sess>,
    helpers: Vec<Helper>,
    started: Instant,
}

fn set_nonblock(fd: RawFd) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
}

fn socketpair() -> std::io::Result<(RawFd, RawFd)> {
    let mut sv = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((sv[0], sv[1]))
}

fn pfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd { fd, events: libc::POLLIN, revents: 0 }
}

/// Entry point of the per-connection root process (`sshd -R`): fd 3 is the TCP
/// connection, fd 4 the MaxStartups pipe.
pub fn run(cfg: Config, host_key: PrivateKey) -> ! {
    for fd in [3, 4] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let tcp = unsafe { std::net::TcpStream::from_raw_fd(3) };
    let startup = ChildStartup(Some(unsafe { OwnedFd::from_raw_fd(4) }));
    let Ok(peer) = tcp.peer_addr() else { std::process::exit(1) };
    let local = tcp.local_addr().ok();
    if let Err(e) = check_environment() {
        log(&format!("{peer}: {e}"));
        std::process::exit(1);
    }
    let (Ok((sign_m, sign_c)), Ok((ctl_m, ctl_c))) = (socketpair(), socketpair()) else {
        log(&format!("{peer}: socketpair: {}", std::io::Error::last_os_error()));
        std::process::exit(1);
    };
    let mut lp = [0i32; 2];
    if unsafe { libc::pipe2(lp.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        std::process::exit(1);
    }
    let (logr, logw) = (lp[0], lp[1]);
    log(&format!("connection from {peer}"));
    if std::env::var("SSHD_TEST_HOOK").as_deref() == Ok("highfd") {
        // Test only: a root-only file open above fd 1023 that the net child must not inherit.
        unsafe {
            let lim = libc::rlimit { rlim_cur: 4096, rlim_max: 4096 };
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
            let f = libc::open(c"/etc/shadow".as_ptr(), libc::O_RDONLY);
            if f >= 0 {
                libc::fcntl(f, libc::F_DUPFD, 1100);
            }
        }
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        log(&format!("{peer}: fork: {}", std::io::Error::last_os_error()));
        std::process::exit(1);
    }
    if pid == 0 {
        // The unprivileged network child: it never returns.
        unsafe {
            libc::close(sign_m);
            libc::close(ctl_m);
            libc::close(logr);
        }
        crate::netchild::run(cfg, host_key.public_key().clone(), tcp.into_raw_fd(), sign_c, ctl_c, logw, peer, local);
    }
    unsafe {
        libc::close(sign_c);
        libc::close(ctl_c);
        libc::close(logw);
    }
    drop(tcp);
    set_nonblock(sign_m);
    set_nonblock(ctl_m);
    set_nonblock(logr);
    let mut m = Mon {
        cfg: Arc::new(cfg),
        peer,
        local,
        user: None,
        key_opts: Default::default(),
        startup: Some(startup),
        authed: false,
        host_key,
        session_id: None,
        signs: 0,
        last_sign: None,
        tries: 0,
        child_pid: pid,
        sign: sign_m,
        ctl: ctl_m,
        logr,
        logbuf: Vec::new(),
        sessions: HashMap::new(),
        helpers: Vec::new(),
        started: Instant::now(),
    };
    m.main_loop()
}

impl Mon {
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
            self.set_authed();
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

    /// RevokedKeys: is the key (or certificate) listed? An unusable file refuses every key.
    fn revoked(&self, eff: &Config, subject: &russh::keys::ssh_key::public::KeyData, cert: Option<&russh::keys::Certificate>) -> bool {
        let Some(f) = eff.revoked_keys.as_deref() else { return false };
        match crate::revoke::is_revoked(f, subject, cert) {
            Ok(false) => false,
            Ok(true) => {
                log(&format!("{}: key is revoked (RevokedKeys {f})", self.peer));
                true
            }
            Err(why) => {
                log(&format!("{}: RevokedKeys file unusable, refusing public key authentication: {why}", self.peer));
                true
            }
        }
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
        if self.revoked(&eff, key.key_data(), None) {
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
        if self.revoked(&eff, cert.public_key(), Some(cert)) {
            return deny("revoked");
        }
        let now = now_secs();
        if cert.verify_signature().is_err() || now < cert.valid_after() || now >= cert.valid_before() {
            return deny("bad signature or outside its validity window");
        }
        // Who vouches for it: TrustedUserCAKeys (principal = the user name) or a cert-authority line.
        let mut line_opts: Option<crate::accounts::KeyOpts> = None;
        let mut principals: Option<String> = None;
        let mut file_principal = false;
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
        // TrustedUserCAKeys with an AuthorizedPrincipalsFile: its names replace the user name.
        if trusted {
            if let Some(pf) = eff.authorized_principals_file.as_deref() {
                let names: Vec<String> = cert.valid_principals().to_vec();
                match accounts::principals_match(&u, pf, &names) {
                    Ok(Some(o)) => {
                        if !self.opts_ok(user, &o) {
                            return None;
                        }
                        line_opts = Some(o);
                        file_principal = true;
                    }
                    Ok(None) => return deny("no principal listed in AuthorizedPrincipalsFile"),
                    Err(why) => return deny(&format!("AuthorizedPrincipalsFile unusable: {why}")),
                }
            }
        }
        // An empty principal list would match anyone: refuse it.
        let allowed = file_principal || match &principals {
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

    // ---- plumbing -------------------------------------------------------

    fn shutdown(&mut self, code: i32) -> ! {
        if self.child_pid > 0 {
            unsafe {
                libc::kill(self.child_pid, libc::SIGKILL);
                libc::waitpid(self.child_pid, std::ptr::null_mut(), 0);
            }
            self.child_pid = 0;
        }
        for (_, s) in self.sessions.drain() {
            unsafe { libc::kill(-s.pid, libc::SIGHUP) };
            if s.tty {
                crate::utmpx::logout_pid(s.pid);
            }
        }
        for h in self.helpers.drain(..) {
            unsafe {
                libc::kill(h.pid, libc::SIGKILL);
                libc::waitpid(h.pid, std::ptr::null_mut(), 0);
            }
        }
        std::process::exit(code)
    }

    fn fatal(&mut self, why: &str) -> ! {
        log(&format!("{}: {why}; closing the connection", self.peer));
        self.shutdown(1)
    }

    fn send_ctl(&mut self, ty: u8, req: u32, body: &[u8], fds: &[i32]) {
        if let Err(e) = rpc::send(self.ctl, ty, req, body, fds) {
            self.fatal(&format!("cannot reach the network process: {e}"));
        }
    }

    fn drain_log(&mut self) {
        let mut buf = [0u8; 2048];
        loop {
            let n = unsafe { libc::read(self.logr, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            self.logbuf.extend_from_slice(&buf[..n as usize]);
        }
        while let Some(i) = self.logbuf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.logbuf.drain(..=i).collect();
            let s: String = String::from_utf8_lossy(&line[..line.len() - 1]).chars().filter(|c| !c.is_control()).take(480).collect();
            log(&s);
        }
        if self.logbuf.len() > 4096 {
            self.logbuf.clear();
        }
    }

    fn main_loop(&mut self) -> ! {
        loop {
            let nh = self.helpers.len();
            let mut fds = vec![pfd(self.sign), pfd(self.ctl), pfd(self.logr)];
            for h in &self.helpers {
                fds.push(pfd(h.sock.as_raw_fd()));
            }
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 100) };
            if fds[2].revents != 0 {
                self.drain_log();
            }
            if fds[0].revents != 0 {
                self.read_sock(true);
            }
            if fds[1].revents != 0 {
                self.read_sock(false);
            }
            let ready: Vec<usize> = (0..nh).filter(|i| fds[3 + i].revents != 0).collect();
            self.finish_helpers(&ready);
            self.housekeeping();
        }
    }

    fn read_sock(&mut self, sign: bool) {
        let fd = if sign { self.sign } else { self.ctl };
        for _ in 0..32 {
            match rpc::recv(fd) {
                Ok(Some((fr, fds))) => {
                    if !fds.is_empty() {
                        for f in fds {
                            unsafe { libc::close(f) };
                        }
                        self.fatal("the network process attached file descriptors");
                    }
                    match fr {
                        Ok(f) => {
                            if sign {
                                self.on_sign(f)
                            } else {
                                self.on_ctl(f)
                            }
                        }
                        Err(e) => self.fatal(&format!("malformed frame from the network process: {e:?}")),
                    }
                }
                Ok(None) => {
                    self.drain_log();
                    self.housekeeping();
                    log(&format!("{}: connection closed", self.peer));
                    self.shutdown(0)
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => self.fatal(&format!("receive from the network process: {e}")),
            }
        }
    }

    fn housekeeping(&mut self) {
        if self.child_pid > 0 {
            let mut st = 0;
            if unsafe { libc::waitpid(self.child_pid, &mut st, libc::WNOHANG) } == self.child_pid {
                self.child_pid = 0;
                self.drain_log();
                if libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0 {
                    log(&format!("{}: connection closed", self.peer));
                    self.shutdown(0)
                }
                log(&format!("{}: connection process ended abnormally (status {st:#x})", self.peer));
                self.shutdown(1)
            }
        }
        let grace = self.cfg.login_grace_secs;
        if !self.authed && grace > 0 && self.started.elapsed() >= Duration::from_secs(grace) {
            log(&format!("{}: login grace time exceeded", self.peer));
            self.shutdown(1)
        }
        // Sessions that ended.
        let mut done = Vec::new();
        for (c, s) in self.sessions.iter_mut() {
            if let Ok(Some(st)) = s.child.try_wait() {
                done.push((*c, st));
            }
        }
        for (c, st) in done {
            if let Some(s) = self.sessions.remove(&c) {
                if s.tty {
                    crate::utmpx::logout_pid(s.pid);
                }
            }
            let mut w = W::default();
            w.u32(c).u32(st.code().map(|x| x as u32).unwrap_or(u32::MAX)).u32(st.signal().unwrap_or(0) as u32).u8(st.core_dumped() as u8);
            self.send_ctl(rpc::SESSION_EXIT, 0, &w.0, &[]);
        }
        // Forward helpers that took too long.
        let now = Instant::now();
        let mut i = 0;
        while i < self.helpers.len() {
            if now >= self.helpers[i].deadline {
                let h = self.helpers.remove(i);
                unsafe {
                    libc::kill(h.pid, libc::SIGKILL);
                    libc::waitpid(h.pid, std::ptr::null_mut(), 0);
                }
                let mut w = W::default();
                w.str(b"timed out");
                self.send_ctl(rpc::ERR, h.req, &w.0, &[]);
            } else {
                i += 1;
            }
        }
        let _ = FRAME_STALL;
    }

    fn finish_helpers(&mut self, ready: &[usize]) {
        for &i in ready.iter().rev() {
            let h = self.helpers.remove(i);
            let res = rpc::recv(h.sock.as_raw_fd());
            unsafe { libc::waitpid(h.pid, std::ptr::null_mut(), 0) };
            match res {
                Ok(Some((Ok(f), fds))) if matches!(f.ty, rpc::FWD_FD | rpc::FWD_LISTEN | rpc::ERR) && fds.len() <= 1 => {
                    self.send_ctl(f.ty, h.req, &f.body, &fds);
                    for fd in fds {
                        unsafe { libc::close(fd) };
                    }
                }
                _ => {
                    let mut w = W::default();
                    w.str(b"helper failed");
                    self.send_ctl(rpc::ERR, h.req, &w.0, &[]);
                }
            }
        }
    }

    // ---- host-key signing -------------------------------------------------

    fn on_sign(&mut self, f: Frame) {
        if f.ty != rpc::SIGN {
            self.fatal("unexpected message on the signing channel");
        }
        let limit_ok = if self.authed {
            self.signs < 64 && self.last_sign.map_or(true, |t| t.elapsed() >= Duration::from_secs(10))
        } else {
            self.signs < 8
        };
        let res = if limit_ok { self.sign_request(&f.body) } else { Err("signing rate limit".to_string()) };
        self.signs += 1;
        self.last_sign = Some(Instant::now());
        let r = match res {
            Ok(sig) => rpc::send(self.sign, rpc::SIG, f.req, &sig, &[]),
            Err(e) => {
                log(&format!("{}: refused to sign: {e}", self.peer));
                let mut w = W::default();
                w.str(e.as_bytes());
                rpc::send(self.sign, rpc::ERR, f.req, &w.0, &[])
            }
        };
        if let Err(e) = r {
            self.fatal(&format!("cannot reach the network process: {e}"));
        }
    }

    fn sign_request(&mut self, body: &[u8]) -> Result<Vec<u8>, String> {
        let mut r = R(body);
        let alg = r.str().map_err(rerr)?;
        let hash = r.u8().map_err(rerr)?;
        let blob = r.str().map_err(rerr)?;
        let h = r.str().map_err(rerr)?;
        r.done().map_err(rerr)?;
        if !matches!(h.len(), 32 | 48 | 64) {
            return Err("bad exchange-hash length".into());
        }
        let pubkey = self.host_key.public_key().to_bytes().map_err(|e| e.to_string())?;
        if blob != pubkey.as_slice() {
            return Err("not the host key".into());
        }
        let alg = Algorithm::new(std::str::from_utf8(alg).map_err(|_| "algorithm")?).map_err(|e| e.to_string())?;
        let ka = self.host_key.algorithm();
        if !(ka == alg || (matches!(alg, Algorithm::Rsa { .. }) && matches!(ka, Algorithm::Rsa { .. }))) {
            return Err("algorithm does not match the host key".into());
        }
        let hash = match hash {
            0 => None,
            1 => Some(HashAlg::Sha256),
            2 => Some(HashAlg::Sha512),
            _ => return Err("hash algorithm".into()),
        };
        let sig = russh::sign_with_hash_alg(&russh::keys::PrivateKeyWithHashAlg::new(Arc::new(self.host_key.clone()), hash), h)
            .map_err(|e| e.to_string())?;
        // The first exchange hash is the session id, set exactly once.
        if self.session_id.is_none() {
            self.session_id = Some(h.to_vec());
        }
        Ok(sig)
    }

    // ---- control channel ---------------------------------------------------

    fn on_ctl(&mut self, f: Frame) {
        use rpc::*;
        match f.ty {
            AUTH_PASSWORD | AUTH_PUBKEY_QUERY | AUTH_PUBKEY if !self.authed => {
                let ok = match f.ty {
                    AUTH_PASSWORD => self.on_password(&f),
                    AUTH_PUBKEY_QUERY => self.on_query(&f),
                    _ => self.on_pubkey(&f),
                };
                match ok {
                    Ok(true) => {
                        let mut w = W::default();
                        w.u8(self.key_opts.no_pty as u8);
                        self.send_ctl(AUTH_OK, f.req, &w.0, &[]);
                    }
                    Ok(false) => self.send_ctl(AUTH_FAIL, f.req, &[], &[]),
                    Err(e) => self.fatal(&e),
                }
            }
            SESSION_OPEN | SESSION_SIGNAL | SESSION_HANGUP | DIRECT_TCPIP | TCPIP_FORWARD if !self.authed => {
                log(&format!("{}: forged auth refused: request {} before authentication", self.peer, f.ty));
                self.shutdown(1)
            }
            SESSION_OPEN | SESSION_SIGNAL | SESSION_HANGUP | DIRECT_TCPIP | TCPIP_FORWARD => {
                let r = match f.ty {
                    SESSION_OPEN => self.session_open(&f),
                    SESSION_SIGNAL => self.session_signal(&f, false),
                    SESSION_HANGUP => self.session_signal(&f, true),
                    DIRECT_TCPIP => self.forward(&f, false),
                    _ => self.forward(&f, true),
                };
                match r {
                    Ok(()) => {}
                    Err(e) => {
                        log(&format!("{}: request {} refused: {e}", self.peer, f.ty));
                        let mut w = W::default();
                        w.str(e.as_bytes());
                        self.send_ctl(ERR, f.req, &w.0, &[]);
                    }
                }
            }
            _ => self.fatal(&format!("unexpected message type {} from the network process", f.ty)),
        }
    }

    fn set_authed(&mut self) {
        self.startup = None; // tells the listener this connection no longer counts against MaxStartups
        self.authed = true;
    }

    fn count_try(&mut self) -> Result<(), String> {
        self.tries += 1;
        if self.tries > (self.cfg.max_auth_tries as u32).saturating_mul(3).saturating_add(3) {
            return Err("too many authentication attempts".into());
        }
        Ok(())
    }

    fn on_password(&mut self, f: &Frame) -> Result<bool, String> {
        let mut r = R(&f.body);
        let user = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
        let pw = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
        let method = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
        r.done().map_err(rerr)?;
        self.count_try()?;
        Ok(self.check_password(&user, &pw, &method))
    }

    fn decode_key(kind: u8, blob: &[u8], hash: Option<HashAlg>) -> Option<PublicKeyOrCertificate> {
        match kind {
            0 => Some(PublicKeyOrCertificate::PublicKey { key: PublicKey::from_bytes(blob).ok()?, hash_alg: hash }),
            1 => Some(PublicKeyOrCertificate::Certificate(russh::keys::Certificate::from_bytes(blob).ok()?)),
            _ => None,
        }
    }

    fn on_query(&mut self, f: &Frame) -> Result<bool, String> {
        let mut r = R(&f.body);
        let user = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
        let kind = r.u8().map_err(rerr)?;
        let blob = r.str().map_err(rerr)?;
        r.done().map_err(rerr)?;
        let Some(PublicKeyOrCertificate::PublicKey { key, .. }) = Self::decode_key(kind, blob, None) else { return Ok(false) };
        Ok(self.pubkey_ok(&user, &key).is_some() || self.cert_possible(&user))
    }

    fn on_pubkey(&mut self, f: &Frame) -> Result<bool, String> {
        let mut r = R(&f.body);
        let user = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
        let kind = r.u8().map_err(rerr)?;
        let blob = r.str().map_err(rerr)?;
        let hash = match r.u8().map_err(rerr)? {
            1 => Some(HashAlg::Sha256),
            2 => Some(HashAlg::Sha512),
            _ => None,
        };
        let signed = r.str().map_err(rerr)?;
        let sig = r.str().map_err(rerr)?;
        r.done().map_err(rerr)?;
        let Some(sid) = self.session_id.clone() else {
            return Err("publickey authentication before the key exchange was signed".into());
        };
        self.count_try()?;
        let Some(koc) = Self::decode_key(kind, blob, hash) else { return Ok(false) };
        if !crate::userauth::verify_userauth(&user, &koc, signed, sig, &sid, Some(&sid)) {
            log(&format!("{}: publickey signature check failed for {user} (forged or unsupported signature)", self.peer));
            return Ok(false);
        }
        let accepted = match &koc {
            PublicKeyOrCertificate::PublicKey { key, .. } => self.pubkey_ok(&user, key).map(|x| {
                log(&format!("{}: accepted publickey for {user}: {} {}", self.peer, key.algorithm(), key.fingerprint(Default::default())));
                x
            }),
            PublicKeyOrCertificate::Certificate(cert) => self.cert_ok(&user, cert).map(|x| {
                log(&format!(
                    "{}: accepted certificate for {user}: {} id {:?} serial {} CA {}",
                    self.peer,
                    cert.algorithm(),
                    cert.key_id(),
                    cert.serial(),
                    cert.signature_key().fingerprint(Default::default())
                ));
                x
            }),
        };
        match accepted {
            Some((u, opts, eff)) => {
                self.user = Some(u);
                self.key_opts = opts;
                self.cfg = Arc::new(eff);
                self.set_authed();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ---- sessions ----------------------------------------------------------

    fn session_signal(&mut self, f: &Frame, hangup: bool) -> Result<(), String> {
        let mut r = R(&f.body);
        let chan = r.u32().map_err(rerr)?;
        let sig = if hangup { libc::SIGHUP } else { r.u32().map_err(rerr)? as i32 };
        r.done().map_err(rerr)?;
        const OK_SIGS: [i32; 13] = [
            libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGILL, libc::SIGABRT, libc::SIGFPE, libc::SIGKILL,
            libc::SIGUSR1, libc::SIGSEGV, libc::SIGUSR2, libc::SIGPIPE, libc::SIGALRM, libc::SIGTERM,
        ];
        if !OK_SIGS.contains(&sig) {
            return Err("signal not allowed".into());
        }
        if let Some(s) = self.sessions.get(&chan) {
            unsafe { libc::kill(-s.pid, sig) };
        }
        // Fire-and-forget: the child expects no reply.
        Ok(())
    }

    fn session_open(&mut self, f: &Frame) -> Result<(), String> {
        let user = self.user.clone().ok_or("not authenticated")?;
        let mut r = R(&f.body);
        let chan = r.u32().map_err(rerr)?;
        let kind = r.u8().map_err(rerr)?;
        let arg = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
        let has_pty = r.u8().map_err(rerr)? == 1;
        let mut pty = crate::session::PtyReq { term: String::new(), cols: 80, rows: 24, px: 0, py: 0 };
        if has_pty {
            pty.term = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
            pty.cols = r.u32().map_err(rerr)?.min(65535) as u16;
            pty.rows = r.u32().map_err(rerr)?.min(65535) as u16;
            pty.px = r.u32().map_err(rerr)?.min(65535) as u16;
            pty.py = r.u32().map_err(rerr)?.min(65535) as u16;
        }
        let nenv = r.u32().map_err(rerr)?;
        if nenv > 64 {
            return Err("too many environment variables".into());
        }
        let mut extra_env: Vec<(String, String)> = Vec::new();
        for _ in 0..nenv {
            let k = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
            let v = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
            if env_allowed(&k) {
                extra_env.push((k, v));
            }
        }
        r.done().map_err(rerr)?;
        if self.sessions.len() >= MAX_SESSIONS || self.sessions.contains_key(&chan) {
            return Err("too many sessions".into());
        }

        enum K {
            Shell,
            Exec(String),
            Sub(String),
        }
        let mut kind = match kind {
            0 => K::Shell,
            1 => K::Exec(arg),
            2 => match (&self.cfg.sftp_server, arg.as_str()) {
                (Some(p), "sftp") if std::path::Path::new(p).exists() => K::Sub(p.clone()),
                _ => return Err(format!("refused subsystem {arg:?}")),
            },
            _ => return Err("bad session kind".into()),
        };
        let pty = if has_pty && !self.key_opts.no_pty && !matches!(kind, K::Sub(_)) { Some(pty) } else { None };
        let mut forced_env: Option<String> = None;
        let adm = self.cfg.force_command.clone();
        if adm.as_deref() == Some("internal-sftp") {
            match (&self.cfg.sftp_server, &kind) {
                (Some(p), K::Sub(_)) => kind = K::Sub(p.clone()),
                _ => return Err("ForceCommand internal-sftp: only the sftp subsystem is allowed".into()),
            }
        } else if let Some(fc) = adm.or_else(|| self.key_opts.command.clone()) {
            match &kind {
                K::Sub(_) => return Err("subsystems are refused for a command= key".into()),
                K::Exec(orig) => forced_env = Some(orig.clone()),
                K::Shell => {}
            }
            kind = K::Exec(fc);
        }
        let shell_base = std::path::Path::new(&user.shell).file_name().and_then(|s| s.to_str()).unwrap_or("sh").to_string();
        let (prog, argv0, args): (String, String, Vec<String>) = match &kind {
            K::Shell => (user.shell.clone(), user.shell.clone(), vec![]),
            K::Exec(cmd) => (user.shell.clone(), shell_base, vec!["-c".into(), cmd.clone()]),
            K::Sub(path) => (path.clone(), path.clone(), vec![]),
        };
        let what = match &kind {
            K::Shell => "shell".to_string(),
            K::Exec(c) => format!("exec {:?} ({} bytes, args redacted)", c.split_whitespace().next().unwrap_or(""), c.len()),
            K::Sub(p) => format!("subsystem {p}"),
        };
        let groups = accounts::groups_of(&user);
        let home_c = CString::new(user.home.clone()).map_err(|_| "bad home")?;
        let (uid, gid) = (user.uid, user.gid);
        let scope = crate::cgsession::prepare(uid, gid);
        let scope_procs = scope.as_ref().map(|s| s.procs());

        let mut cmd = Command::new(&prog);
        cmd.arg0(&argv0).args(&args).env_clear();
        let mut master: Option<OwnedFd> = None;
        let mut tty_name: Option<String> = None;
        if let Some(p) = &pty {
            let (m, slave, name) = open_pty(p).map_err(|e| format!("openpty: {e}"))?;
            unsafe {
                libc::fchown(slave.as_raw_fd(), uid, gid);
                libc::fchmod(slave.as_raw_fd(), 0o620);
            }
            let s1 = slave.try_clone().map_err(|e| e.to_string())?;
            let s2 = slave.try_clone().map_err(|e| e.to_string())?;
            cmd.stdin(Stdio::from(slave)).stdout(Stdio::from(s1)).stderr(Stdio::from(s2));
            master = Some(m);
            tty_name = Some(name);
        } else {
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
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
        let pid = child.id() as i32;
        log(&format!(
            "{}: {} for {} (uid {}) pid {}{}",
            self.peer, what, user.name, uid, pid,
            tty_name.as_deref().map(|t| format!(" on {t}")).unwrap_or_default()
        ));
        if let Some(t) = tty_name.as_deref() {
            let line = t.strip_prefix("/dev/").unwrap_or(t);
            crate::utmpx::login(pid, line.as_bytes(), user.name.as_bytes(), self.peer.ip().to_string().as_bytes(), uid as u32);
        }
        let mut fds: Vec<OwnedFd> = Vec::new();
        if let Some(m) = master {
            fds.push(m);
        } else {
            fds.push(OwnedFd::from(child.stdin.take().ok_or("no stdin")?));
            fds.push(OwnedFd::from(child.stdout.take().ok_or("no stdout")?));
            fds.push(OwnedFd::from(child.stderr.take().ok_or("no stderr")?));
        }
        for fd in &fds {
            set_nonblock(fd.as_raw_fd());
        }
        let mut w = W::default();
        w.u32(chan).u8(with_tty as u8).u32(pid as u32);
        let raw: Vec<i32> = fds.iter().map(|f| f.as_raw_fd()).collect();
        self.sessions.insert(chan, Sess { child, pid, tty: with_tty, _scope: scope });
        self.send_ctl(rpc::SESSION_FDS, f.req, &w.0, &raw);
        drop(fds);
        Ok(())
    }

    // ---- forwarding ----------------------------------------------------------

    fn forward(&mut self, f: &Frame, listen: bool) -> Result<(), String> {
        let user = self.user.clone().ok_or("not authenticated")?;
        let mut r = R(&f.body);
        let host = String::from_utf8_lossy(r.str().map_err(rerr)?).into_owned();
        let port = r.u32().map_err(rerr)?;
        r.done().map_err(rerr)?;
        if !self.forwarding_allowed(!listen) {
            return Err("port forwarding disabled (AllowTcpForwarding or the key)".into());
        }
        if port > 65535 {
            return Err("bad port".into());
        }
        if listen {
            // Like OpenSSH: only root may bind a privileged port (0 = server picks).
            if port != 0 && port < 1024 && user.uid != 0 {
                return Err("privileged port, not root".into());
            }
            let shown = if host.is_empty() { "localhost" } else { host.as_str() };
            if !accounts::forward_permitted(&self.key_opts.permit_listen, shown, port) {
                return Err("permitlisten".into());
            }
        } else if !accounts::forward_permitted(&self.key_opts.permit_open, &host, port) {
            return Err("permitopen".into());
        }
        if self.helpers.len() >= MAX_HELPERS {
            return Err("too many pending forwards".into());
        }
        let (a, b) = socketpair().map_err(|e| e.to_string())?;
        let groups = accounts::groups_of(&user);
        let (uid, gid) = (user.uid, user.gid);
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(a);
                libc::close(b);
            }
            return Err("fork failed".into());
        }
        if pid == 0 {
            unsafe {
                crate::sandbox::close_except(&[b]);
                let fail = |msg: &str| -> ! {
                    let mut w = W::default();
                    w.str(msg.as_bytes());
                    let _ = rpc::send(b, rpc::ERR, 0, &w.0, &[]);
                    libc::_exit(1)
                };
                if uid != 0 {
                    if libc::setgroups(groups.len() as _, groups.as_ptr()) != 0
                        || libc::setresgid(gid, gid, gid) != 0
                        || libc::setresuid(uid, uid, uid) != 0
                        || libc::getuid() != uid
                        || libc::geteuid() != uid
                        || libc::setuid(0) == 0
                    {
                        fail("cannot drop privileges");
                    }
                }
                if listen {
                    // GatewayPorts no: remote forwards listen on loopback only.
                    match std::net::TcpListener::bind(("127.0.0.1", port as u16)) {
                        Ok(l) => {
                            let p = l.local_addr().map(|a| a.port()).unwrap_or(port as u16);
                            let mut w = W::default();
                            w.u32(p as u32);
                            let _ = rpc::send(b, rpc::FWD_LISTEN, 0, &w.0, &[l.as_raw_fd()]);
                        }
                        Err(e) => fail(&format!("bind failed: {e}")),
                    }
                } else {
                    match std::net::TcpStream::connect((host.as_str(), port as u16)) {
                        Ok(s) => {
                            let _ = rpc::send(b, rpc::FWD_FD, 0, &[], &[s.as_raw_fd()]);
                        }
                        Err(e) => fail(&format!("connect failed: {e}")),
                    }
                }
                libc::_exit(0)
            }
        }
        unsafe { libc::close(b) };
        set_nonblock(a);
        self.helpers.push(Helper { pid, sock: unsafe { OwnedFd::from_raw_fd(a) }, req: f.req, deadline: Instant::now() + HELPER_DEADLINE });
        Ok(())
    }
}

fn rerr(e: rpc::RpcError) -> String {
    format!("malformed request: {e:?}")
}
