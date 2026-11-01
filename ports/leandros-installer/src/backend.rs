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
    /// Largest unallocated range in bytes (0 when the backend reports none).
    pub gap_bytes: u64,
    pub dualboot: bool,
    pub dualboot_reason: String,
    /// An existing LeandrOS dual-boot set can be reinstalled in place.
    pub reinstall: bool,
    /// Shrinkable Linux partition (optional `shrink` object of `list --json`).
    pub shrink_part: String,
    pub shrink_cur: u64,
    pub shrink_min: u64,
    /// Session B state from `list --json`: "", "boot_linux" or "ready_b".
    pub finish: String,
}

#[derive(Clone, Debug, Default)]
pub struct PartRow {
    pub n: u64,
    pub first: u64,
    pub last: u64,
    pub name: String,
    pub ptype: String,
}

impl PartRow {
    pub fn mib(&self) -> u64 {
        (self.last + 1 - self.first) / 2048
    }
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub old_gpt: Vec<PartRow>,
    pub new_gpt: Vec<PartRow>,
    pub esp_files: Vec<(String, u64)>,
    pub backup_dir: String,
    /// Reinstall plan: one line per partition and its action.
    pub actions: Vec<String>,
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
    Gate { id: String, name: String, status: String, msg: String },
    Plan(Plan),
    Info(String),
    Done { root_partuuid: String, disk_guid: String, dry_run: bool },
    ShrinkStatus { state: String, phase: String },
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
            let gap_bytes = v
                .get("gaps")
                .and_then(|g| g.as_array())
                .map(|a| a.iter().filter_map(|g| g.get("sectors").and_then(|x| x.as_u64())).max().unwrap_or(0) * 512)
                .unwrap_or(0);
            let sh = v.get("shrink");
            let shu = |k: &str| sh.and_then(|o| o.get(k)).and_then(|x| x.as_u64()).unwrap_or(0);
            Some(Event::Disk(Disk {
                gap_bytes,
                dualboot: v.get("dualboot").and_then(|x| x.as_bool()).unwrap_or(false),
                dualboot_reason: s(&["dualboot_reason"]),
                reinstall: v.get("reinstall").and_then(|x| x.as_bool()).unwrap_or(false),
                shrink_part: sh.and_then(|o| o.get("part")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
                shrink_cur: shu("cur_bytes"),
                shrink_min: shu("min_bytes"),
                finish: s(&["finish"]),
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
        "gate" => Some(Event::Gate { id: s(&["id"]), name: s(&["name"]), status: s(&["status"]), msg: s(&["msg"]) }),
        "info" => Some(Event::Info(s(&["msg"]))),
        "shrink_status" => Some(Event::ShrinkStatus { state: s(&["state"]), phase: s(&["phase"]) }),
        "plan" if v.get("new_gpt").is_some() || v.get("partitions").is_some() => {
            let rows = |k: &str| -> Vec<PartRow> {
                v.get(k)
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .map(|e| PartRow {
                                n: e.get("n").and_then(|x| x.as_u64()).unwrap_or(0),
                                first: e.get("first").and_then(|x| x.as_u64()).unwrap_or(0),
                                last: e.get("last").and_then(|x| x.as_u64()).unwrap_or(0),
                                name: e.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                ptype: e.get("type").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let esp_files = v
                .get("esp_files")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .map(|e| {
                            (
                                e.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                e.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let actions = v
                .get("partitions")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .map(|e| {
                            let g = |k: &str| e.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                            format!("p{}  {}: {}", e.get("n").and_then(|x| x.as_u64()).unwrap_or(0), g("role"), g("action"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Event::Plan(Plan { old_gpt: rows("old_gpt"), new_gpt: rows("new_gpt"), esp_files, backup_dir: s(&["backup_dir"]), actions }))
        }
        "done" => Some(Event::Done {
            root_partuuid: s(&["root_partuuid"]),
            disk_guid: s(&["disk_guid"]),
            dry_run: v.get("dry_run").and_then(|x| x.as_bool()).unwrap_or(false),
        }),
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Install into unallocated space next to the existing OS.
    Free,
    /// Shrink the Linux partition first (two sessions), then install.
    Shrink,
    /// Session B: Linux was shrunk and booted; cut its GPT entry and install.
    Finish,
    /// Rewrite the existing LeandrOS partition set in place (home kept).
    Reinstall,
    /// Wipe the whole disk.
    Erase,
}

pub fn install_args(dev: &str, mode: Mode, root_gib: Option<u64>, dry: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["install".into(), "--target".into(), dev.into()];
    if mode != Mode::Erase {
        let flag = match mode {
            Mode::Finish => "--after-shrink",
            Mode::Reinstall => "--reinstall",
            _ => "--use-free",
        };
        a.extend(["--mode".into(), "dualboot".into(), flag.into()]);
    }
    if let Some(g) = root_gib {
        a.extend(["--root-gib".into(), g.to_string()]);
    }
    a.push("--yes".into());
    a.push("--json".into());
    if dry {
        a.push("--dry-run".into());
    }
    a
}

pub fn shrink_args(part: &str, to_bytes: u64) -> Vec<String> {
    vec!["shrink".into(), "--part".into(), part.into(), "--to".into(), to_bytes.to_string(), "--yes".into(), "--json".into()]
}

pub fn recover_args() -> Vec<String> {
    vec!["shrink".into(), "--recover".into(), "--yes".into(), "--json".into()]
}

pub fn mark_verified_args() -> Vec<String> {
    vec!["shrink-status".into(), "--mark-verified".into(), "--json".into()]
}

pub fn shrink_status_args() -> Vec<String> {
    vec!["shrink-status".into(), "--json".into()]
}

/// True when stderr from sudo indicates a bad password.
pub fn wrong_password(stderr: &str) -> bool {
    let l = stderr.to_lowercase();
    l.contains("incorrect password") || l.contains("authentication failed") || l.contains("sorry, try again")
}
