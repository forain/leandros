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

/// Names of the groups `groups_of` lists, for Match Group.
pub fn group_names(user: &User) -> Vec<String> {
    let gids = groups_of(user);
    let mut out = Vec::new();
    if let Ok(text) = fs::read_to_string("/etc/group") {
        for line in text.lines() {
            let f: Vec<&str> = line.splitn(4, ':').collect();
            if f.len() >= 3 && f[2].parse::<u32>().map_or(false, |g| gids.contains(&g)) {
                out.push(f[0].to_string());
            }
        }
    }
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

/// The text form of an address, an IPv4-mapped IPv6 one as plain IPv4.
pub fn ip_text(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(|v| v.to_string()).unwrap_or_else(|| v6.to_string()),
        v4 => v4.to_string(),
    }
}

/// `AuthorizedPrincipalsFile` path: tokens %u (user), %U (uid), %h (home), %% ; a relative
/// path is under the home. An unknown token is an error (the file is not used).
pub fn principals_path(user: &User, file: &str) -> Result<PathBuf, String> {
    let mut out = String::new();
    let mut it = file.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('%') => out.push('%'),
            Some('u') => out.push_str(&user.name),
            Some('U') => out.push_str(&user.uid.to_string()),
            Some('h') => out.push_str(&user.home),
            other => return Err(format!("unsupported token %{} in AuthorizedPrincipalsFile", other.unwrap_or(' '))),
        }
    }
    Ok(if out.starts_with('/') { PathBuf::from(out) } else { Path::new(&user.home).join(out) })
}

/// Principal names (one per line, optional key options in front) of the user's
/// `AuthorizedPrincipalsFile`: the options of the first line naming one of `cert_principals`.
/// The file and its directories obey StrictModes. `Ok(None)` = no line matches.
pub fn principals_match(user: &User, file: &str, cert_principals: &[String]) -> Result<Option<KeyOpts>, String> {
    let path = principals_path(user, file)?;
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    strict_ok(&path, user)?;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (opts, names) = if line.split_whitespace().count() > 1 {
            let (o, n) = split_options(line);
            (o, n)
        } else {
            ("", line)
        };
        let o = match parse_options(opts) {
            Ok(o) => o,
            Err(why) => {
                crate::log(&format!("principals file of {}: line skipped: {why}", user.name));
                continue;
            }
        };
        if cert_principals.iter().any(|p| p == names.trim()) {
            return Ok(Some(o));
        }
    }
    Ok(None)
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
    /// `permitopen="host:port"` (repeatable): the only `-L` destinations; `None` = unrestricted.
    pub permit_open: Option<Vec<String>>,
    /// `permitlisten="[host:]port"` (repeatable): the only `-R` listen requests; `None` = unrestricted.
    pub permit_listen: Option<Vec<String>>,
    /// `environment="NAME=value"` (repeatable); only applied with PermitUserEnvironment yes.
    pub environment: Vec<(String, String)>,
    /// `expiry-time="YYYYMMDD[HHMM[SS]][Z]"` as a unix time; the key is refused from then on.
    pub expiry: Option<u64>,
    /// `cert-authority`: the line's key is a user CA, not a login key.
    pub cert_authority: bool,
    /// `principals="a,b"` on a cert-authority line: certificate principals accepted instead of the user name.
    pub principals: Option<String>,
}

/// `YYYYMMDD`, `YYYYMMDDHHMM` or `YYYYMMDDHHMMSS`, optional `Z`, as unix seconds.
/// The guest runs in UTC, so a zone-less time is taken as UTC too.
pub fn parse_expiry(v: &str) -> Option<u64> {
    let v = v.trim_end_matches(['Z', 'z']);
    if !matches!(v.len(), 8 | 12 | 14) || !v.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> i64 { v.get(r).and_then(|s| s.parse().ok()).unwrap_or(0) };
    let (y, m, d) = (n(0..4), n(4..6), n(6..8));
    let (hh, mm, ss) = (n(8..10), n(10..12), n(12..14));
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

/// A legacy `$sha256$` field is replaced by a fresh `$6$rounds=65536$` sha512-crypt one,
/// atomically (temp file + rename). Called only after the password verified.
pub fn rehash_legacy(user: &User, password: &str) {
    let Some(field) = shadow_field(&user.name) else { return };
    if !field.starts_with("$sha256$") {
        return;
    }
    let mut rnd = [0u8; 16];
    let ok = fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut rnd)).is_ok();
    if !ok {
        return;
    }
    const AL: &[u8] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let salt: String = rnd.iter().map(|b| AL[(*b & 63) as usize] as char).collect();
    let rounds = 65536;
    let Some(h) = crate::shadowhash::sha512_crypt_raw(password.as_bytes(), salt.as_bytes(), rounds) else { return };
    let new_field = format!("$6$rounds={rounds}${salt}${}", String::from_utf8_lossy(&h));
    if !crate::shadowhash::verify(&new_field, password.as_bytes()) {
        return;
    }
    let Ok(text) = fs::read_to_string("/etc/shadow") else { return };
    let mut out = String::with_capacity(text.len() + 128);
    let mut done = false;
    for line in text.lines() {
        let mut f: Vec<&str> = line.split(':').collect();
        if !done && f.first() == Some(&user.name.as_str()) && f.get(1).is_some_and(|x| x.starts_with("$sha256$")) {
            f[1] = &new_field;
            done = true;
            out.push_str(&f.join(":"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !done {
        return;
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = "/etc/shadow.sshd-new";
    let _ = fs::remove_file(tmp);
    let r = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(tmp).and_then(|mut f| {
        f.write_all(out.as_bytes())?;
        f.sync_all()
    });
    match r.and_then(|_| fs::rename(tmp, "/etc/shadow")) {
        Ok(()) => crate::log(&format!("{}: shadow hash upgraded from $sha256$ to $6$", user.name)),
        Err(e) => {
            let _ = fs::remove_file(tmp);
            crate::log(&format!("{}: shadow hash upgrade failed: {e}", user.name));
        }
    }
}

/// Does `list` (entries `host:port`, `*` globs in the host, `*` or a number
/// as port) allow `host:port`? `None` means no restriction.
pub fn forward_permitted(list: &Option<Vec<String>>, host: &str, port: u32) -> bool {
    let Some(list) = list else { return true };
    list.iter().any(|e| {
        let Some((h, p)) = e.rsplit_once(':') else { return false };
        let h = h.trim_start_matches('[').trim_end_matches(']');
        (p == "*" || p.parse::<u32>().ok() == Some(port)) && glob(&h.to_ascii_lowercase(), &host.to_ascii_lowercase())
    })
}

pub fn glob(pat: &str, s: &str) -> bool {
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
            ("cert-authority", None) => out.cert_authority = true,
            ("principals", Some(v)) => out.principals = Some(unq(v)),
            ("no-pty", None) => out.no_pty = true,
            ("no-port-forwarding", None) => out.no_port_forwarding = true,
            ("restrict", None) => {
                out.no_pty = true;
                out.no_port_forwarding = true;
            }
            ("port-forwarding", None) => out.no_port_forwarding = false,
            ("environment", Some(v)) => {
                let v = unq(v);
                match v.split_once('=') {
                    Some((k, val)) if !k.is_empty() => out.environment.push((k.to_string(), val.to_string())),
                    _ => return Err(format!("bad environment {v}")),
                }
            }
            ("expiry-time", Some(v)) => {
                out.expiry = Some(parse_expiry(&unq(v)).ok_or_else(|| format!("bad expiry-time {v}"))?);
            }
            ("permitopen", Some(v)) => {
                let v = unq(v);
                let l = out.permit_open.get_or_insert_with(Vec::new);
                match v.as_str() {
                    "any" => out.permit_open = None,
                    "none" => {}
                    _ if v.rsplit_once(':').is_some() => l.push(v),
                    _ => return Err(format!("bad permitopen {v}")),
                }
            }
            ("permitlisten", Some(v)) => {
                let v = unq(v);
                let l = out.permit_listen.get_or_insert_with(Vec::new);
                match v.as_str() {
                    "any" => out.permit_listen = None,
                    "none" => {}
                    _ if v.contains(':') => l.push(v),
                    _ => l.push(format!("*:{v}")),
                }
            }
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
            Ok(o) if o.cert_authority => continue,
            Ok(o) => return Ok(Some(o)),
            Err(why) => crate::log(&format!("authorized_keys of {}: line skipped: {why}", user.name)),
        }
    }
    Ok(None)
}

/// The `cert-authority` lines of the user's authorized_keys: (CA key, line options).
pub fn cert_authorities(user: &User, file: &str) -> Vec<(PublicKey, KeyOpts)> {
    let path = authorized_keys_path(user, file);
    let Ok(text) = fs::read_to_string(&path) else { return Vec::new() };
    if strict_ok(&path, user).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (opts, rest) = split_options(line);
        let Ok(ca) = PublicKey::from_openssh(rest) else { continue };
        match parse_options(opts) {
            Ok(o) if o.cert_authority => out.push((ca, o)),
            Ok(_) => {}
            Err(why) => crate::log(&format!("authorized_keys of {}: line skipped: {why}", user.name)),
        }
    }
    out
}

/// CA keys of a `TrustedUserCAKeys` file (one OpenSSH public key per line).
pub fn trusted_cas(file: &str) -> Vec<PublicKey> {
    let Ok(text) = fs::read_to_string(file) else { return Vec::new() };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| PublicKey::from_openssh(l).ok())
        .collect()
}

/// Token values for `AuthorizedPrincipalsCommand` (`%%` needs none).
pub struct CmdTokens<'a> {
    pub user: &'a User,
    pub cert: &'a russh::keys::Certificate,
    pub conn_id: String,
}

fn b64_of(line: &str) -> String {
    line.split_whitespace().nth(1).unwrap_or("").to_string()
}

fn expand_cmd_token(c: char, t: &CmdTokens) -> Result<String, String> {
    use russh::keys::{HashAlg, PublicKey};
    let ca = PublicKey::from(t.cert.signature_key().clone());
    Ok(match c {
        '%' => "%".into(),
        'C' => t.conn_id.clone(),
        'D' => "default".into(),
        'F' => ca.fingerprint(HashAlg::Sha256).to_string(),
        'f' => PublicKey::from(t.cert.public_key().clone()).fingerprint(HashAlg::Sha256).to_string(),
        'h' => t.user.home.clone(),
        'i' => t.cert.key_id().to_string(),
        'K' => b64_of(&ca.to_openssh().map_err(|e| e.to_string())?),
        'k' => b64_of(&t.cert.to_openssh().map_err(|e| e.to_string())?),
        's' => t.cert.serial().to_string(),
        'T' => ca.algorithm().to_string(),
        't' => t.cert.algorithm().to_certificate_type().to_string(),
        'U' => t.user.uid.to_string(),
        'u' => t.user.name.clone(),
        other => return Err(format!("unsupported token %{other} in AuthorizedPrincipalsCommand")),
    })
}

/// Split `cmd` on whitespace (double quotes group), then expand `%` tokens in every word.
pub fn expand_command(cmd: &str, t: &CmdTokens) -> Result<Vec<String>, String> {
    let mut words: Vec<String> = Vec::new();
    let (mut cur, mut inq, mut have) = (String::new(), false, false);
    for c in cmd.chars() {
        match c {
            '"' => {
                inq = !inq;
                have = true;
            }
            c if c.is_whitespace() && !inq => {
                if have {
                    words.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        words.push(cur);
    }
    let mut out = Vec::new();
    for w in words {
        let mut s = String::new();
        let mut it = w.chars();
        while let Some(c) = it.next() {
            if c == '%' {
                s.push_str(&expand_cmd_token(it.next().ok_or("trailing % in AuthorizedPrincipalsCommand")?, t)?);
            } else {
                s.push(c);
            }
        }
        out.push(s);
    }
    if out.is_empty() {
        return Err("empty command".into());
    }
    Ok(out)
}

/// The command and every directory above it must be root-owned and not group/other writable.
fn safe_command_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{}: not an absolute path", path.display()));
    }
    let mut p: Option<&Path> = Some(path);
    while let Some(cur) = p {
        let md = fs::metadata(cur).map_err(|e| format!("{}: {e}", cur.display()))?;
        if md.uid() != 0 || md.mode() & 0o022 != 0 {
            return Err(format!("{}: unsafe ownership or modes ({:o})", cur.display(), md.mode() & 0o7777));
        }
        p = cur.parent();
    }
    Ok(())
}

/// Run `argv` as `runas` (stdin null, stderr dropped, 10 s limit, 64 KiB of output); its stdout on success.
pub fn run_command_as(runas: &User, argv: &[String]) -> Result<String, String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    safe_command_path(Path::new(&argv[0]))?;
    let groups = groups_of(runas);
    let (uid, gid) = (runas.uid, runas.gid);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(move || {
            if uid != 0
                && (libc::setgroups(groups.len() as _, groups.as_ptr()) != 0
                    || libc::setresgid(gid, gid, gid) != 0
                    || libc::setresuid(uid, uid, uid) != 0)
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", argv[0]))?;
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut out).take(65536).read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{}: timed out", argv[0]));
            }
        }
    };
    let buf = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("{}: {status}", argv[0]));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Like [`principals_match`], on the text a command printed (no StrictModes: no file).
pub fn principals_match_text(user: &User, text: &str, cert_principals: &[String]) -> Option<KeyOpts> {
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (opts, names) = if line.split_whitespace().count() > 1 { split_options(line) } else { ("", line) };
        let o = match parse_options(opts) {
            Ok(o) => o,
            Err(why) => {
                crate::log(&format!("principals command of {}: line skipped: {why}", user.name));
                continue;
            }
        };
        if cert_principals.iter().any(|p| p == names.trim()) {
            return Some(o);
        }
    }
    None
}
