//! Backend driver: runs `leandros-install` (directly as root, else via
//! `sudo -S -k -p ''`) and parses its JSON-lines output. No disk I/O here.
use futures::channel::mpsc::UnboundedSender;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

pub const DEFAULT_BIN: &str = "/usr/sbin/leandros-install";

#[derive(Clone, Debug, Default)]
pub struct Disk {
    pub dev: String,
    pub model: String,
    pub bus: String,
    pub size: u64,
    pub eligible: bool,
    pub reason: String,
}

impl Disk {
    pub fn size_human(&self) -> String {
        let g = self.size as f64 / (1u64 << 30) as f64;
        if g >= 1024.0 { format!("{:.1} TiB", g / 1024.0) } else { format!("{:.1} GiB", g) }
    }
    pub fn label(&self) -> String {
        let m = if self.model.is_empty() { self.bus.as_str() } else { self.model.as_str() };
        if m.is_empty() { self.dev.clone() } else { format!("{} ({})", m, self.dev) }
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Disk(Disk),
    Step { name: String, pct: u32 },
    Done { root_partuuid: String, disk_guid: String },
    Error(String),
    /// Process finished; Some(code) of the exit status.
    Exit(Option<i32>, String),
}

pub fn bin() -> String {
    std::env::var("LEANDROS_INSTALL_BIN").unwrap_or_else(|_| DEFAULT_BIN.to_string())
}

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Tolerant parse of one JSON line; unknown fields and unparsable lines are ignored.
pub fn parse_line(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let s = |keys: &[&str]| -> String {
        keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_str())).unwrap_or("").to_string()
    };
    match v.get("ev")?.as_str()? {
        "disk" => {
            let size = ["size", "size_bytes", "bytes"]
                .iter()
                .find_map(|k| v.get(*k).and_then(|x| x.as_u64()))
                .unwrap_or(0);
            let eligible = v.get("eligible").and_then(|x| x.as_bool()).unwrap_or(true);
            Some(Event::Disk(Disk {
                dev: s(&["dev", "path", "name", "device"]),
                model: s(&["model"]),
                bus: s(&["bus"]),
                size,
                eligible,
                reason: s(&["reason"]),
            }))
        }
        "step" => Some(Event::Step {
            name: s(&["name"]),
            pct: v.get("pct").and_then(|x| x.as_u64()).unwrap_or(0).min(100) as u32,
        }),
        "done" => Some(Event::Done { root_partuuid: s(&["root_partuuid"]), disk_guid: s(&["disk_guid"]) }),
        "error" => Some(Event::Error(s(&["msg", "message"]))),
        _ => None,
    }
}

/// Spawn the backend on a thread; events go to `tx`, ending with `Event::Exit`.
pub fn run(args: Vec<String>, password: Option<String>, tx: UnboundedSender<Event>) {
    std::thread::spawn(move || {
        let direct = is_root() || std::env::var_os("LEANDROS_INSTALL_NOSUDO").is_some();
        let mut cmd;
        if direct {
            cmd = Command::new(bin());
        } else {
            cmd = Command::new("sudo");
            cmd.args(["-S", "-k", "-p", ""]).arg(bin());
        }
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.unbounded_send(Event::Exit(None, format!("cannot start backend: {e}")));
                return;
            }
        };
        if let Some(mut si) = child.stdin.take() {
            if !direct {
                let _ = writeln!(si, "{}", password.unwrap_or_default());
            }
            drop(si);
        }
        let stderr = child.stderr.take();
        let err_thread = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(e) = stderr {
                for l in BufReader::new(e).lines().map_while(Result::ok) {
                    s.push_str(&l);
                    s.push('\n');
                }
            }
            s
        });
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(ev) = parse_line(&line) {
                    let _ = tx.unbounded_send(ev);
                }
            }
        }
        let code = child.wait().ok().and_then(|s| s.code());
        let err = err_thread.join().unwrap_or_default();
        let _ = tx.unbounded_send(Event::Exit(code, err));
    });
}

pub fn list_args() -> Vec<String> {
    vec!["list".into(), "--json".into()]
}

pub fn install_args(dev: &str) -> Vec<String> {
    vec!["install".into(), "--target".into(), dev.into(), "--yes".into(), "--json".into()]
}

/// True when stderr from sudo indicates a bad password.
pub fn wrong_password(stderr: &str) -> bool {
    let l = stderr.to_lowercase();
    l.contains("incorrect password") || l.contains("authentication failed") || l.contains("sorry, try again")
}
