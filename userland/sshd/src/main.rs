//! sshd -- the LeandrOS SSH server, built on russh.
//!
//! One root process runs the SSH protocol for every connection (tokio); each
//! shell, command or subsystem is a child that drops to the authenticated
//! user (setgroups, setresgid, setresuid, read back) before it execs. See
//! artifacts/notes/lane-gitssh-2026-10-14.md and docs/userland/programs.md.
//!
//! Usage: sshd [-f config] [-e] [-t] [-p port]
//!   -f  configuration file (default /etc/ssh/sshd_config)
//!   -e  log to stderr instead of /var/log/sshd.log
//!   -t  check the configuration and the host key, then exit
//!   -p  override Port

mod accounts;
mod session;

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
    pub pubkey_auth: bool,
    pub authorized_keys_file: String,
    pub sftp_server: Option<String>,
    pub max_auth_tries: usize,
    pub login_grace_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 22,
            listen: Vec::new(),
            host_key: "/etc/ssh/ssh_host_ed25519_key".into(),
            permit_root_login: RootLogin::ProhibitPassword,
            password_auth: true,
            pubkey_auth: true,
            authorized_keys_file: ".ssh/authorized_keys".into(),
            sftp_server: Some("/usr/lib/ssh/sftp-server".into()),
            max_auth_tries: 6,
            login_grace_secs: 120,
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

/// The OpenSSH sshd_config subset this server understands. Keywords are
/// case-insensitive; unknown keywords are reported and ignored.
fn parse_config(text: &str, cfg: &mut Config) -> Result<(), String> {
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let key = it.next().unwrap_or("").to_ascii_lowercase();
        let val = it.next().unwrap_or("").trim();
        let err = |m: String| format!("line {}: {m}", n + 1);
        match key.as_str() {
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
            "maxauthtries" => {
                cfg.max_auth_tries = val.parse().map_err(|_| err(format!("bad MaxAuthTries {val}")))?
            }
            "logingracetime" => {
                cfg.login_grace_secs =
                    val.trim_end_matches('s').parse().map_err(|_| err(format!("bad LoginGraceTime {val}")))?
            }
            "permitemptypasswords" => {
                if yes_no(val, &key).map_err(err)? {
                    return Err(err("PermitEmptyPasswords yes is not supported".into()));
                }
            }
            _ => log(&format!("config line {}: ignoring unsupported keyword {key}", n + 1)),
        }
    }
    Ok(())
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

fn main() {
    let mut config_path = "/etc/ssh/sshd_config".to_string();
    let mut to_stderr = false;
    let mut test_only = false;
    let mut port_override = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-f" => config_path = args.next().unwrap_or_else(|| usage()),
            "-e" => to_stderr = true,
            "-t" => test_only = true,
            "-p" => port_override = Some(args.next().and_then(|p| p.parse().ok()).unwrap_or_else(|| usage())),
            _ => usage(),
        }
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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(serve(cfg, host_key, fp.to_string()));
}

async fn serve(cfg: Config, host_key: PrivateKey, fp: String) {
    let mut methods = russh::MethodSet::empty();
    if cfg.pubkey_auth {
        methods.push(MethodKind::PublicKey);
    }
    if cfg.password_auth {
        methods.push(MethodKind::Password);
    }
    let rcfg = Arc::new(russh::server::Config {
        server_id: russh::SshId::Standard("SSH-2.0-LeandrOS_sshd_russh-0.64".into()),
        methods,
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::from_secs(0)),
        keys: vec![host_key],
        max_auth_attempts: cfg.max_auth_tries,
        // An idle interactive shell must stay up; dead peers are found by
        // keepalives instead (OpenSSH: ClientAliveInterval).
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(60)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
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
        match TcpListener::bind(sa).await {
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
        let rcfg = rcfg.clone();
        let cfg = cfg.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                match l.accept().await {
                    Ok((stream, peer)) => {
                        let local = stream.local_addr().ok();
                        let rcfg = rcfg.clone();
                        let cfg = cfg.clone();
                        tokio::spawn(async move {
                            session::run_connection(rcfg, cfg, stream, peer, local).await;
                        });
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
