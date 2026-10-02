//! org.freedesktop.PolicyKit1 Authority: an authority with no rules and no
//! way to authenticate anyone yet.
//!
//! cosmic-osd registers itself as the session's polkit authentication agent
//! at startup (after asking login1 for its session id). With no authority on
//! the bus that registration failed and was logged on every login. Here it
//! succeeds and is recorded.
//!
//! Authorization answers are the conservative polkit answer for "no rule
//! grants this": a subject running as uid 0 is authorized (polkit always
//! authorizes root), and anyone else is not authorized and not offered a
//! challenge. A challenge would need polkit-agent-helper-1 to verify a
//! password, and LeandrOS does not ship one. So no caller is ever granted
//! more than its uid already has.

use std::collections::HashMap;
use std::sync::Mutex;

use zbus::zvariant::OwnedValue;

const NAME: &str = "org.freedesktop.PolicyKit1";
const PATH: &str = "/org/freedesktop/PolicyKit1/Authority";

type Subject = (String, HashMap<String, OwnedValue>);

fn log(msg: std::fmt::Arguments<'_>) {
    crate::log("polkit", msg)
}

/// The uid a polkit subject runs as, when it can be determined.
fn subject_uid(subject: &Subject) -> Option<u32> {
    let get_u32 = |k: &str| -> Option<u32> {
        let v = subject.1.get(k)?;
        u32::try_from(v.clone()).ok().or_else(|| i32::try_from(v.clone()).ok().map(|x| x as u32))
    };
    match subject.0.as_str() {
        "unix-user" => get_u32("uid"),
        "unix-process" => {
            if let Some(uid) = get_u32("uid").filter(|u| *u != u32::MAX) {
                return Some(uid);
            }
            let pid = get_u32("pid")?;
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            status
                .lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|r| r.split_whitespace().next())
                .and_then(|u| u.parse().ok())
        }
        // Our single session (login1 c1) belongs to the user we run as.
        // SAFETY: plain libc getter.
        "unix-session" => Some(unsafe { libc::getuid() }),
        _ => None,
    }
}

struct Authority {
    agents: Mutex<Vec<(Subject, String, String)>>,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl Authority {
    fn register_authentication_agent(
        &self,
        subject: Subject,
        locale: String,
        object_path: String,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        log(format_args!("agent registered: {sender} {object_path} ({} {locale})", subject.0));
        let mut agents = self.agents.lock().unwrap();
        agents.retain(|a| a.2 != sender);
        agents.push((subject, object_path, sender));
    }

    fn register_authentication_agent_with_options(
        &self,
        subject: Subject,
        locale: String,
        object_path: String,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) {
        self.register_authentication_agent(subject, locale, object_path, hdr)
    }

    fn unregister_authentication_agent(
        &self,
        _subject: Subject,
        object_path: String,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        self.agents
            .lock()
            .unwrap()
            .retain(|a| !(a.1 == object_path && a.2 == sender));
    }

    fn check_authorization(
        &self,
        subject: Subject,
        action_id: String,
        _details: HashMap<String, String>,
        _flags: u32,
        _cancellation_id: String,
    ) -> (bool, bool, HashMap<String, String>) {
        let uid = subject_uid(&subject);
        let authorized = uid == Some(0);
        log(format_args!(
            "CheckAuthorization {action_id} subject={} uid={uid:?} -> {}",
            subject.0,
            if authorized { "yes" } else { "no" }
        ));
        (authorized, false, HashMap::new())
    }

    fn cancel_check_authorization(&self, _cancellation_id: String) {}

    #[allow(clippy::type_complexity)]
    fn enumerate_actions(
        &self,
        _locale: String,
    ) -> Vec<(String, String, String, String, String, String, u32, u32, u32, HashMap<String, String>)> {
        Vec::new()
    }

    fn authentication_agent_response(&self, _cookie: String, _identity: Subject) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported("no authentication helper on LeandrOS".into()))
    }

    fn authentication_agent_response2(&self, _uid: u32, _cookie: String, _identity: Subject) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported("no authentication helper on LeandrOS".into()))
    }

    #[zbus(property)]
    fn backend_name(&self) -> &str {
        "leandros-sysbus"
    }

    #[zbus(property)]
    fn backend_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    #[zbus(property)]
    fn backend_features(&self) -> u32 {
        0
    }
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    builder
        .serve_at(PATH, Authority { agents: Mutex::new(Vec::new()) })?
        .name(NAME)?
        .build()
        .await
}
