//! The unprivileged network half of a connection (privsep P3/P4): uid 22,
//! chrooted to /var/empty, no-new-privs, seccomp'd. It speaks SSH with russh
//! but holds no secret: signatures, authentication decisions, sessions and
//! forwards all come from the root monitor over two SEQPACKET sockets.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::ssh_key::private::{Ed25519Keypair, Ed25519PrivateKey, KeypairData};
use russh::keys::ssh_key::{Algorithm, HashAlg};
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId, Pty};
use tokio::io::unix::AsyncFd;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use crate::rpc::{self, Frame, R, W};
use crate::session::{sig_from_number, sig_number, write_all_fd, DupFd, Input, PtyReq};
use crate::{log, Config};

struct Raw(RawFd);
impl AsRawFd for Raw {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

struct ExitInfo {
    code: u32,
    sig: u32,
    core: bool,
}

struct Ctl {
    fd: RawFd,
    afd: AsyncFd<Raw>,
    next: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<(Frame, Vec<i32>)>>>,
    exits: Mutex<HashMap<u32, oneshot::Sender<ExitInfo>>>,
}

fn die(code: i32) -> ! {
    unsafe { libc::_exit(code) }
}

impl Ctl {
    async fn reader(self: Arc<Self>) {
        loop {
            let mut g = match self.afd.readable().await {
                Ok(g) => g,
                Err(_) => die(1),
            };
            loop {
                match rpc::recv(self.fd) {
                    Ok(Some((Ok(f), fds))) => self.dispatch(f, fds),
                    Ok(Some((Err(_), _))) => die(1),
                    Ok(None) => die(0),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        g.clear_ready();
                        break;
                    }
                    Err(_) => die(1),
                }
            }
        }
    }

    fn dispatch(&self, f: Frame, fds: Vec<i32>) {
        if f.req == 0 {
            match f.ty {
                rpc::SESSION_EXIT => {
                    let mut r = R(&f.body);
                    if let (Ok(c), Ok(code), Ok(sig), Ok(core)) = (r.u32(), r.u32(), r.u32(), r.u8()) {
                        if let Some(tx) = self.exits.lock().unwrap().remove(&c) {
                            let _ = tx.send(ExitInfo { code, sig, core: core != 0 });
                        }
                    }
                }
                rpc::SHUTDOWN => die(0),
                _ => {}
            }
            return;
        }
        match self.pending.lock().unwrap().remove(&f.req) {
            Some(tx) => {
                let _ = tx.send((f, fds));
            }
            None => {
                for fd in fds {
                    unsafe { libc::close(fd) };
                }
            }
        }
    }

    async fn call(&self, ty: u8, body: &[u8]) -> Result<(Frame, Vec<i32>), String> {
        let req = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req, tx);
        if let Err(e) = rpc::send(self.fd, ty, req, body, &[]) {
            self.pending.lock().unwrap().remove(&req);
            return Err(format!("monitor: {e}"));
        }
        match tokio::time::timeout(Duration::from_secs(120), rx).await {
            Ok(Ok(v)) => Ok(v),
            _ => Err("the monitor did not answer".into()),
        }
    }

    fn notify(&self, ty: u8, body: &[u8]) {
        let _ = rpc::send(self.fd, ty, 0, body, &[]);
    }
}

fn set_nonblock(fd: RawFd) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
}

fn err_text(f: &Frame) -> String {
    R(&f.body).str().map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_else(|_| "refused".into())
}

fn hash_code(h: Option<HashAlg>) -> u8 {
    match h {
        Some(HashAlg::Sha256) => 1,
        Some(HashAlg::Sha512) => 2,
        _ => 0,
    }
}

/// A key with the real public half and an all-zero private half: russh needs
/// a `PrivateKey` to announce the host key, but every signature is made by the
/// monitor (`host_key_signer`).
fn stand_in(public: &PublicKey) -> Result<PrivateKey, String> {
    let Some(ed) = public.key_data().ed25519() else {
        return Err("only ed25519 host keys are supported with privilege separation".into());
    };
    let kp = Ed25519Keypair { public: *ed, private: Ed25519PrivateKey::from_bytes(&[0u8; 32]) };
    PrivateKey::new(KeypairData::Ed25519(kp), "").map_err(|e| e.to_string())
}

fn signer(sign: RawFd, blob: Vec<u8>) -> russh::server::HostKeySigner {
    let lock = Mutex::new(());
    Arc::new(move |alg: &Algorithm, hash: Option<HashAlg>, h: &[u8]| {
        let bad = || russh::keys::ssh_key::Error::Encoding(ssh_encoding::Error::Length);
        let _g = lock.lock().unwrap();
        let mut w = W::default();
        w.str(alg.as_str().as_bytes()).u8(hash_code(hash)).str(&blob).str(h);
        rpc::send(sign, rpc::SIGN, 1, &w.0, &[]).map_err(|_| bad())?;
        let mut p = libc::pollfd { fd: sign, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, 30_000) } <= 0 {
            return Err(bad());
        }
        match rpc::recv(sign) {
            Ok(Some((Ok(f), fds))) if f.ty == rpc::SIG && fds.is_empty() => Ok(f.body),
            _ => Err(bad()),
        }
    })
}

#[allow(clippy::too_many_arguments)]
pub fn run(cfg: Config, public: PublicKey, tcp: RawFd, sign: RawFd, ctl: RawFd, logw: RawFd, peer: SocketAddr, _local: Option<SocketAddr>) -> ! {
    crate::CHILD_LOG_FD.store(logw, Ordering::Relaxed);
    set_nonblock(logw);
    unsafe { libc::prctl(libc::PR_SET_NAME, c"sshd-preauth".as_ptr() as libc::c_ulong, 0, 0, 0) };
    let hook = std::env::var("SSHD_TEST_HOOK").ok();
    let seccomp_log = std::env::var("SSHD_SECCOMP").map(|v| v == "log").unwrap_or(false);
    let standin = match stand_in(&public) {
        Ok(k) => k,
        Err(e) => {
            log(&format!("{peer}: {e}"));
            die(1)
        }
    };
    set_nonblock(tcp);
    set_nonblock(ctl);
    if let Err(e) = crate::sandbox::confine(22, 22, "/var/empty", &[tcp, sign, ctl, logw]) {
        log(&format!("{peer}: cannot confine the network process: {e}"));
        die(1);
    }
    if let Err(e) = crate::sandbox::install_seccomp(seccomp_log) {
        log(&format!("{peer}: cannot install the seccomp filter: {e}"));
        die(1);
    }
    if hook.as_deref() == Some("badsyscall") {
        unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    }
    let blob = public.to_bytes().unwrap_or_default();
    let rt = tokio::runtime::Builder::new_current_thread().enable_io().enable_time().build().unwrap_or_else(|_| die(1));
    rt.block_on(async move {
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(tcp) };
        let Ok(stream) = TcpStream::from_std(std_stream) else { die(1) };
        let Ok(afd) = AsyncFd::new(Raw(ctl)) else { die(1) };
        let ctl = Arc::new(Ctl {
            fd: ctl,
            afd,
            next: AtomicU32::new(0),
            pending: Mutex::new(HashMap::new()),
            exits: Mutex::new(HashMap::new()),
        });
        tokio::spawn(ctl.clone().reader());
        let mut rcfg = crate::russh_config(&cfg, standin);
        if let Some(c) = Arc::get_mut(&mut rcfg) {
            c.host_key_signer = Some(signer(sign, blob));
        }
        let authed = Arc::new(AtomicBool::new(false));
        let net = Net {
            ctl,
            cfg: Arc::new(cfg),
            peer,
            authed,
            no_pty: false,
            chans: HashMap::new(),
            next_chan: 0,
            fwds: HashMap::new(),
            hook,
        };
        match russh::server::run_stream(rcfg, stream, net).await {
            Ok(running) => match running.await {
                Ok(()) => log(&format!("{peer}: connection ended")),
                Err(e) => log(&format!("{peer}: connection ended: {e:?}")),
            },
            Err(e) => {
                log(&format!("{peer}: handshake failed: {e:?}"));
                die(1)
            }
        }
    });
    die(0)
}

#[derive(Default)]
struct NChan {
    no: u32,
    pty: Option<PtyReq>,
    env: Vec<(String, String)>,
    input: Option<mpsc::UnboundedSender<Input>>,
    master: Option<Arc<OwnedFd>>,
    started: bool,
}

struct Net {
    ctl: Arc<Ctl>,
    cfg: Arc<Config>,
    peer: SocketAddr,
    authed: Arc<AtomicBool>,
    no_pty: bool,
    chans: HashMap<ChannelId, NChan>,
    next_chan: u32,
    fwds: HashMap<(String, u32), tokio::task::AbortHandle>,
    hook: Option<String>,
}

impl Drop for Net {
    fn drop(&mut self) {
        for (_, h) in self.fwds.drain() {
            h.abort();
        }
    }
}

async fn pump(afd: &AsyncFd<DupFd>, h: &russh::server::Handle, id: ChannelId, ext: bool) {
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
                let sent = if ext { h.extended_data(id, 1, buf[..n].to_vec()).await.is_ok() } else { h.data(id, buf[..n].to_vec()).await.is_ok() };
                if !sent {
                    return;
                }
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(Err(_)) => return,
            Err(_would_block) => {}
        }
    }
}

async fn finish_info(handle: &russh::server::Handle, id: ChannelId, e: ExitInfo) {
    if e.code != u32::MAX {
        let _ = handle.exit_status_request(id, e.code).await;
    } else if e.sig != 0 {
        let _ = handle.exit_signal_request(id, sig_from_number(e.sig as i32), e.core, String::new(), String::new()).await;
    }
    let _ = handle.eof(id).await;
    let _ = handle.close(id).await;
}

fn reg(fd: i32) -> Result<Arc<AsyncFd<DupFd>>, String> {
    let o = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
    AsyncFd::new(DupFd(o)).map(Arc::new).map_err(|e| format!("epoll: {e}"))
}

impl Net {
    fn accept(&mut self, body: &[u8]) -> Auth {
        self.no_pty = R(body).u8().map(|b| b != 0).unwrap_or(false);
        self.authed.store(true, Ordering::Release);
        unsafe { libc::prctl(libc::PR_SET_NAME, c"sshd-net".as_ptr() as libc::c_ulong, 0, 0, 0) };
        Auth::Accept
    }

    async fn password(&mut self, user: &str, pw: &str, method: &str) -> Auth {
        let mut w = W::default();
        w.str(user.as_bytes()).str(pw.as_bytes()).str(method.as_bytes());
        match self.ctl.call(rpc::AUTH_PASSWORD, &w.0).await {
            Ok((f, _)) if f.ty == rpc::AUTH_OK => self.accept(&f.body),
            _ => Auth::reject(),
        }
    }

    async fn start(&mut self, id: ChannelId, kind: u8, arg: String, session: &mut Session) -> Result<(), String> {
        if !self.authed.load(Ordering::Acquire) {
            return Err("not authenticated".into());
        }
        let handle = session.handle();
        let (no, pty, env) = {
            let ch = self.chans.get(&id).ok_or("no such channel")?;
            if ch.started {
                return Err("channel already has a program".into());
            }
            (ch.no, ch.pty.clone(), ch.env.clone())
        };
        let mut w = W::default();
        w.u32(no).u8(kind).str(arg.as_bytes()).u8(pty.is_some() as u8);
        if let Some(p) = &pty {
            w.str(p.term.as_bytes()).u32(p.cols as u32).u32(p.rows as u32).u32(p.px as u32).u32(p.py as u32);
        }
        w.u32(env.len() as u32);
        for (k, v) in &env {
            w.str(k.as_bytes()).str(v.as_bytes());
        }
        let (exit_tx, exit_rx) = oneshot::channel();
        self.ctl.exits.lock().unwrap().insert(no, exit_tx);
        let res = self.ctl.call(rpc::SESSION_OPEN, &w.0).await;
        let (f, fds) = match res {
            Ok(v) => v,
            Err(e) => {
                self.ctl.exits.lock().unwrap().remove(&no);
                return Err(e);
            }
        };
        if f.ty != rpc::SESSION_FDS {
            self.ctl.exits.lock().unwrap().remove(&no);
            for fd in fds {
                unsafe { libc::close(fd) };
            }
            return Err(if f.ty == rpc::ERR { err_text(&f) } else { "unexpected reply".into() });
        }
        let ch = self.chans.get_mut(&id).ok_or("no such channel")?;
        ch.started = true;
        let (tx, mut rx) = mpsc::unbounded_channel::<Input>();
        ch.input = Some(tx);
        if fds.len() == 1 {
            let afd = reg(fds[0])?;
            ch.master = Some(afd.get_ref().0.clone());
            let wfd = afd.clone();
            tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    if let Input::Data(d) = m {
                        if write_all_fd(&wfd, &d).await.is_err() {
                            break;
                        }
                    }
                }
            });
            let (rfd, h) = (afd.clone(), handle.clone());
            let mut reader = tokio::spawn(async move { pump(&rfd, &h, id, false).await });
            tokio::spawn(async move {
                let info = exit_rx.await;
                if tokio::time::timeout(Duration::from_millis(500), &mut reader).await.is_err() {
                    reader.abort();
                }
                if let Ok(i) = info {
                    finish_info(&handle, id, i).await;
                }
            });
        } else if fds.len() == 3 {
            let (stdin, stdout, stderr) = (reg(fds[0])?, reg(fds[1])?, reg(fds[2])?);
            tokio::spawn(async move {
                let mut stdin = Some(stdin);
                while let Some(m) = rx.recv().await {
                    match m {
                        Input::Data(d) => {
                            let Some(w) = stdin.as_ref() else { continue };
                            if write_all_fd(w, &d).await.is_err() {
                                stdin = None;
                            }
                        }
                        Input::Eof => stdin = None,
                    }
                }
            });
            let (h1, h2) = (handle.clone(), handle.clone());
            let out = tokio::spawn(async move { pump(&stdout, &h1, id, false).await });
            let err = tokio::spawn(async move { pump(&stderr, &h2, id, true).await });
            tokio::spawn(async move {
                let info = exit_rx.await;
                let both = async {
                    let _ = out.await;
                    let _ = err.await;
                };
                let _ = tokio::time::timeout(Duration::from_secs(5), both).await;
                if let Ok(i) = info {
                    finish_info(&handle, id, i).await;
                }
            });
        } else {
            for fd in fds {
                unsafe { libc::close(fd) };
            }
            return Err("bad descriptor set".into());
        }
        Ok(())
    }

    async fn reply_start(&mut self, id: ChannelId, kind: u8, arg: String, session: &mut Session) -> Result<(), russh::Error> {
        match self.start(id, kind, arg, session).await {
            Ok(()) => session.channel_success(id)?,
            Err(e) => {
                log(&format!("{}: cannot start the session: {e}", self.peer));
                session.channel_failure(id)?;
            }
        }
        Ok(())
    }
}

impl russh::server::Handler for Net {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(self.password(user, password, "password").await)
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        match response {
            None => Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: std::borrow::Cow::Owned(vec![("Password: ".into(), false)]),
            }),
            Some(mut r) => {
                let pw = r.next().map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                Ok(self.password(user, &pw, "keyboard-interactive").await)
            }
        }
    }

    async fn auth_publickey_offered(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let mut w = W::default();
        w.str(user.as_bytes()).u8(0).str(&key.to_bytes().unwrap_or_default());
        Ok(match self.ctl.call(rpc::AUTH_PUBKEY_QUERY, &w.0).await {
            Ok((f, _)) if f.ty == rpc::AUTH_OK => Auth::Accept,
            _ => Auth::reject(),
        })
    }

    async fn auth_publickey_signed(
        &mut self,
        user: &str,
        key: &russh::PublicKeyOrCertificate,
        signed_data: &[u8],
        signature: &[u8],
        _session_id: &[u8],
    ) -> Result<Auth, Self::Error> {
        let (kind, blob, hash) = match key {
            russh::PublicKeyOrCertificate::PublicKey { key, hash_alg } => (0u8, key.to_bytes().unwrap_or_default(), *hash_alg),
            russh::PublicKeyOrCertificate::Certificate(c) => (1u8, c.to_bytes().unwrap_or_default(), None),
        };
        let mut sig = signature.to_vec();
        let forge = self.hook.as_deref() == Some("forgeauth");
        if forge {
            for b in sig.iter_mut().rev().take(8) {
                *b ^= 0x55;
            }
        }
        let mut w = W::default();
        w.str(user.as_bytes()).u8(kind).str(&blob).u8(hash_code(hash)).str(signed_data).str(&sig);
        let r = self.ctl.call(rpc::AUTH_PUBKEY, &w.0).await;
        if forge {
            // A compromised child asking for a session as if it had authenticated.
            self.ctl.notify(rpc::SESSION_OPEN, &[]);
            return Ok(Auth::reject());
        }
        Ok(match r {
            Ok((f, _)) if f.ty == rpc::AUTH_OK => self.accept(&f.body),
            _ => Auth::reject(),
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.authed.load(Ordering::Acquire) {
            return Ok(());
        }
        self.next_chan += 1;
        self.chans.insert(channel.id(), NChan { no: self.next_chan, ..Default::default() });
        reply.accept().await;
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
        if !self.authed.load(Ordering::Acquire) {
            return Ok(());
        }
        let (ctl, peer) = (self.ctl.clone(), self.peer);
        let (host, orig_addr) = (host.to_string(), orig_addr.to_string());
        tokio::spawn(async move {
            let mut w = W::default();
            w.str(host.as_bytes()).u32(port);
            let Ok((f, fds)) = ctl.call(rpc::DIRECT_TCPIP, &w.0).await else { return };
            if f.ty != rpc::FWD_FD || fds.len() != 1 {
                for fd in fds {
                    unsafe { libc::close(fd) };
                }
                log(&format!("{peer}: direct-tcpip to {host}:{port} failed: {}", if f.ty == rpc::ERR { err_text(&f) } else { "unexpected reply".into() }));
                return; // dropping `reply` rejects
            }
            let std_s = unsafe { std::net::TcpStream::from_raw_fd(fds[0]) };
            let _ = std_s.set_nonblocking(true);
            let Ok(mut sock) = TcpStream::from_std(std_s) else { return };
            log(&format!("{peer}: direct-tcpip {orig_addr}:{orig_port} -> {host}:{port}"));
            reply.accept().await;
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
        });
        Ok(())
    }

    async fn tcpip_forward(&mut self, address: &str, port: &mut u32, session: &mut Session) -> Result<bool, Self::Error> {
        if !self.authed.load(Ordering::Acquire) {
            return Ok(false);
        }
        let mut w = W::default();
        w.str(address.as_bytes()).u32(*port);
        let Ok((f, fds)) = self.ctl.call(rpc::TCPIP_FORWARD, &w.0).await else { return Ok(false) };
        if f.ty != rpc::FWD_LISTEN || fds.len() != 1 {
            for fd in fds {
                unsafe { libc::close(fd) };
            }
            return Ok(false);
        }
        let bound = R(&f.body).u32().unwrap_or(*port);
        let std_l = unsafe { std::net::TcpListener::from_raw_fd(fds[0]) };
        let _ = std_l.set_nonblocking(true);
        let Ok(listener) = tokio::net::TcpListener::from_std(std_l) else { return Ok(false) };
        let key = (address.to_string(), bound);
        if self.fwds.contains_key(&key) {
            return Ok(false);
        }
        *port = bound;
        log(&format!("{}: tcpip-forward listening on 127.0.0.1:{bound} for {address}", self.peer));
        let handle = session.handle();
        let addr = address.to_string();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, from)) = listener.accept().await else { return };
                let (handle, addr) = (handle.clone(), addr.clone());
                tokio::spawn(async move {
                    let ch = match handle.channel_open_forwarded_tcpip(addr, bound, from.ip().to_string(), from.port() as u32).await {
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
            if ch.started {
                let mut w = W::default();
                w.u32(ch.no);
                self.ctl.notify(rpc::SESSION_HANGUP, &w.0);
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
        let no_pty = self.no_pty;
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

    async fn env_request(&mut self, id: ChannelId, name: &str, value: &str, session: &mut Session) -> Result<(), Self::Error> {
        match self.chans.get_mut(&id) {
            Some(ch) if crate::session::env_allowed(name) && !ch.started => {
                ch.env.push((name.to_string(), value.to_string()));
                session.channel_success(id)?;
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }

    async fn shell_request(&mut self, id: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        self.reply_start(id, 0, String::new(), session).await
    }

    async fn exec_request(&mut self, id: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).into_owned();
        self.reply_start(id, 1, cmd, session).await
    }

    async fn subsystem_request(&mut self, id: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        self.reply_start(id, 2, name.to_string(), session).await
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
                    crate::session::set_winsize(m.as_raw_fd(), p);
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

    async fn signal(&mut self, id: ChannelId, signal: russh::Sig, _session: &mut Session) -> Result<(), Self::Error> {
        if let (Some(ch), Some(n)) = (self.chans.get(&id), sig_number(&signal)) {
            if ch.started {
                let mut w = W::default();
                w.u32(ch.no).u32(n as u32);
                self.ctl.notify(rpc::SESSION_SIGNAL, &w.0);
            }
        }
        Ok(())
    }
}
