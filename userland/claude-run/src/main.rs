//! claude-run — the `/bin/claude` launcher: run the Claude Code CLI in its
//! Alpine root as the user who called it.
//!
//! The CLI is a musl Bun executable installed by `claude-code-install` into
//! the Alpine root `/opt/alpine` (it needs Alpine's musl loader, bash, git
//! and ripgrep, none of which the LeandrOS root has). Entering that root takes
//! `mount` and `chroot`, which only root may do, so this program is installed
//! set-user-ID root (mode 04755) and does exactly this, in order:
//!
//! 1. refuses to run unless it really is euid 0 and the root is root-owned and
//!    not writable by anyone else (otherwise a user could plant the tree it
//!    is about to bind mounts into and write `/etc/passwd` lines into);
//! 2. idempotently mounts into the root: `proc` on `/proc` (Bun reads
//!    `/proc/self/exe`), `/dev` (the terminal is a `/dev/pts` node), `/tmp`
//!    (AF_UNIX sockets bind on tmpfs only) and `/home`, plus the caller's
//!    home when it is not under `/home` (root's `/root`), so `~/.claude`
//!    lives in the caller's real home;
//! 3. makes sure the caller's `passwd` entry and groups exist in the root's
//!    `/etc/passwd` and `/etc/group` (`~`, `whoami`, `id`);
//! 4. chroots, then DROPS every privilege before anything of the caller's
//!    runs: the caller's supplementary groups (`setgroups`), then
//!    `setresgid(rgid, rgid, rgid)` and `setresuid(ruid, ruid, ruid)`, and
//!    verifies that `setuid(0)`/`seteuid(0)` now fail;
//! 5. as the caller, changes to the caller's working directory when it is
//!    visible inside the root (under `/home`, `/tmp` or the home), else the
//!    home, and execs `/usr/local/bin/claude` with the caller's environment
//!    (minus `LD_*`) and `PATH`, `SHELL`, `HOME`, `USER`, `LOGNAME` set for
//!    the root.
//!
//! Run by root (uid 0) it does the same and stays root. `CLAUDE_CODE_ROOT`
//! moves the root, but only for a caller that is root already.
//!
//! Invoked as `alpine-run` (a hard link of the same binary) it runs any
//! command of the root instead: `alpine-run tmux new -s work`. That grants
//! nothing `claude` does not (its Bash tool runs anything in the root as the
//! caller too): every privilege is gone before the command starts.

use std::ffi::{c_char, CString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

type UidT = u32;
type GidT = u32;

extern "C" {
    fn getuid() -> UidT;
    fn geteuid() -> UidT;
    fn getgid() -> GidT;
    fn getgroups(size: i32, list: *mut GidT) -> i32;
    fn setgroups(size: usize, list: *const GidT) -> i32;
    fn setresuid(r: UidT, e: UidT, s: UidT) -> i32;
    fn setresgid(r: GidT, e: GidT, s: GidT) -> i32;
    fn getresuid(r: *mut UidT, e: *mut UidT, s: *mut UidT) -> i32;
    fn getresgid(r: *mut GidT, e: *mut GidT, s: *mut GidT) -> i32;
    fn setuid(u: UidT) -> i32;
    fn seteuid(u: UidT) -> i32;
    fn mount(src: *const c_char, target: *const c_char, fstype: *const c_char, flags: u64, data: *const u8) -> i32;
    fn chroot(path: *const c_char) -> i32;
    fn umask(mask: u32) -> u32;
}

const MS_BIND: u64 = 4096;
const DEFAULT_ROOT: &str = "/opt/alpine";
const CLI: &str = "/usr/local/bin/claude";
const ROOT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn die(msg: &str) -> ! {
    eprintln!("claude: {msg}");
    exit(126);
}

fn errno_str() -> String { std::io::Error::last_os_error().to_string() }

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap_or_else(|_| die("path with a NUL byte"))
}

/// One `/etc/passwd` entry of the caller.
struct User { name: String, gid: GidT, gecos: String, home: String }

fn lookup_user(uid: UidT, gid: GidT) -> User {
    if let Ok(text) = fs::read_to_string("/etc/passwd") {
        for line in text.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() >= 7 && f[2].parse::<UidT>().ok() == Some(uid) {
                return User {
                    name: f[0].to_string(),
                    gid: f[3].parse().unwrap_or(gid),
                    gecos: f[4].to_string(),
                    home: if f[5].starts_with('/') { f[5].to_string() } else { "/".into() },
                };
            }
        }
    }
    User { name: uid.to_string(), gid, gecos: String::new(), home: "/".into() }
}

/// Mount points currently listed in /proc/mounts (namespace paths).
fn mount_points() -> Vec<String> {
    fs::read_to_string("/proc/mounts").unwrap_or_default()
        .lines().filter_map(|l| l.split_whitespace().nth(1).map(str::to_string)).collect()
}

/// `dir` (inside the root) must be a real directory, never a symlink: the
/// root is root-owned, but a link could still send a bind mount elsewhere.
/// Missing directories are created, root-owned, 0755.
fn ensure_dir(dir: &Path) {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_dir() => {}
        Ok(_) => die(&format!("{} is not a directory", dir.display())),
        Err(_) => {
            if let Some(parent) = dir.parent() { ensure_dir(parent); }
            if fs::create_dir(dir).is_err() && !dir.is_dir() {
                die(&format!("cannot create {}: {}", dir.display(), errno_str()));
            }
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        }
    }
}

/// Mount `src` on `target` unless something is mounted there already.
fn ensure_mount(src: &str, target: &Path, fstype: Option<&str>, flags: u64) {
    let t = target.to_string_lossy().into_owned();
    if mount_points().iter().any(|m| *m == t) { return; }
    ensure_dir(target);
    let s = CString::new(src).unwrap();
    let ty = fstype.map(|f| CString::new(f).unwrap());
    let r = unsafe {
        mount(s.as_ptr(), cstr(target).as_ptr(),
              ty.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()), flags, std::ptr::null())
    };
    // A concurrent launch may have won the race; that is fine.
    if r != 0 && !mount_points().iter().any(|m| *m == t) {
        die(&format!("mount {src} on {t}: {}", errno_str()));
    }
}

/// Append `line` to `file` unless a line's field `key_idx` already equals
/// `key`, or the name (field 0) is already taken by another id, in which case
/// Alpine's entry stays and nothing is written.
fn ensure_entry(file: &Path, key_idx: usize, key: &str, name: &str, line: &str) {
    let text = fs::read_to_string(file).unwrap_or_default();
    for l in text.lines() {
        let f: Vec<&str> = l.split(':').collect();
        if f.get(key_idx) == Some(&key) { return; }
        if f.first() == Some(&name) { return; } // name taken by another id: leave Alpine's
    }
    let mut out = match OpenOptions::new().append(true).create(true).open(file) {
        Ok(f) => f,
        Err(e) => die(&format!("{}: {e}", file.display())),
    };
    let sep = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
    let _ = write!(out, "{sep}{line}\n");
}

fn host_group_line(gid: GidT) -> Option<(String, String)> {
    let text = fs::read_to_string("/etc/group").ok()?;
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 3 && f[2].parse::<GidT>().ok() == Some(gid)).then(|| (f[0].to_string(), l.to_string()))
    })
}

/// The caller's supplementary groups, as the process holds them now.
fn caller_groups() -> Vec<GidT> {
    let n = unsafe { getgroups(0, std::ptr::null_mut()) };
    if n <= 0 { return Vec::new(); }
    let mut v = vec![0 as GidT; n as usize];
    let n = unsafe { getgroups(n, v.as_mut_ptr()) };
    v.truncate(n.max(0) as usize);
    v
}

/// The root must belong to root and be writable by root alone, and so must
/// its /etc: this program writes passwd/group lines there and mounts into it.
fn check_root(root: &Path) {
    for p in [root.to_path_buf(), root.join("etc")] {
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0 => {}
            Ok(_) => die(&format!("{} must be a root-owned directory not writable by others", p.display())),
            Err(_) => die(&format!("not installed in {}; run claude-code-install as root", root.display())),
        }
    }
}

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let generic = args.first().map(|a| Path::new(a).file_name() == Some("alpine-run".as_ref()))
        .unwrap_or(false);
    if generic && args.len() < 2 {
        eprintln!("usage: alpine-run COMMAND [ARG]...   (runs COMMAND in the Alpine root as you)");
        exit(2);
    }
    let ruid = unsafe { getuid() };
    let rgid = unsafe { getgid() };
    if unsafe { geteuid() } != 0 {
        die("not set-user-ID root: install /bin/claude as root, mode 04755");
    }
    let root = PathBuf::from(match std::env::var_os("CLAUDE_CODE_ROOT") {
        Some(r) if ruid == 0 => r,
        _ => DEFAULT_ROOT.into(),
    });
    check_root(&root);
    // Resolved like the chroot will resolve it (the link is relative).
    if !generic && fs::metadata(root.join(CLI.trim_start_matches('/'))).is_err() {
        eprintln!("claude: not installed in {}; run claude-code-install as root", root.display());
        exit(127);
    }
    let groups = caller_groups();
    let user = lookup_user(ruid, rgid);
    let cwd = std::env::current_dir().ok();
    unsafe { umask(0o022); }

    // ── mounts ──────────────────────────────────────────────────────────────
    let r = |p: &str| root.join(p.trim_start_matches('/'));
    ensure_mount("proc", &r("/proc"), Some("proc"), 0);
    ensure_mount("/dev", &r("/dev"), None, MS_BIND);
    ensure_mount("/tmp", &r("/tmp"), None, MS_BIND);
    if Path::new("/home").is_dir() { ensure_mount("/home", &r("/home"), None, MS_BIND); }
    let home_outside = user.home != "/" && !user.home.starts_with("/home/") && user.home != "/home";
    if home_outside && Path::new(&user.home).is_dir() {
        ensure_mount(&user.home, &r(&user.home), None, MS_BIND);
    }

    // ── identity inside the root ────────────────────────────────────────────
    let passwd = format!("{}:x:{}:{}:{}:{}:/bin/bash", user.name, ruid, user.gid, user.gecos, user.home);
    ensure_entry(&r("/etc/passwd"), 2, &ruid.to_string(), &user.name, &passwd);
    let mut gids = groups.clone();
    gids.push(user.gid);
    gids.push(rgid);
    gids.sort_unstable();
    gids.dedup();
    for g in gids {
        if let Some((name, line)) = host_group_line(g) {
            ensure_entry(&r("/etc/group"), 2, &g.to_string(), &name, &line);
        }
    }

    // ── enter, then drop privileges for good ────────────────────────────────
    if unsafe { chroot(cstr(&root).as_ptr()) } != 0 { die(&format!("chroot: {}", errno_str())); }
    if std::env::set_current_dir("/").is_err() { die("chdir /"); }
    unsafe {
        if setgroups(groups.len(), groups.as_ptr()) != 0 { die(&format!("setgroups: {}", errno_str())); }
        if setresgid(rgid, rgid, rgid) != 0 { die(&format!("setresgid: {}", errno_str())); }
        if setresuid(ruid, ruid, ruid) != 0 { die(&format!("setresuid: {}", errno_str())); }
        let (mut a, mut b, mut c) = (0, 0, 0);
        getresuid(&mut a, &mut b, &mut c);
        let (mut d, mut e, mut f) = (0, 0, 0);
        getresgid(&mut d, &mut e, &mut f);
        if (a, b, c) != (ruid, ruid, ruid) || (d, e, f) != (rgid, rgid, rgid) {
            die("privileges were not dropped");
        }
        if ruid != 0 && (setuid(0) == 0 || seteuid(0) == 0) {
            die("could regain root after dropping it; refusing to run");
        }
    }

    // ── as the caller ───────────────────────────────────────────────────────
    let visible = |p: &Path| {
        let h = Path::new(&user.home);
        p.starts_with("/home") || p.starts_with("/tmp") || (user.home != "/" && p.starts_with(h))
    };
    let mut entered = false;
    if let Some(c) = cwd.as_deref().filter(|c| visible(c)) {
        entered = std::env::set_current_dir(c).is_ok();
    }
    if !entered && std::env::set_current_dir(&user.home).is_err() {
        let _ = std::env::set_current_dir("/");
    }

    let mut cmd = if generic {
        let mut c = Command::new(&args[1]);
        c.args(&args[2..]);
        c
    } else {
        let mut c = Command::new(CLI);
        c.arg0("claude");
        c.args(&args[1..]);
        c
    };
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        let kb = k.as_bytes();
        if kb.starts_with(b"LD_") { continue; }
        if matches!(kb, b"PATH" | b"SHELL" | b"HOME" | b"USER" | b"LOGNAME") { continue; }
        cmd.env(k, v);
    }
    cmd.env("PATH", ROOT_PATH)
        .env("SHELL", "/bin/bash")
        .env("HOME", &user.home)
        .env("USER", &user.name)
        .env("LOGNAME", &user.name);
    if std::env::var_os("TERM").is_none() { cmd.env("TERM", "xterm-256color"); }
    if std::env::var_os("LANG").is_none() { cmd.env("LANG", "C.UTF-8"); }
    let err = cmd.exec();
    let what = if generic { args[1].to_string_lossy().into_owned() } else { CLI.to_string() };
    die(&format!("exec {what}: {err}"));
}
