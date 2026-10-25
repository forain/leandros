//! Secrets lifted off the kernel command line.
//!
//! The command line is copied into many places (`/proc/cmdline`, the
//! console echo that lands in the persistent klog, kexec's
//! `--reuse-cmdline`, per-driver copies), so a credential on it leaks
//! everywhere. At capture time the kernel removes every secret-looking
//! `key=value` knob (see `vfs_server::cmdline_redact::is_secret_key`) from
//! its stored command line and parks the value here, where only the one
//! consumer that asks by name can take it (once). Nothing else ever sees it.
//!
//! The only consumer today is the Wi-Fi driver's deprecated `wifi.psk=`
//! fallback; the supported place for the passphrase is the root-only
//! `/etc/leandros/wifi.conf` (or a stored netctl profile).

use spin::Mutex;

const SLOTS: usize = 4;
const KEY_MAX: usize = 32;
const VAL_MAX: usize = 128;

struct Slot { klen: usize, vlen: usize, key: [u8; KEY_MAX], val: [u8; VAL_MAX] }

const EMPTY: Slot = Slot { klen: 0, vlen: 0, key: [0; KEY_MAX], val: [0; VAL_MAX] };
static STASH: Mutex<[Slot; SLOTS]> = Mutex::new([EMPTY; SLOTS]);

/// Park `val` under `key`. Returns false (and stores nothing) when the
/// key/value is too long or the stash is full; the value is dropped then.
pub fn stash(key: &[u8], val: &[u8]) -> bool {
    if key.is_empty() || key.len() > KEY_MAX || val.len() > VAL_MAX { return false; }
    let mut s = STASH.lock();
    let Some(slot) = s.iter_mut().find(|x| x.klen == 0 || &x.key[..x.klen] == key) else { return false };
    slot.key[..key.len()].copy_from_slice(key);
    slot.klen = key.len();
    slot.val = [0; VAL_MAX];
    slot.val[..val.len()].copy_from_slice(val);
    slot.vlen = val.len();
    true
}

/// Take (and wipe) the secret stashed under `key`.
pub fn take(key: &str) -> Option<alloc::vec::Vec<u8>> {
    let mut s = STASH.lock();
    let slot = s.iter_mut().find(|x| x.klen != 0 && &x.key[..x.klen] == key.as_bytes())?;
    let v = slot.val[..slot.vlen].to_vec();
    for b in slot.val.iter_mut() { unsafe { core::ptr::write_volatile(b, 0) }; }
    slot.vlen = 0;
    Some(v)
}

/// Copy the secret stashed under `key` without wiping it. The two in-kernel
/// consumers of the legacy Wi-Fi credentials (iwlwifi and the netctl legacy
/// profile) both need it and start in no fixed order; it never leaves the
/// kernel (/proc/cmdline and the klog stay redacted).
pub fn peek(key: &str) -> Option<alloc::vec::Vec<u8>> {
    let s = STASH.lock();
    let slot = s.iter().find(|x| x.klen != 0 && &x.key[..x.klen] == key.as_bytes())?;
    Some(slot.val[..slot.vlen].to_vec())
}

/// Was any secret lifted off the command line (key names only)?
pub fn stashed_keys(mut f: impl FnMut(&str)) {
    let s = STASH.lock();
    for x in s.iter().filter(|x| x.klen != 0) {
        if let Ok(k) = core::str::from_utf8(&x.key[..x.klen]) { f(k); }
    }
}
