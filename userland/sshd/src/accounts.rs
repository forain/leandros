//! Accounts: /etc/passwd, /etc/group, /etc/shadow and ~/.ssh/authorized_keys.
//!
//! The password format is the system's own (docs/servers/security.md):
//! sha512-crypt `$6$[rounds=N$]<salt>$<hash>` or legacy `$sha256$<salt>$<hex>`
//! (lib/shadowhash/shadowhash.rs, shared source). /bin/login (userland/login) and pam-leandros verify the same
//! field byte for byte; the image builder writes it (shadow_hash in
//! scripts/mkfs-f2fs-populated.py).

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use russh::keys::PublicKey;

#[derive(Clone, Debug)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

/// One /etc/passwd entry by name.
pub fn lookup_user(name: &str) -> Option<User> {
    if name.is_empty() || name.contains(':') || name.contains('\n') {
        return None;
    }
    let text = fs::read_to_string("/etc/passwd").ok()?;
    for line in text.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 || f[0] != name {
            continue;
        }
        return Some(User {
            name: f[0].to_string(),
            uid: f[2].parse().ok()?,
            gid: f[3].parse().ok()?,
            home: if f[5].is_empty() { "/".into() } else { f[5].to_string() },
            shell: if f[6].is_empty() { "/bin/sh".into() } else { f[6].to_string() },
        });
    }
    None
}

/// Supplementary groups: every /etc/group entry whose member list names the
/// user, plus the primary group (initgroups(3)).
pub fn groups_of(user: &User) -> Vec<u32> {
    let mut out = vec![user.gid];
    if let Ok(text) = fs::read_to_string("/etc/group") {
        for line in text.lines() {
            let f: Vec<&str> = line.splitn(4, ':').collect();
            if f.len() < 4 {
                continue;
            }
            let Ok(gid) = f[2].parse::<u32>() else { continue };
            if f[3].split(',').any(|m| m == user.name) && !out.contains(&gid) {
                out.push(gid);
            }
        }
    }
    out.truncate(32); // the kernel's NGROUPS
    out
}

/// A login shell that refuses logins (`/bin/false`, `nologin`) or does not exist.
pub fn shell_usable(user: &User) -> bool {
    let base = Path::new(&user.shell).file_name().and_then(|s| s.to_str()).unwrap_or("");
    if base == "false" || base == "nologin" {
        return false;
    }
    Path::new(&user.shell).exists()
}

fn shadow_field(name: &str) -> Option<String> {
    let text = fs::read_to_string("/etc/shadow").ok()?;
    for line in text.lines() {
        let mut f = line.splitn(3, ':');
        if f.next() == Some(name) {
            return Some(f.next().unwrap_or("").to_string());
        }
    }
    None
}

/// True when the account is locked: a shadow field starting with `!`
/// (`passwd -l`). Locked accounts cannot log in by any method, as with
/// OpenSSH on Linux.
pub fn locked(user: &User) -> bool {
    matches!(shadow_field(&user.name), Some(f) if f.starts_with('!'))
}

/// Check `password` against the account's /etc/shadow field. An empty field
/// (no password) is refused: PermitEmptyPasswords no.
pub fn verify_password(user: &User, password: &str) -> bool {
    let Some(field) = shadow_field(&user.name) else { return false };
    if field.is_empty() {
        return false;
    }
    crate::shadowhash::verify(&field, password.as_bytes())
}

/// StrictModes, as OpenSSH: the file and every directory from it up to the
/// home must be owned by the user or root and not writable by group/other.
fn strict_ok(path: &Path, user: &User) -> Result<(), String> {
    let home = Path::new(&user.home);
    let mut p: Option<&Path> = Some(path);
    while let Some(cur) = p {
        let md = fs::metadata(cur).map_err(|e| format!("{}: {e}", cur.display()))?;
        if md.uid() != user.uid && md.uid() != 0 {
            return Err(format!("bad ownership of {}", cur.display()));
        }
        if md.mode() & 0o022 != 0 {
            return Err(format!("bad modes for {} ({:o})", cur.display(), md.mode() & 0o7777));
        }
        if cur == home {
            break;
        }
        p = cur.parent();
    }
    Ok(())
}

pub fn authorized_keys_path(user: &User, file: &str) -> PathBuf {
    let rel = file.replace("%u", &user.name).replace("%h", &user.home);
    if rel.starts_with('/') {
        PathBuf::from(rel)
    } else {
        Path::new(&user.home).join(rel)
    }
}

/// Per-key restrictions from the options field of an authorized_keys line.
#[derive(Clone, Debug, Default)]
pub struct KeyOpts {
    /// `from="pattern-list"`: allowed client addresses (`*`/`?` globs, `!` negation, CIDR).
    pub from: Option<String>,
    /// `command="..."`: forced command, run instead of whatever the client asked for.
    pub command: Option<String>,
    pub no_pty: bool,
    pub no_port_forwarding: bool,
}

fn glob(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    fn rec(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => (0..=t.len()).any(|i| rec(&p[1..], &t[i..])),
            Some('?') => !t.is_empty() && rec(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && rec(&p[1..], &t[1..]),
        }
    }
    rec(&p, &t)
}

fn cidr_match(pat: &str, ip: std::net::IpAddr) -> Option<bool> {
    let (net, bits) = pat.split_once('/')?;
    let net: std::net::IpAddr = net.parse().ok()?;
    let bits: u32 = bits.parse().ok()?;
    match (net, ip) {
        (std::net::IpAddr::V4(n), std::net::IpAddr::V4(a)) if bits <= 32 => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            Some(u32::from(n) & mask == u32::from(a) & mask)
        }
        (std::net::IpAddr::V6(n), std::net::IpAddr::V6(a)) if bits <= 128 => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            Some(u128::from(n) & mask == u128::from(a) & mask)
        }
        _ => Some(false),
    }
}

/// OpenSSH pattern-list semantics: the address matches when some positive
/// pattern matches and no negated one does.
pub fn from_matches(list: &str, ip: std::net::IpAddr) -> bool {
    let ip = match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    let text = ip.to_string();
    let mut hit = false;
    for pat in list.split(',') {
        let (neg, pat) = match pat.strip_prefix('!') {
            Some(p) => (true, p),
            None => (false, pat),
        };
        let m = cidr_match(pat, ip).unwrap_or_else(|| glob(pat, &text));
        if m {
            if neg {
                return false;
            }
            hit = true;
        }
    }
    hit
}

/// Split `opts key-type base64 comment` into the options text and the key
/// part, honoring double quotes in the options.
fn split_options(line: &str) -> (&str, &str) {
    // A line that starts with a key type has no options.
    let first = line.split_whitespace().next().unwrap_or("");
    if first.starts_with("ssh-") || first.starts_with("ecdsa-") || first.starts_with("sk-") {
        return ("", line);
    }
    let mut quoted = false;
    let mut esc = false;
    for (i, c) in line.char_indices() {
        if esc {
            esc = false;
        } else if c == '\\' && quoted {
            esc = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if c.is_whitespace() && !quoted {
            return (&line[..i], line[i..].trim_start());
        }
    }
    (line, "")
}

/// Parse an options field. An option this server does not implement makes
/// the whole line unusable (Err): a restricted key must never gain access
/// by having its restriction ignored.
fn parse_options(opts: &str) -> Result<KeyOpts, String> {
    let mut out = KeyOpts::default();
    let mut items = Vec::new();
    let (mut cur, mut quoted, mut esc) = (String::new(), false, false);
    for c in opts.chars() {
        if esc {
            cur.push(c);
            esc = false;
        } else if c == '\\' && quoted {
            esc = true;
        } else if c == '"' {
            quoted = !quoted;
            cur.push(c);
        } else if c == ',' && !quoted {
            items.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    items.push(cur);
    let unq = |v: &str| v.trim_matches('"').to_string();
    for it in items.iter().filter(|s| !s.is_empty()) {
        let (k, v) = match it.split_once('=') {
            Some((k, v)) => (k.to_ascii_lowercase(), Some(v)),
            None => (it.to_ascii_lowercase(), None),
        };
        match (k.as_str(), v) {
            ("from", Some(v)) => out.from = Some(unq(v)),
            ("command", Some(v)) => out.command = Some(unq(v)),
            ("no-pty", None) => out.no_pty = true,
            ("no-port-forwarding", None) => out.no_port_forwarding = true,
            ("restrict", None) => {
                out.no_pty = true;
                out.no_port_forwarding = true;
            }
            ("port-forwarding", None) => out.no_port_forwarding = false,
            ("pty", None) => out.no_pty = false,
            // Features this server does not have anyway.
            ("no-agent-forwarding" | "no-x11-forwarding" | "no-user-rc", None) => {}
            _ => return Err(format!("unsupported option {k}")),
        }
    }
    Ok(out)
}

/// Is `key` listed in the user's authorized_keys? Returns the matching
/// line's options. Lines with options this server does not implement are
/// skipped, so a restricted key never gets an unrestricted login.
pub fn key_authorized(user: &User, file: &str, key: &PublicKey) -> Result<Option<KeyOpts>, String> {
    let path = authorized_keys_path(user, file);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    strict_ok(&path, user)?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (opts, rest) = split_options(line);
        let Ok(cand) = PublicKey::from_openssh(rest) else { continue };
        if cand.key_data() != key.key_data() {
            continue;
        }
        match parse_options(opts) {
            Ok(o) => return Ok(Some(o)),
            Err(why) => crate::log(&format!("authorized_keys of {}: line skipped: {why}", user.name)),
        }
    }
    Ok(None)
}
