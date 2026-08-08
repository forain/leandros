//! `pam-leandros` — the libpam application ABI, implemented for LeandrOS.
//!
//! greetd authenticates through `pam-sys`, which is a raw FFI binding to
//! `libpam`. LeandrOS has no PAM stack, no `/etc/pam.d` modules, no NSS and no
//! logind, so there is nothing for those bindings to bind to. This crate
//! supplies the symbols `pam-sys` references and implements the only part that
//! has to do real work — checking a password against `/etc/shadow` — using the
//! same scheme `/bin/login` already validates:
//!
//! ```text
//! shadow field = "$sha256$<salt>$<hexdigest>"
//! hexdigest    = lowercase hex of SHA-256( salt_bytes || password_bytes )
//! empty field  = passwordless (authentication always succeeds)
//! ```
//!
//! No iteration count, no base64, no `crypt(3)`.
//!
//! # Why an rlib and not a `staticlib`
//!
//! A Rust `staticlib` embeds its own copy of `std`; linking one into another
//! Rust binary duplicates every `std` symbol. So this is an ordinary rlib that
//! greetd depends on directly, and the `#[no_mangle] extern "C"` definitions
//! below resolve `pam-sys`'s undefined references at link time exactly as a
//! real `libpam.a` would. `pam-sys`'s build script still emits
//! `-lpam -lpam_misc`, so the build supplies two empty archives by those names
//! for the linker to find and pull nothing out of — see `ports/greetd/build.sh`.
//!
//! # What is real and what is a stub
//!
//! Real: `pam_authenticate` (conversation + `/etc/shadow`), the item table,
//! and the environment table. `pam_putenv`/`pam_getenvlist` are **not**
//! stubs here and must not become stubs: greetd builds the session's *entire*
//! environment through them (`session/worker.rs`), including the `GREETD_SOCK`
//! the greeter reads to find the socket. An empty env list means the greeter
//! starts with no environment at all.
//!
//! Stubs returning success: `pam_setcred`, `pam_acct_mgmt`, `pam_open_session`,
//! `pam_close_session`. There is no credential cache, no account expiry
//! database and no session registry on LeandrOS for them to act on.
//!
//! # Concurrency
//!
//! A handle is a plain heap allocation with no interior locking. greetd drives
//! PAM only from the forked session worker, which is single-threaded, and real
//! libpam handles are not thread-safe either.

use std::ffi::{CStr, CString};
use std::fs;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

mod sha256;

// ── Linux-PAM constants (security/_pam_types.h) ─────────────────────────────

pub const PAM_SUCCESS: c_int = 0;
pub const PAM_SYSTEM_ERR: c_int = 4;
pub const PAM_BUF_ERR: c_int = 5;
pub const PAM_PERM_DENIED: c_int = 6;
pub const PAM_AUTH_ERR: c_int = 7;
pub const PAM_USER_UNKNOWN: c_int = 10;
pub const PAM_CONV_ERR: c_int = 19;
pub const PAM_ABORT: c_int = 26;

const PAM_PROMPT_ECHO_OFF: c_int = 1;

const PAM_SERVICE: c_int = 1;
const PAM_USER: c_int = 2;
const PAM_TTY: c_int = 3;
const PAM_RHOST: c_int = 4;
const PAM_CONV: c_int = 5;
const PAM_RUSER: c_int = 8;

/// `struct pam_message`.
#[repr(C)]
pub struct PamMessage {
    pub msg_style: c_int,
    pub msg: *const c_char,
}

/// `struct pam_response`. The conversation function allocates these with
/// `calloc`, so they are released here with `free`.
#[repr(C)]
pub struct PamResponse {
    pub resp: *mut c_char,
    pub resp_retcode: c_int,
}

/// `struct pam_conv`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PamConv {
    pub conv: Option<
        extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int,
    >,
    pub appdata_ptr: *mut c_void,
}

/// `pam_handle_t`. Opaque to callers; they only ever hold the pointer.
pub struct PamHandle {
    service: Option<CString>,
    user: Option<CString>,
    tty: Option<CString>,
    rhost: Option<CString>,
    ruser: Option<CString>,
    conv: PamConv,
    /// `NAME=VALUE` entries, in insertion order. Replacing a name keeps its
    /// original position, which is what Linux-PAM does.
    env: Vec<CString>,
}

/// Turn a raw handle back into a reference, or bail out with `err`.
macro_rules! handle {
    ($pamh:expr, $err:expr) => {
        match unsafe { ($pamh as *mut PamHandle).as_mut() } {
            Some(h) => h,
            None => return $err,
        }
    };
}

fn dup_cstr(s: *const c_char) -> Option<CString> {
    if s.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(s) }.to_owned())
    }
}

// ── lifecycle ───────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn pam_start(
    service_name: *const c_char,
    user: *const c_char,
    pam_conversation: *const PamConv,
    pamh: *mut *mut PamHandle,
) -> c_int {
    if pamh.is_null() {
        return PAM_ABORT;
    }
    let h = Box::new(PamHandle {
        service: dup_cstr(service_name),
        user: dup_cstr(user),
        tty: None,
        rhost: None,
        ruser: None,
        conv: match pam_conversation.as_ref() {
            Some(c) => *c,
            None => PamConv { conv: None, appdata_ptr: ptr::null_mut() },
        },
        env: Vec::new(),
    });
    *pamh = Box::into_raw(h);
    PAM_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn pam_end(pamh: *mut PamHandle, _pam_status: c_int) -> c_int {
    if pamh.is_null() {
        return PAM_SYSTEM_ERR;
    }
    drop(Box::from_raw(pamh));
    PAM_SUCCESS
}

// ── authentication ──────────────────────────────────────────────────────────

/// Ask the application for the password through its conversation callback and
/// check it against `/etc/shadow`.
///
/// A single `PAM_PROMPT_ECHO_OFF` message is what a real `pam_unix` sends, and
/// it is what greetd relays to the greeter as
/// `auth_message{auth_message_type: "secret"}` — so the prompt text here is
/// what the login screen displays.
#[no_mangle]
pub unsafe extern "C" fn pam_authenticate(pamh: *mut PamHandle, _flags: c_int) -> c_int {
    let h = handle!(pamh, PAM_ABORT);
    let conv = match h.conv.conv {
        Some(f) => f,
        None => return PAM_CONV_ERR,
    };

    let prompt = match CString::new("Password: ") {
        Ok(p) => p,
        Err(_) => return PAM_BUF_ERR,
    };
    let msg = PamMessage { msg_style: PAM_PROMPT_ECHO_OFF, msg: prompt.as_ptr() };
    let msgp: *const PamMessage = &msg;
    let mut resp: *mut PamResponse = ptr::null_mut();

    let rc = conv(1, &msgp as *const _ as *mut _, &mut resp, h.conv.appdata_ptr);
    if rc != PAM_SUCCESS {
        free_responses(resp, 1);
        return PAM_AUTH_ERR;
    }
    if resp.is_null() {
        return PAM_CONV_ERR;
    }

    let user = match &h.user {
        Some(u) => u.to_string_lossy().into_owned(),
        None => {
            free_responses(resp, 1);
            return PAM_USER_UNKNOWN;
        }
    };

    let password: Vec<u8> = {
        let p = (*resp).resp;
        if p.is_null() { Vec::new() } else { CStr::from_ptr(p).to_bytes().to_vec() }
    };
    let verdict = verify_user(&user, &password);
    free_responses(resp, 1);
    verdict
}

/// Wipe and release a `pam_response` array allocated by the caller's
/// conversation function (`calloc`, so `free`).
unsafe fn free_responses(resp: *mut PamResponse, num: usize) {
    if resp.is_null() {
        return;
    }
    for i in 0..num {
        let r = resp.add(i);
        if !(*r).resp.is_null() {
            let len = CStr::from_ptr((*r).resp).to_bytes().len();
            ptr::write_bytes((*r).resp as *mut u8, 0, len);
            libc::free((*r).resp as *mut c_void);
            (*r).resp = ptr::null_mut();
        }
    }
    libc::free(resp as *mut c_void);
}

/// Look `user` up in `/etc/shadow` and check `password` against the stored
/// hash. An unknown user and a wrong password are deliberately the same
/// answer to the caller.
fn verify_user(user: &str, password: &[u8]) -> c_int {
    let shadow = match fs::read_to_string("/etc/shadow") {
        Ok(s) => s,
        Err(_) => return PAM_SYSTEM_ERR,
    };
    for line in shadow.lines() {
        let mut fields = line.splitn(3, ':');
        if fields.next() != Some(user) {
            continue;
        }
        let hash_field = fields.next().unwrap_or("");
        return if verify_password(hash_field, password) { PAM_SUCCESS } else { PAM_AUTH_ERR };
    }
    PAM_AUTH_ERR
}

/// `$sha256$<salt>$<hex>`; an empty field means passwordless. Kept
/// byte-compatible with `userland/login/src/main.rs`'s `verify_password` and
/// with what `scripts/mkfs-f2fs-populated.py` writes into `/etc/shadow`.
fn verify_password(hash_field: &str, password: &[u8]) -> bool {
    if hash_field.is_empty() {
        return true;
    }
    let rest = match hash_field.strip_prefix("$sha256$") {
        Some(r) => r,
        None => return false,
    };
    let mut parts = rest.splitn(2, '$');
    let salt = parts.next().unwrap_or("");
    let hex = parts.next().unwrap_or("");
    if hex.is_empty() {
        return false;
    }

    let mut msg = Vec::with_capacity(salt.len() + password.len());
    msg.extend_from_slice(salt.as_bytes());
    msg.extend_from_slice(password);

    let digest = sha256::sha256(&msg);
    let mut hexbuf = [0u8; 64];
    sha256::to_hex(&digest, &mut hexbuf);

    ct_eq(&hexbuf, hex.as_bytes())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ── account / credential / session stubs ────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn pam_acct_mgmt(_pamh: *mut PamHandle, _flags: c_int) -> c_int {
    PAM_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn pam_setcred(_pamh: *mut PamHandle, _flags: c_int) -> c_int {
    PAM_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn pam_open_session(_pamh: *mut PamHandle, _flags: c_int) -> c_int {
    PAM_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn pam_close_session(_pamh: *mut PamHandle, _flags: c_int) -> c_int {
    PAM_SUCCESS
}

/// Changing a password is not supported: `/etc/shadow` is on the read-mostly
/// root image and there is no `passwd` implementation to keep in step with.
/// greetd only reaches this on `PAM_NEW_AUTHTOK_REQD`, which `pam_acct_mgmt`
/// above never returns.
#[no_mangle]
pub unsafe extern "C" fn pam_chauthtok(_pamh: *mut PamHandle, _flags: c_int) -> c_int {
    PAM_PERM_DENIED
}

#[no_mangle]
pub unsafe extern "C" fn pam_fail_delay(_pamh: *mut PamHandle, _usec: u32) -> c_int {
    PAM_SUCCESS
}

// ── items ───────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn pam_set_item(
    pamh: *mut PamHandle,
    item_type: c_int,
    item: *const c_void,
) -> c_int {
    let h = handle!(pamh, PAM_SYSTEM_ERR);
    match item_type {
        PAM_CONV => {
            if let Some(c) = (item as *const PamConv).as_ref() {
                h.conv = *c;
            }
        }
        PAM_SERVICE => h.service = dup_cstr(item as *const c_char),
        PAM_USER => h.user = dup_cstr(item as *const c_char),
        PAM_TTY => h.tty = dup_cstr(item as *const c_char),
        PAM_RHOST => h.rhost = dup_cstr(item as *const c_char),
        PAM_RUSER => h.ruser = dup_cstr(item as *const c_char),
        _ => {}
    }
    PAM_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn pam_get_item(
    pamh: *const PamHandle,
    item_type: c_int,
    item: *mut *const c_void,
) -> c_int {
    let h = match (pamh as *mut PamHandle).as_mut() {
        Some(h) => h,
        None => return PAM_SYSTEM_ERR,
    };
    if item.is_null() {
        return PAM_SYSTEM_ERR;
    }
    let p = |s: &Option<CString>| s.as_ref().map_or(ptr::null(), |c| c.as_ptr()) as *const c_void;
    *item = match item_type {
        PAM_CONV => &h.conv as *const PamConv as *const c_void,
        PAM_SERVICE => p(&h.service),
        PAM_USER => p(&h.user),
        PAM_TTY => p(&h.tty),
        PAM_RHOST => p(&h.rhost),
        PAM_RUSER => p(&h.ruser),
        _ => ptr::null(),
    };
    PAM_SUCCESS
}

/// The username the session will run as. greetd calls this after
/// authentication and feeds the answer straight to `getpwnam`, so it must be
/// the name that was authenticated — not a prompt. `prompt` is ignored: the
/// name is always already known here, because greetd passes it to `pam_start`.
#[no_mangle]
pub unsafe extern "C" fn pam_get_user(
    pamh: *mut PamHandle,
    user: *mut *const c_char,
    _prompt: *const c_char,
) -> c_int {
    let h = handle!(pamh, PAM_SYSTEM_ERR);
    if user.is_null() {
        return PAM_SYSTEM_ERR;
    }
    match &h.user {
        Some(u) => {
            *user = u.as_ptr();
            PAM_SUCCESS
        }
        None => PAM_USER_UNKNOWN,
    }
}

#[no_mangle]
pub unsafe extern "C" fn pam_strerror(_pamh: *mut PamHandle, errnum: c_int) -> *const c_char {
    let s: &[u8] = match errnum {
        PAM_SUCCESS => b"Success\0",
        PAM_SYSTEM_ERR => b"System error\0",
        PAM_BUF_ERR => b"Memory buffer error\0",
        PAM_PERM_DENIED => b"Permission denied\0",
        PAM_AUTH_ERR => b"Authentication failure\0",
        PAM_USER_UNKNOWN => b"User not known to the underlying authentication module\0",
        PAM_CONV_ERR => b"Conversation error\0",
        PAM_ABORT => b"Critical error - immediate abort\0",
        _ => b"Unknown PAM error\0",
    };
    s.as_ptr() as *const c_char
}

// ── environment ─────────────────────────────────────────────────────────────
//
// This is the load-bearing part for greetd. `session/worker.rs` puts
// GREETD_SOCK, XDG_SEAT, XDG_SESSION_CLASS, USER, LOGNAME, HOME, SHELL, TERM
// and every variable the greeter sent in `start_session{env}` through
// `pam_putenv`, then reads the whole set back with `pam_getenvlist` and passes
// it to `execve` as the session's complete environment. Whatever is missing
// here is missing from the session.

/// Linux-PAM semantics: `NAME=VALUE` sets, `NAME=` sets empty, a bare `NAME`
/// deletes. Deleting a name that is not set is not an error.
#[no_mangle]
pub unsafe extern "C" fn pam_putenv(pamh: *mut PamHandle, name_value: *const c_char) -> c_int {
    let h = handle!(pamh, PAM_SYSTEM_ERR);
    if name_value.is_null() {
        return PAM_PERM_DENIED;
    }
    let entry = CStr::from_ptr(name_value).to_bytes();

    match entry.iter().position(|&b| b == b'=') {
        Some(eq) => {
            let name = &entry[..=eq]; // includes '=' so "FOO" never matches "FOOBAR="
            let replacement = match CString::new(entry) {
                Ok(c) => c,
                Err(_) => return PAM_BUF_ERR,
            };
            match h.env.iter().position(|e| e.to_bytes().starts_with(name)) {
                Some(i) => h.env[i] = replacement,
                None => h.env.push(replacement),
            }
        }
        None => {
            let mut name = entry.to_vec();
            name.push(b'=');
            h.env.retain(|e| !e.to_bytes().starts_with(&name));
        }
    }
    PAM_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn pam_getenv(pamh: *mut PamHandle, name: *const c_char) -> *const c_char {
    let h = handle!(pamh, ptr::null());
    if name.is_null() {
        return ptr::null();
    }
    let mut key = CStr::from_ptr(name).to_bytes().to_vec();
    key.push(b'=');
    match h.env.iter().find(|e| e.to_bytes().starts_with(&key)) {
        Some(e) => unsafe { e.as_ptr().add(key.len()) },
        None => ptr::null(),
    }
}

/// A NULL-terminated array of `NAME=VALUE` strings, allocated with `malloc` so
/// the caller can release it with `pam_misc_drop_env` below (which is exactly
/// what greetd's `PamEnvList::drop` does).
///
/// Returning NULL is not an option: greetd treats it as
/// "unable to retrieve environment" and fails the session. An empty table
/// therefore still yields a one-element array holding only the terminator.
#[no_mangle]
pub unsafe extern "C" fn pam_getenvlist(pamh: *mut PamHandle) -> *mut *mut c_char {
    let h = handle!(pamh, ptr::null_mut());

    let n = h.env.len();
    let list = libc::calloc(n + 1, std::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
    if list.is_null() {
        return ptr::null_mut();
    }
    for (i, entry) in h.env.iter().enumerate() {
        let bytes = entry.to_bytes_with_nul();
        let dup = libc::malloc(bytes.len()) as *mut c_char;
        if dup.is_null() {
            // Release what was built so far rather than hand back a
            // half-populated array.
            for j in 0..i {
                libc::free(*list.add(j) as *mut c_void);
            }
            libc::free(list as *mut c_void);
            return ptr::null_mut();
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), dup as *mut u8, bytes.len());
        *list.add(i) = dup;
    }
    *list.add(n) = ptr::null_mut();
    list
}

/// From `libpam_misc`, not `libpam` — `pam-sys`'s build script links both, and
/// greetd's `PamEnvList` frees its list with this. Same allocator as
/// `pam_getenvlist` above.
#[no_mangle]
pub unsafe extern "C" fn pam_misc_drop_env(env: *mut *mut c_char) -> c_int {
    if env.is_null() {
        return PAM_SUCCESS;
    }
    let mut i = 0isize;
    while !(*env.offset(i)).is_null() {
        libc::free(*env.offset(i) as *mut c_void);
        i += 1;
    }
    libc::free(env as *mut c_void);
    PAM_SUCCESS
}

/// Also `libpam_misc`. greetd does not call these two, but they are declared
/// in the same `pam-sys` FFI block; defining them keeps a link failure from
/// depending on which declarations the optimiser happens to keep.
#[no_mangle]
pub unsafe extern "C" fn pam_misc_paste_env(
    pamh: *mut PamHandle,
    user_env: *const *const c_char,
) -> c_int {
    if user_env.is_null() {
        return PAM_SUCCESS;
    }
    let mut i = 0isize;
    while !(*user_env.offset(i)).is_null() {
        let rc = pam_putenv(pamh, *user_env.offset(i));
        if rc != PAM_SUCCESS {
            return rc;
        }
        i += 1;
    }
    PAM_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn pam_misc_setenv(
    pamh: *mut PamHandle,
    name: *const c_char,
    value: *const c_char,
    _readonly: c_int,
) -> c_int {
    if name.is_null() {
        return PAM_PERM_DENIED;
    }
    let mut entry = CStr::from_ptr(name).to_bytes().to_vec();
    entry.push(b'=');
    if !value.is_null() {
        entry.extend_from_slice(CStr::from_ptr(value).to_bytes());
    }
    match CString::new(entry) {
        Ok(c) => pam_putenv(pamh, c.as_ptr()),
        Err(_) => PAM_BUF_ERR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash `/bin/login` accepts for root must be the hash this accepts.
    /// Salt "lnd0" and password "root" are what
    /// `scripts/mkfs-f2fs-populated.py`'s `shadow_hash` writes for root, and
    /// the literal below is that function's output — so a divergence in either
    /// hasher fails here rather than at a login prompt.
    #[test]
    fn shadow_scheme_matches_login() {
        let digest = sha256::sha256(b"lnd0root");
        let mut hex = [0u8; 64];
        sha256::to_hex(&digest, &mut hex);
        let field = format!("$sha256$lnd0${}", std::str::from_utf8(&hex).unwrap());
        assert_eq!(
            field,
            "$sha256$lnd0$777a310c8707848af9b04df014e11fe313c7603650c649862d13b53553bf9875"
        );
        assert!(verify_password(&field, b"root"));
        assert!(!verify_password(&field, b"rootx"));
        assert!(!verify_password(&field, b""));
    }

    #[test]
    fn empty_field_is_passwordless() {
        assert!(verify_password("", b"anything"));
    }

    #[test]
    fn unknown_scheme_is_refused() {
        assert!(!verify_password("$6$salt$whatever", b"anything"));
        assert!(!verify_password("!", b"anything"));
        assert!(!verify_password("$sha256$salt$", b"anything"));
    }

    /// A greeter that sends `XDG_SESSION_TYPE=wayland` twice must not put two
    /// entries in the session's environment, and a prefix must not alias.
    #[test]
    fn putenv_replaces_in_place_and_does_not_alias_prefixes() {
        let mut h = std::ptr::null_mut();
        unsafe {
            let svc = CString::new("greetd").unwrap();
            let usr = CString::new("root").unwrap();
            assert_eq!(pam_start(svc.as_ptr(), usr.as_ptr(), std::ptr::null(), &mut h), PAM_SUCCESS);

            for e in ["XDG_SESSION_TYPE=wayland", "XDG_SESSION_TYPE_EXTRA=x", "XDG_SESSION_TYPE=x11"] {
                let c = CString::new(e).unwrap();
                assert_eq!(pam_putenv(h, c.as_ptr()), PAM_SUCCESS);
            }

            let list = pam_getenvlist(h);
            assert!(!list.is_null());
            let mut got = Vec::new();
            let mut i = 0isize;
            while !(*list.offset(i)).is_null() {
                got.push(CStr::from_ptr(*list.offset(i)).to_str().unwrap().to_string());
                i += 1;
            }
            assert_eq!(got, vec!["XDG_SESSION_TYPE=x11", "XDG_SESSION_TYPE_EXTRA=x"]);
            assert_eq!(pam_misc_drop_env(list), PAM_SUCCESS);

            // A bare name deletes.
            let del = CString::new("XDG_SESSION_TYPE").unwrap();
            assert_eq!(pam_putenv(h, del.as_ptr()), PAM_SUCCESS);
            assert!(pam_getenv(h, del.as_ptr()).is_null());

            assert_eq!(pam_end(h, PAM_SUCCESS), PAM_SUCCESS);
        }
    }

    /// greetd fails the whole session if `pam_getenvlist` hands back NULL, so
    /// the empty case must still be a valid one-element array.
    #[test]
    fn getenvlist_is_never_null_when_empty() {
        let mut h = std::ptr::null_mut();
        unsafe {
            let svc = CString::new("greetd").unwrap();
            assert_eq!(pam_start(svc.as_ptr(), std::ptr::null(), std::ptr::null(), &mut h), PAM_SUCCESS);
            let list = pam_getenvlist(h);
            assert!(!list.is_null());
            assert!((*list).is_null());
            assert_eq!(pam_misc_drop_env(list), PAM_SUCCESS);
            assert_eq!(pam_end(h, PAM_SUCCESS), PAM_SUCCESS);
        }
    }
}
