//! org.freedesktop.locale1 (systemd-localed's interface), backed by the same
//! files localed uses:
//!
//!   /etc/locale.conf                       Locale  (LANG=..., LC_*=...)
//!   /etc/vconsole.conf                     VConsoleKeymap / VConsoleKeymapToggle
//!   /etc/X11/xorg.conf.d/00-keyboard.conf  X11Layout / X11Model / X11Variant / X11Options
//!
//! Consumers: cosmic-settings' Region & Language page (reads Locale, calls
//! SetLocale) and cosmic-settings-daemon, which mirrors the compositor's xkb
//! config to locale1 with SetX11Keyboard and back again on X11*Changed.
//!
//! Writes. This process runs as the session user (busd has no privilege
//! drop, see ports/dbus/session-pkg/session.conf), and /etc belongs to root.
//!   * SetLocale changes the system locale; when /etc/locale.conf cannot be
//!     written it fails with AccessDenied, as localed does for an
//!     unauthorised caller. Nothing is changed in that case.
//!   * SetX11Keyboard / SetVConsoleKeyboard take effect on the bus
//!     (properties + PropertiesChanged) and are persisted best-effort. The
//!     authoritative per-user keymap is cosmic-comp's own config; locale1 is
//!     its mirror. Failing here would only make cosmic-settings-daemon log an
//!     error on every keymap change.
//!
//! Defaults with no files: X11* are empty (localed reports "" when nothing is
//! configured, and cosmic-settings-daemon's empty XkbConfig matches that, so
//! the first sync is a no-op), and Locale is LANG=en_US.UTF-8, the language
//! the COSMIC UI falls back to on an image that sets no LANG.

use std::collections::BTreeMap;
use std::sync::Mutex;

use zbus::object_server::SignalEmitter;

const NAME: &str = "org.freedesktop.locale1";
const PATH: &str = "/org/freedesktop/locale1";
const LOCALE_CONF: &str = "/etc/locale.conf";
const VCONSOLE_CONF: &str = "/etc/vconsole.conf";
const X11_CONF: &str = "/etc/X11/xorg.conf.d/00-keyboard.conf";
const DEFAULT_LOCALE: &str = "LANG=en_US.UTF-8";

/// The locale variables localed accepts, in its order.
const LOCALE_KEYS: &[&str] = &[
    "LANG", "LANGUAGE", "LC_CTYPE", "LC_NUMERIC", "LC_TIME", "LC_COLLATE", "LC_MONETARY",
    "LC_MESSAGES", "LC_PAPER", "LC_NAME", "LC_ADDRESS", "LC_TELEPHONE", "LC_MEASUREMENT",
    "LC_IDENTIFICATION",
];

#[derive(Default, Clone)]
struct State {
    locale: Vec<String>,
    vc_keymap: String,
    vc_keymap_toggle: String,
    x11_layout: String,
    x11_model: String,
    x11_variant: String,
    x11_options: String,
}

struct Locale1 {
    state: Mutex<State>,
}

fn log(msg: std::fmt::Arguments<'_>) {
    crate::log("locale1", msg)
}

/// Log a failed best-effort write once per file, not on every keymap change.
fn log_not_persisted(what: &str, path: &str, e: &std::io::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static VC: AtomicBool = AtomicBool::new(false);
    static X11: AtomicBool = AtomicBool::new(false);
    let flag = if path == VCONSOLE_CONF { &VC } else { &X11 };
    if !flag.swap(true, Ordering::Relaxed) {
        log(format_args!("{what}: kept in memory, not persisted ({path}: {e}); further failures not logged"));
    }
}

/// KEY=VALUE lines, `#` comments, optional double or single quotes.
fn parse_env_file(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            out.insert(k.trim().to_owned(), v.to_owned());
        }
    }
    out
}

/// `Option "XkbLayout" "us"` lines from an xorg.conf.d InputClass section.
fn parse_x11_conf(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('"').collect();
        // Option "Key" "Value"  ->  ["Option ", "Key", " ", "Value", ""]
        if parts.len() >= 4 && parts[0].trim() == "Option" {
            out.insert(parts[1].to_owned(), parts[3].to_owned());
        }
    }
    out
}

fn load() -> State {
    let mut st = State::default();
    if let Ok(text) = std::fs::read_to_string(LOCALE_CONF) {
        let vars = parse_env_file(&text);
        for key in LOCALE_KEYS {
            if let Some(v) = vars.get(*key).filter(|v| !v.is_empty()) {
                st.locale.push(format!("{key}={v}"));
            }
        }
    }
    if st.locale.is_empty() {
        st.locale.push(DEFAULT_LOCALE.to_owned());
    }
    if let Ok(text) = std::fs::read_to_string(VCONSOLE_CONF) {
        let vars = parse_env_file(&text);
        st.vc_keymap = vars.get("KEYMAP").cloned().unwrap_or_default();
        st.vc_keymap_toggle = vars.get("KEYMAP_TOGGLE").cloned().unwrap_or_default();
    }
    if let Ok(text) = std::fs::read_to_string(X11_CONF) {
        let opts = parse_x11_conf(&text);
        st.x11_layout = opts.get("XkbLayout").cloned().unwrap_or_default();
        st.x11_model = opts.get("XkbModel").cloned().unwrap_or_default();
        st.x11_variant = opts.get("XkbVariant").cloned().unwrap_or_default();
        st.x11_options = opts.get("XkbOptions").cloned().unwrap_or_default();
    }
    st
}

/// Replace `path` atomically (temp file + rename in the same directory).
fn write_atomic(path: &str, contents: &str) -> std::io::Result<()> {
    let p = std::path::Path::new(path);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = format!("{path}.tmp-{}", std::process::id());
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn x11_conf_text(st: &State) -> String {
    let mut s = String::from(
        "# Written by leandros-sysbus locale1 (org.freedesktop.locale1 SetX11Keyboard).\n\
         Section \"InputClass\"\n        Identifier \"system-keyboard\"\n        \
         MatchIsKeyboard \"on\"\n",
    );
    for (k, v) in [
        ("XkbLayout", &st.x11_layout),
        ("XkbModel", &st.x11_model),
        ("XkbVariant", &st.x11_variant),
        ("XkbOptions", &st.x11_options),
    ] {
        if !v.is_empty() {
            s.push_str(&format!("        Option \"{k}\" \"{v}\"\n"));
        }
    }
    s.push_str("EndSection\n");
    s
}

#[zbus::interface(name = "org.freedesktop.locale1")]
impl Locale1 {
    async fn set_locale(
        &self,
        locale: Vec<String>,
        _interactive: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let mut assignments = Vec::new();
        for entry in &locale {
            let Some((k, v)) = entry.split_once('=') else {
                return Err(zbus::fdo::Error::InvalidArgs(format!("not KEY=VALUE: {entry:?}")));
            };
            if !LOCALE_KEYS.contains(&k) {
                return Err(zbus::fdo::Error::InvalidArgs(format!("unknown locale variable {k:?}")));
            }
            if v.is_empty() || v.contains(['\n', '"', '\'']) {
                return Err(zbus::fdo::Error::InvalidArgs(format!("bad value for {k}: {v:?}")));
            }
            assignments.push(entry.clone());
        }
        if assignments.is_empty() {
            assignments.push(DEFAULT_LOCALE.to_owned());
        }
        let mut text = String::from("# Written by leandros-sysbus locale1 (org.freedesktop.locale1 SetLocale).\n");
        for a in &assignments {
            text.push_str(a);
            text.push('\n');
        }
        if let Err(e) = write_atomic(LOCALE_CONF, &text) {
            log(format_args!("SetLocale {assignments:?}: {LOCALE_CONF}: {e}"));
            return Err(match e.kind() {
                std::io::ErrorKind::PermissionDenied => zbus::fdo::Error::AccessDenied(format!(
                    "cannot write {LOCALE_CONF}: {e} (the system locale belongs to root)"
                )),
                _ => zbus::fdo::Error::Failed(format!("cannot write {LOCALE_CONF}: {e}")),
            });
        }
        let changed = {
            let mut st = self.state.lock().unwrap();
            let changed = st.locale != assignments;
            st.locale = assignments;
            changed
        };
        if changed {
            let _ = self.locale_changed(&emitter).await;
        }
        Ok(())
    }

    #[zbus(name = "SetVConsoleKeyboard")]
    async fn set_vconsole_keyboard(
        &self,
        keymap: String,
        keymap_toggle: String,
        _convert: bool,
        _interactive: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let (km, kt) = {
            let mut st = self.state.lock().unwrap();
            let r = (st.vc_keymap != keymap, st.vc_keymap_toggle != keymap_toggle);
            st.vc_keymap = keymap.clone();
            st.vc_keymap_toggle = keymap_toggle.clone();
            r
        };
        let mut text = String::new();
        if !keymap.is_empty() {
            text.push_str(&format!("KEYMAP={keymap}\n"));
        }
        if !keymap_toggle.is_empty() {
            text.push_str(&format!("KEYMAP_TOGGLE={keymap_toggle}\n"));
        }
        if let Err(e) = write_atomic(VCONSOLE_CONF, &text) {
            log_not_persisted("SetVConsoleKeyboard", VCONSOLE_CONF, &e);
        }
        if km {
            let _ = self.v_console_keymap_changed(&emitter).await;
        }
        if kt {
            let _ = self.v_console_keymap_toggle_changed(&emitter).await;
        }
        Ok(())
    }

    #[zbus(name = "SetX11Keyboard")]
    #[allow(clippy::too_many_arguments)]
    async fn set_x11_keyboard(
        &self,
        layout: String,
        model: String,
        variant: String,
        options: String,
        _convert: bool,
        _interactive: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        for v in [&layout, &model, &variant, &options] {
            if v.contains(['\n', '"']) {
                return Err(zbus::fdo::Error::InvalidArgs(format!("bad xkb value {v:?}")));
            }
        }
        let (snapshot, l, m, v, o) = {
            let mut st = self.state.lock().unwrap();
            let r = (
                st.x11_layout != layout,
                st.x11_model != model,
                st.x11_variant != variant,
                st.x11_options != options,
            );
            st.x11_layout = layout;
            st.x11_model = model;
            st.x11_variant = variant;
            st.x11_options = options;
            (st.clone(), r.0, r.1, r.2, r.3)
        };
        if !(l || m || v || o) {
            return Ok(());
        }
        if let Err(e) = write_atomic(X11_CONF, &x11_conf_text(&snapshot)) {
            log_not_persisted("SetX11Keyboard", X11_CONF, &e);
        }
        if l {
            let _ = self.x11_layout_changed(&emitter).await;
        }
        if m {
            let _ = self.x11_model_changed(&emitter).await;
        }
        if v {
            let _ = self.x11_variant_changed(&emitter).await;
        }
        if o {
            let _ = self.x11_options_changed(&emitter).await;
        }
        Ok(())
    }

    #[zbus(property)]
    fn locale(&self) -> Vec<String> {
        self.state.lock().unwrap().locale.clone()
    }

    #[zbus(property, name = "VConsoleKeymap")]
    fn vconsole_keymap(&self) -> String {
        self.state.lock().unwrap().vc_keymap.clone()
    }

    #[zbus(property, name = "VConsoleKeymapToggle")]
    fn vconsole_keymap_toggle(&self) -> String {
        self.state.lock().unwrap().vc_keymap_toggle.clone()
    }

    #[zbus(property, name = "X11Layout")]
    fn x11_layout(&self) -> String {
        self.state.lock().unwrap().x11_layout.clone()
    }

    #[zbus(property, name = "X11Model")]
    fn x11_model(&self) -> String {
        self.state.lock().unwrap().x11_model.clone()
    }

    #[zbus(property, name = "X11Variant")]
    fn x11_variant(&self) -> String {
        self.state.lock().unwrap().x11_variant.clone()
    }

    #[zbus(property, name = "X11Options")]
    fn x11_options(&self) -> String {
        self.state.lock().unwrap().x11_options.clone()
    }
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    let conn = builder
        .serve_at(PATH, Locale1 { state: Mutex::new(load()) })?
        .name(NAME)?
        .build()
        .await?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file() {
        let m = parse_env_file("# c\nLANG=\"de_DE.UTF-8\"\nLC_TIME='en_GB.UTF-8'\n\nKEYMAP=us\n");
        assert_eq!(m["LANG"], "de_DE.UTF-8");
        assert_eq!(m["LC_TIME"], "en_GB.UTF-8");
        assert_eq!(m["KEYMAP"], "us");
    }

    #[test]
    fn x11_round_trip() {
        let st = State {
            x11_layout: "us,de".into(),
            x11_options: "grp:alt_shift_toggle".into(),
            ..Default::default()
        };
        let m = parse_x11_conf(&x11_conf_text(&st));
        assert_eq!(m["XkbLayout"], "us,de");
        assert_eq!(m["XkbOptions"], "grp:alt_shift_toggle");
        assert!(!m.contains_key("XkbModel"));
    }
}
