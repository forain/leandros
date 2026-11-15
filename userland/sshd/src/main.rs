//! sshd -- the LeandrOS SSH server, built on russh.
//!
//! One root process runs the SSH protocol for every connection (tokio); each
//! shell, command or subsystem is a child that drops to the authenticated
//! user (setgroups, setresgid, setresuid, read back) before it execs. See
//! artifacts/notes/lane-gitssh-2026-10-14.md and docs/userland/programs.md.
//!
//! The root listener only accepts: it re-execs `sshd -R` once per connection
//! (socket on fd 3, startup pipe on fd 4), so a crash in one connection kills
//! only that connection. The listener enforces MaxStartups and kills a child
//! still unauthenticated after LoginGraceTime.
//!
//! Usage: sshd [-f config] [-e] [-t] [-p port] [-R]
//!   -f  configuration file (default /etc/ssh/sshd_config)
//!   -e  log to stderr instead of /var/log/sshd.log
//!   -t  check the configuration and the host key, then exit
//!   -p  override Port
//!   -R  (internal) serve the one connection on fd 3; fd 4 is the startup pipe

mod accounts;
mod cgsession;
#[path = "../../../lib/utmpx/utmpx.rs"]
mod utmpx;
#[path = "../../../lib/shadowhash/shadowhash.rs"]
mod shadowhash;
mod session;
mod userauth;
#[allow(dead_code)] // privsep P3/P4 building blocks, wired in by the monitor/netchild split
mod rpc;
#[allow(dead_code)]
mod sandbox;

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::ssh_key::LineEnding;
use russh::keys::PrivateKey;
use russh::MethodKind;
use tokio::net::TcpListener;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DevPasswords { No, Local, Yes }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RootLogin {
    Yes,
    No,
    ProhibitPassword,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub listen: Vec<String>,
    pub host_key: String,
    pub permit_root_login: RootLogin,
    pub password_auth: bool,
    /// `DevPasswordAuthentication no|local|yes` (LeandrOS extension): may a
    /// password equal to the user name (the dev images' root/root and
    /// leandro/leandro) log in? `local` (the default) allows it only from
    /// loopback, the socket_vmnet host 192.168.105.1, and QEMU user networking (10.0.2.0/24, where the host's
    /// hostfwd appears); a LAN peer must use a key or a real password.
    pub dev_password_auth: DevPasswords,
    pub pubkey_auth: bool,
    pub authorized_keys_file: String,
    pub sftp_server: Option<String>,
    pub max_auth_tries: usize,
    /// `PermitUserEnvironment yes`: honor `environment=` options of authorized_keys lines.
    pub permit_user_environment: bool,
    pub login_grace_secs: u64,
    /// `MaxStartups start:rate:full` — unauthenticated connections: above
    /// `start` new ones are dropped with probability `rate`% rising linearly
    /// to 100% at `full`.
    pub max_startups: (usize, usize, usize),
    /// `AllowTcpForwarding yes|all|no|local|remote` (combined with the per-key `no-port-forwarding`).
    pub allow_tcp_forwarding: TcpFwd,
    /// `ForceCommand cmd` (`none` = unset); `internal-sftp` forces the sftp subsystem.
    pub force_command: Option<String>,
    /// `TrustedUserCAKeys file`: CA public keys whose user certificates are accepted.
    pub trusted_user_ca_keys: Option<String>,
    /// `Match` blocks, in file order; applied per connection once the user is known.
    pub matches: Vec<MatchBlock>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpFwd { All, Local, Remote, No }

/// One `Match` block: every criterion must match; `lines` are the keywords it sets.
#[derive(Clone, Debug, Default)]
pub struct MatchBlock {
    pub all: bool,
    /// (criterion, pattern list), criterion lowercase: user, group, address, localport.
    pub crit: Vec<(String, String)>,
    pub lines: Vec<(String, String)>,
}

/// Keywords a Match block may set.
const MATCH_KEYWORDS: &[&str] = &[
    "passwordauthentication", "pubkeyauthentication", "permitrootlogin", "allowtcpforwarding",
    "forcecommand", "permituserenvironment", "authorizedkeysfile", "devpasswordauthentication",
];

/// OpenSSH pattern list: comma-separated `*`/`?` globs; a matching `!pattern`
/// vetoes the whole list, otherwise one positive match is needed.
pub fn pattern_list_match(list: &str, s: &str) -> bool {
    let mut hit = false;
    for p in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match p.strip_prefix('!') {
            Some(n) => {
                if accounts::glob(n, s) {
                    return false;
                }
            }
            None => hit |= accounts::glob(p, s),
        }
    }
    hit
}

impl Config {
    /// The configuration for one connection: this config plus the Match blocks
    /// that match. Within the matching blocks the first value of a keyword wins.
    pub fn effective(&self, user: &str, groups: &[String], peer: SocketAddr, local: Option<SocketAddr>) -> Config {
        let mut eff = self.clone();
        eff.matches.clear();
        let mut seen = std::collections::HashSet::new();
        for b in &self.matches {
            let ok = b.all
                || b.crit.iter().all(|(c, v)| match c.as_str() {
                    "user" => pattern_list_match(v, user),
                    "group" => groups.iter().any(|g| pattern_list_match(v, g)),
                    "address" => accounts::from_matches(v, peer.ip()),
                    "localport" => local.map_or(false, |l| pattern_list_match(v, &l.port().to_string())),
                    _ => false,
                });
            if !ok {
                continue;
            }
            for (k, v) in &b.lines {
                if seen.insert(k.clone()) {
                    let _ = apply_kw(&mut eff, k, v);
                }
            }
        }
        eff
    }

    /// Could any Match block (or the global section) turn this on? Decides
    /// which authentication methods the server advertises.
    pub fn any(&self, f: impl Fn(&Config) -> bool) -> bool {
        f(self)
            || self.matches.iter().any(|b| {
                let mut c = self.clone();
                for (k, v) in &b.lines {
                    let _ = apply_kw(&mut c, k, v);
                }
                f(&c)
            })
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 22,
            listen: Vec::new(),
            host_key: "/etc/ssh/ssh_host_ed25519_key".into(),
            permit_root_login: RootLogin::ProhibitPassword,
            password_auth: true,
            dev_password_auth: DevPasswords::Local,
            pubkey_auth: true,
            authorized_keys_file: ".ssh/authorized_keys".into(),
            sftp_server: Some("/usr/lib/ssh/sftp-server".into()),
            max_auth_tries: 6,
            permit_user_environment: false,
            login_grace_secs: 120,
            max_startups: (10, 30, 100),
            allow_tcp_forwarding: TcpFwd::All,
            force_command: None,
            trusted_user_ca_keys: None,
            matches: Vec::new(),
        }
    }
}

fn yes_no(v: &str, key: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err(format!("{key}: expected yes or no, got {v}")),
    }
}

fn apply_kw(cfg: &mut Config, key: &str, val: &str) -> Result<bool, String> {
    let err = |m: String| m;
    match key {
        "port" => cfg.port = val.parse().map_err(|_| err(format!("bad Port {val}")))?,
        "listenaddress" => cfg.listen.push(val.to_string()),
        "hostkey" => cfg.host_key = val.to_string(),
        "permitrootlogin" => {
            cfg.permit_root_login = match val {
                "yes" => RootLogin::Yes,
                "no" => RootLogin::No,
                "prohibit-password" | "without-password" => RootLogin::ProhibitPassword,
                _ => return Err(err(format!("bad PermitRootLogin {val}"))),
            }
        }
        "passwordauthentication" => cfg.password_auth = yes_no(val, &key).map_err(err)?,
        "devpasswordauthentication" => {
            cfg.dev_password_auth = match val.to_ascii_lowercase().as_str() {
                "no" => DevPasswords::No,
                "local" => DevPasswords::Local,
                "yes" => DevPasswords::Yes,
                _ => return Err(err(format!("bad DevPasswordAuthentication {val:?}"))),
            }
        }
        "pubkeyauthentication" => cfg.pubkey_auth = yes_no(val, &key).map_err(err)?,
        "authorizedkeysfile" => {
            cfg.authorized_keys_file = val.split_whitespace().next().unwrap_or("").to_string()
        }
        "subsystem" => {
            let mut p = val.split_whitespace();
            match (p.next(), p.next()) {
                (Some("sftp"), Some(path)) => cfg.sftp_server = Some(path.to_string()),
                _ => return Err(err(format!("unsupported Subsystem {val}"))),
            }
        }
        "permituserenvironment" => cfg.permit_user_environment = yes_no(val, "PermitUserEnvironment")?,
        "maxauthtries" => {
            cfg.max_auth_tries = val.parse().map_err(|_| err(format!("bad MaxAuthTries {val}")))?
        }
        "logingracetime" => {
            cfg.login_grace_secs =
                val.trim_end_matches('s').parse().map_err(|_| err(format!("bad LoginGraceTime {val}")))?
        }
        "maxstartups" => {
            let n: Vec<usize> = val.split(':').filter_map(|p| p.parse().ok()).collect();
            cfg.max_startups = match n[..] {
                [full] if !val.contains(':') => (full, 100, full),
                [s, r, f] if s <= f && r <= 100 => (s, r, f),
                _ => return Err(err(format!("bad MaxStartups {val}"))),
            }
        }
        "permitemptypasswords" => {
            if yes_no(val, &key).map_err(err)? {
                return Err(err("PermitEmptyPasswords yes is not supported".into()));
            }
        }
        "allowtcpforwarding" => {
            cfg.allow_tcp_forwarding = match val.to_ascii_lowercase().as_str() {
                "yes" | "all" => TcpFwd::All,
                "no" => TcpFwd::No,
                "local" => TcpFwd::Local,
                "remote" => TcpFwd::Remote,
                _ => return Err(err(format!("bad AllowTcpForwarding {val}"))),
            }
        }
        "forcecommand" => {
            cfg.force_command = if val.is_empty() { return Err(err("ForceCommand needs a command".into())) }
                else if val.eq_ignore_ascii_case("none") { None } else { Some(val.to_string()) }
        }
        "trustedusercakeys" => {
            cfg.trusted_user_ca_keys = if val.eq_ignore_ascii_case("none") { None } else { Some(val.to_string()) }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn parse_match(val: &str) -> Result<MatchBlock, String> {
    let t: Vec<&str> = val.split_whitespace().collect();
    if t.len() == 1 && t[0].eq_ignore_ascii_case("all") {
        return Ok(MatchBlock { all: true, ..Default::default() });
    }
    if t.is_empty() || t.len() % 2 != 0 {
        return Err(format!("bad Match {val:?}"));
    }
    let mut b = MatchBlock::default();
    for p in t.chunks(2) {
        let c = p[0].to_ascii_lowercase();
        if !matches!(c.as_str(), "user" | "group" | "address" | "localport") {
            return Err(format!("unsupported Match criterion {}", p[0]));
        }
        b.crit.push((c, p[1].to_string()));
    }
    Ok(b)
}

/// The OpenSSH sshd_config subset this server understands. Keywords are
/// case-insensitive; unknown keywords are reported and ignored.
fn parse_config(text: &str, cfg: &mut Config) -> Result<(), String> {
    let mut cur: Option<MatchBlock> = None;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let key = it.next().unwrap_or("").to_ascii_lowercase();
        let val = it.next().unwrap_or("").trim();
        let err = |m: String| format!("line {}: {m}", n + 1);
        if key == "match" {
            if let Some(b) = cur.take() {
                cfg.matches.push(b);
            }
            cur = Some(parse_match(val).map_err(err)?);
            continue;
        }
        if let Some(b) = cur.as_mut() {
            if MATCH_KEYWORDS.contains(&key.as_str()) {
                apply_kw(&mut cfg.clone(), &key, val).map_err(err)?;
                b.lines.push((key, val.to_string()));
            } else {
                log(&format!("config line {}: ignoring {key} inside Match (not a Match keyword here)", n + 1));
            }
            continue;
        }
        if !apply_kw(cfg, &key, val).map_err(err)? {
            log(&format!("config line {}: ignoring unsupported keyword {key}", n + 1));
        }
    }
    if let Some(b) = cur.take() {
        cfg.matches.push(b);
    }
    Ok(())
}

static UNAUTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One unauthenticated connection (MaxStartups); released on drop.
pub struct StartupSlot;

impl Drop for StartupSlot {
    fn drop(&mut self) {
        UNAUTH.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Admit a new connection under MaxStartups, or None to drop it.
fn admit(max: (usize, usize, usize)) -> Option<StartupSlot> {
    use std::sync::atomic::Ordering::AcqRel;
    let cur = UNAUTH.fetch_add(1, AcqRel); // unauthenticated connections before this one
    let slot = StartupSlot;
    let (start, rate, full) = max;
    if cur >= full {
        return None;
    }
    if cur >= start {
        let span = (full - start).max(1) as u64;
        let pct = rate as u64 + (100 - rate as u64) * (cur - start) as u64 / span;
        let mut b = [0u8; 8];
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
        if u64::from_le_bytes(b) % 100 < pct {
            return None;
        }
    }
    Some(slot)
}

/// Child side of the startup pipe (fd 4): dropping it (on authentication or
/// exit) tells the listener this connection no longer counts against MaxStartups.
pub struct ChildStartup(Option<std::os::fd::OwnedFd>);

impl Drop for ChildStartup {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        if let Some(fd) = self.0.take() {
            unsafe { libc::write(fd.as_raw_fd(), b"A".as_ptr().cast(), 1) };
        }
        set_comm(b"sshd-net\0");
    }
}

fn set_comm(name: &[u8]) {
    unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr() as libc::c_ulong, 0, 0, 0) };
}

static LOG: OnceLock<Mutex<Box<dyn Write + Send>>> = OnceLock::new();

pub fn log(msg: &str) {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let line = format!("[{}] sshd[{}]: {}\n", ts.tv_sec, std::process::id(), msg);
    match LOG.get() {
        Some(m) => {
            if let Ok(mut w) = m.lock() {
                let _ = w.write_all(line.as_bytes());
                let _ = w.flush();
            }
        }
        None => eprint!("{line}"),
    }
}

/// Load the ed25519 host key, generating and saving it on first boot.
fn load_or_create_host_key(path: &str) -> Result<PrivateKey, String> {
    if Path::new(path).exists() {
        return russh::keys::load_secret_key(path, None).map_err(|e| format!("{path}: {e}"));
    }
    let mut seed = [0u8; 32];
    let mut got = 0;
    while got < seed.len() {
        let r = unsafe { libc::getrandom(seed[got..].as_mut_ptr().cast(), seed.len() - got, 0) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("getrandom: {e}"));
        }
        got += r as usize;
    }
    let kp = Ed25519Keypair::from_seed(&seed);
    let key = PrivateKey::new(KeypairData::Ed25519(kp), "root@leandros")
        .map_err(|e| format!("host key: {e}"))?;
    if let Some(dir) = Path::new(path).parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    }
    let pem = key.to_openssh(LineEnding::LF).map_err(|e| format!("host key encode: {e}"))?;
    let tmp = format!("{path}.new");
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("{tmp}: {e}"))?;
        f.write_all(pem.as_bytes()).map_err(|e| format!("{tmp}: {e}"))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path).map_err(|e| format!("{path}: {e}"))?;
    let public = key.public_key().to_openssh().map_err(|e| format!("host key encode: {e}"))?;
    let pubpath = format!("{path}.pub");
    fs::write(&pubpath, format!("{public}\n")).map_err(|e| format!("{pubpath}: {e}"))?;
    let _ = fs::set_permissions(&pubpath, fs::Permissions::from_mode(0o644));
    log(&format!("generated a new ed25519 host key {path}"));
    Ok(key)
}

fn usage() -> ! {
    eprintln!("usage: sshd [-f config] [-e] [-t] [-p port]");
    std::process::exit(2);
}

/// Listen on `sa`. An IPv6 listener is V6ONLY, so `::` and `0.0.0.0` are two
/// separate listeners (as OpenSSH does) and neither collides with the other.
fn bind_listener(sa: SocketAddr) -> std::io::Result<TcpListener> {
    use std::os::fd::AsRawFd;
    let sock = if sa.is_ipv6() { tokio::net::TcpSocket::new_v6()? } else { tokio::net::TcpSocket::new_v4()? };
    if sa.is_ipv6() {
        let one: libc::c_int = 1;
        let r = unsafe {
            libc::setsockopt(sock.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_V6ONLY,
                &one as *const _ as *const libc::c_void, core::mem::size_of::<libc::c_int>() as libc::socklen_t)
        };
        if r != 0 { return Err(std::io::Error::last_os_error()); }
    }
    sock.set_reuseaddr(true)?;
    sock.bind(sa)?;
    sock.listen(1024)
}

fn main() {
    let mut config_path = "/etc/ssh/sshd_config".to_string();
    let mut to_stderr = false;
    let mut test_only = false;
    let mut port_override = None;
    let mut child_mode = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-f" => config_path = args.next().unwrap_or_else(|| usage()),
            "-e" => to_stderr = true,
            "-t" => test_only = true,
            "-R" => child_mode = true,
            "-p" => port_override = Some(args.next().and_then(|p| p.parse().ok()).unwrap_or_else(|| usage())),
            _ => usage(),
        }
    }

    if child_mode {
        set_comm(b"sshd-preauth\0");
    }
    if !to_stderr && !test_only {
        let _ = fs::create_dir_all("/var/log");
        match fs::OpenOptions::new().create(true).append(true).mode(0o600).open("/var/log/sshd.log") {
            Ok(f) => {
                let _ = LOG.set(Mutex::new(Box::new(f)));
            }
            Err(e) => eprintln!("sshd: /var/log/sshd.log: {e}; logging to stderr"),
        }
    }

    let mut cfg = Config::default();
    match fs::read_to_string(&config_path) {
        Ok(text) => {
            if let Err(e) = parse_config(&text, &mut cfg) {
                log(&format!("{config_path}: {e}"));
                eprintln!("sshd: {config_path}: {e}");
                std::process::exit(1);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log(&format!("{config_path} not found, using the built-in defaults"))
        }
        Err(e) => {
            eprintln!("sshd: {config_path}: {e}");
            std::process::exit(1);
        }
    }
    if let Some(p) = port_override {
        cfg.port = p;
    }
    if cfg.listen.is_empty() {
        cfg.listen = vec!["0.0.0.0".into(), "::".into()];
    }

    let host_key = match load_or_create_host_key(&cfg.host_key) {
        Ok(k) => k,
        Err(e) => {
            log(&format!("cannot load the host key: {e}"));
            eprintln!("sshd: cannot load the host key: {e}");
            std::process::exit(1);
        }
    };
    let fp = host_key.public_key().fingerprint(Default::default());
    if test_only {
        println!("sshd: configuration OK, host key {fp}");
        return;
    }

    // A dead client must not kill the daemon on a write.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    if child_mode {
        run_child(cfg, host_key);
        return;
    }
    let mut child_args: Vec<String> = vec!["-R".into(), "-f".into(), config_path.clone(), "-p".into(), cfg.port.to_string()];
    if to_stderr {
        child_args.push("-e".into());
    }
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| std::env::args().next().unwrap_or_else(|| "sshd".into()));
    let _ = CHILD_CMD.set((exe, child_args, to_stderr));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(serve(cfg, fp.to_string()));
}

static CHILD_CMD: OnceLock<(String, Vec<String>, bool)> = OnceLock::new();

/// `sshd -R`: serve the connection on fd 3 (startup pipe on fd 4), then exit.
fn run_child(cfg: Config, host_key: PrivateKey) {
    use std::os::fd::{FromRawFd, OwnedFd};
    // fds 3 and 4 came through exec without CLOEXEC; keep them from leaking
    // into the shells this process spawns.
    for fd in [3, 4] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(3) };
    let startup = ChildStartup(Some(unsafe { OwnedFd::from_raw_fd(4) }));
    let (Ok(peer), local) = (std_stream.peer_addr(), std_stream.local_addr().ok()) else {
        std::process::exit(1);
    };
    let _ = std_stream.set_nonblocking(true);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async move {
        let stream = match tokio::net::TcpStream::from_std(std_stream) {
            Ok(s) => s,
            Err(e) => {
                log(&format!("{peer}: {e}"));
                return;
            }
        };
        let rcfg = russh_config(&cfg, host_key);
        session::run_connection(rcfg, Arc::new(cfg), stream, peer, local, startup).await;
    });
}

fn russh_config(cfg: &Config, host_key: PrivateKey) -> Arc<russh::server::Config> {
    let mut methods = russh::MethodSet::empty();
    if cfg.any(|c| c.pubkey_auth) {
        methods.push(MethodKind::PublicKey);
    }
    if cfg.any(|c| c.password_auth) {
        methods.push(MethodKind::Password);
        methods.push(MethodKind::KeyboardInteractive);
    }
    Arc::new(russh::server::Config {
        server_id: russh::SshId::Standard("SSH-2.0-LeandrOS_sshd_russh-0.64".into()),
        methods,
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::from_secs(0)),
        // Privsep P1 hooks: kex signs through `host_key_signer` (which also
        // records the session id); `keys` only selects the algorithm/public half.
        host_key_signer: Some(userauth::host_key_signer(vec![host_key.clone()])),
        keys: vec![host_key],
        max_auth_attempts: cfg.max_auth_tries,
        // An idle interactive shell must stay up; dead peers are found by
        // keepalives instead (OpenSSH: ClientAliveInterval).
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(60)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    })
}

/// Hand one accepted connection to a fresh `sshd -R`, and watch it: its slot in
/// MaxStartups is held until it signals authentication (or exits), and a child
/// still unauthenticated well after LoginGraceTime is killed.
async fn spawn_child(stream: tokio::net::TcpStream, peer: SocketAddr, grace: u64, slot: StartupSlot) {
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::process::CommandExt;
    use tokio::io::AsyncReadExt;
    let Some((exe, args, to_stderr)) = CHILD_CMD.get() else { return };
    let Ok(std_stream) = stream.into_std() else { return };
    let _ = std_stream.set_nonblocking(false);
    let mut sv = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr()) } != 0 {
        log(&format!("{peer}: socketpair: {}", std::io::Error::last_os_error()));
        return;
    }
    let (mine, theirs) = (unsafe { std::os::fd::OwnedFd::from_raw_fd(sv[0]) }, unsafe { std::os::fd::OwnedFd::from_raw_fd(sv[1]) });
    // Sources above 4 so the dup2 into 3/4 cannot clobber one another.
    let sock_fd = unsafe { libc::fcntl(std_stream.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    let pipe_fd = unsafe { libc::fcntl(theirs.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if sock_fd < 0 || pipe_fd < 0 {
        log(&format!("{peer}: F_DUPFD_CLOEXEC failed"));
        return;
    }
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).stdin(std::process::Stdio::null());
    if !*to_stderr {
        cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    }
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(sock_fd, 3) < 0 || libc::dup2(pipe_fd, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let spawned = tokio::process::Command::from(cmd).spawn();
    unsafe {
        libc::close(sock_fd);
        libc::close(pipe_fd);
    }
    drop(std_stream);
    drop(theirs);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            log(&format!("{peer}: cannot start {exe}: {e}"));
            return;
        }
    };
    let Ok(std_mine) = (|| -> std::io::Result<std::os::unix::net::UnixStream> {
        let s = unsafe { std::os::unix::net::UnixStream::from_raw_fd(mine.into_raw_fd()) };
        s.set_nonblocking(true)?;
        Ok(s)
    })() else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return;
    };
    let Ok(mut pipe) = tokio::net::UnixStream::from_std(std_mine) else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return;
    };
    let mut slot = Some(slot);
    let backstop = tokio::time::sleep(Duration::from_secs(if grace == 0 { 86400 * 365 } else { grace + 2 }));
    tokio::pin!(backstop);
    let mut buf = [0u8; 8];
    loop {
        tokio::select! {
            _ = pipe.read(&mut buf), if slot.is_some() => { slot = None; }
            _ = &mut backstop, if slot.is_some() => {
                log(&format!("{peer}: still unauthenticated after LoginGraceTime, killing the connection process"));
                let _ = child.start_kill();
                slot = None;
            }
            st = child.wait() => {
                if let Ok(st) = st {
                    if !st.success() {
                        log(&format!("{peer}: connection process ended abnormally: {st}"));
                    }
                }
                break;
            }
        }
    }
}

async fn serve(cfg: Config, fp: String) {
    let cfg = Arc::new(cfg);

    let mut listeners = Vec::new();
    for addr in &cfg.listen {
        let sa: SocketAddr = if addr.contains(':') && !addr.starts_with('[') {
            match format!("[{addr}]:{}", cfg.port).parse() {
                Ok(s) => s,
                Err(_) => {
                    log(&format!("bad ListenAddress {addr}"));
                    continue;
                }
            }
        } else {
            match format!("{addr}:{}", cfg.port).parse() {
                Ok(s) => s,
                Err(_) => {
                    log(&format!("bad ListenAddress {addr}"));
                    continue;
                }
            }
        };
        match bind_listener(sa) {
            Ok(l) => {
                log(&format!("listening on {sa}"));
                listeners.push(l);
            }
            Err(e) => log(&format!("cannot listen on {sa}: {e}")),
        }
    }
    if listeners.is_empty() {
        log("no listening socket, exiting");
        std::process::exit(1);
    }
    log(&format!("ready, host key {fp}"));

    let mut tasks = Vec::new();
    for l in listeners {
        unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&l), libc::F_SETFD, libc::FD_CLOEXEC) };
        let cfg = cfg.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                match l.accept().await {
                    Ok((stream, peer)) => {
                        let Some(slot) = admit(cfg.max_startups) else {
                            log(&format!("{peer}: dropping connection (MaxStartups)"));
                            continue;
                        };
                        // Not every accept path sets close-on-exec: a connection fd leaked into
                        // another connection's process would keep this one open after a crash.
                        unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&stream), libc::F_SETFD, libc::FD_CLOEXEC) };
                        let grace = cfg.login_grace_secs;
                        tokio::spawn(spawn_child(stream, peer, grace, slot));
                    }
                    Err(e) => {
                        log(&format!("accept: {e}"));
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
}
