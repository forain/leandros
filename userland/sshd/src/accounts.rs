//! Accounts: /etc/passwd, /etc/group, /etc/shadow and ~/.ssh/authorized_keys.
//!
//! The password format is the system's own (docs/servers/security.md):
//! `$sha256$<salt>$<hex>`, hex = lowercase SHA-256 of salt bytes followed by
//! password bytes. /bin/login (userland/login) and pam-leandros verify the same
//! field byte for byte; the image builder writes it (shadow_hash in
//! scripts/mkfs-f2fs-populated.py).

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use russh::keys::PublicKey;
use sha2::{Digest, Sha256};

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
    let Some(rest) = field.strip_prefix("$sha256$") else { return false };
    let Some((salt, hex)) = rest.split_once('$') else { return false };
    if hex.len() != 64 {
        return false;
    }
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(password.as_bytes());
    let digest = h.finalize();
    let mut want = [0u8; 64];
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, b) in digest.iter().enumerate() {
        want[2 * i] = HEX[(b >> 4) as usize];
        want[2 * i + 1] = HEX[(b & 15) as usize];
    }
    // constant time
    let mut diff = 0u8;
    for (a, b) in want.iter().zip(hex.as_bytes()) {
        diff |= a ^ b;
    }
    diff == 0
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

/// Is `key` listed in the user's authorized_keys? Options in front of a key
/// (`command=`, `from=`, ...) are not supported: such lines are skipped, so a
/// restricted key never gets an unrestricted login.
pub fn key_authorized(user: &User, file: &str, key: &PublicKey) -> Result<bool, String> {
    let path = authorized_keys_path(user, file);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    strict_ok(&path, user)?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Ok(cand) = PublicKey::from_openssh(line) else { continue };
        if cand.key_data() == key.key_data() {
            return Ok(true);
        }
    }
    Ok(false)
}
