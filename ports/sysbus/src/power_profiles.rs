//! org.freedesktop.UPower.PowerProfiles (power-profiles-daemon), as ppd
//! behaves on a machine with no platform_profile and no CPU driver it knows:
//! the "placeholder" driver, offering power-saver and balanced. Choosing one
//! records it and changes nothing in hardware, which is what ppd does there.
//! "performance" is not offered and SetActiveProfile rejects it, as ppd does.
//!
//! Claimed under both bus names ppd uses (the legacy net.hadess.PowerProfiles
//! and org.freedesktop.UPower.PowerProfiles), each with its own object path.
//! cosmic-settings' Power & battery page needs this, or it shows
//! "Backend not found. Install system76-power or power-profiles-daemon."
//!
//! The choice lives for the session only: ppd keeps it in /var/lib, which the
//! session user cannot write.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

const PROFILES: &[&str] = &["power-saver", "balanced"];

struct State {
    active: String,
    holds: Vec<(u32, String, String, String)>,
    next_cookie: u32,
}

#[derive(Clone)]
struct PowerProfiles {
    st: Arc<Mutex<State>>,
}

fn dict(pairs: &[(&str, &str)]) -> HashMap<String, OwnedValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), OwnedValue::try_from(Value::from(*v)).expect("string value")))
        .collect()
}

#[zbus::interface(name = "org.freedesktop.UPower.PowerProfiles")]
impl PowerProfiles {
    async fn hold_profile(
        &self,
        profile: String,
        reason: String,
        application_id: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<u32> {
        if profile != "power-saver" && profile != "performance" {
            return Err(zbus::fdo::Error::InvalidArgs(format!("only power-saver or performance can be held, not {profile:?}")));
        }
        if !PROFILES.contains(&profile.as_str()) {
            return Err(zbus::fdo::Error::NotSupported(format!("profile {profile:?} is not available")));
        }
        let cookie = {
            let mut st = self.st.lock().unwrap();
            st.next_cookie += 1;
            let c = st.next_cookie;
            st.holds.push((c, profile.clone(), reason, application_id));
            st.active = profile;
            c
        };
        let _ = self.active_profile_changed(&emitter).await;
        let _ = self.active_profile_holds_changed(&emitter).await;
        Ok(cookie)
    }

    async fn release_profile(
        &self,
        cookie: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        {
            let mut st = self.st.lock().unwrap();
            let before = st.holds.len();
            st.holds.retain(|h| h.0 != cookie);
            if st.holds.len() == before {
                return Err(zbus::fdo::Error::InvalidArgs(format!("no hold with cookie {cookie}")));
            }
            if st.holds.is_empty() {
                st.active = "balanced".into();
            }
        }
        let _ = Self::profile_released(&emitter, cookie).await;
        let _ = self.active_profile_changed(&emitter).await;
        let _ = self.active_profile_holds_changed(&emitter).await;
        Ok(())
    }

    #[zbus(signal)]
    async fn profile_released(emitter: &SignalEmitter<'_>, cookie: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn active_profile(&self) -> String {
        self.st.lock().unwrap().active.clone()
    }

    #[zbus(property)]
    fn set_active_profile(&mut self, value: String) -> zbus::fdo::Result<()> {
        if !PROFILES.contains(&value.as_str()) {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "profile {value:?} is not available (this machine offers {PROFILES:?})"
            )));
        }
        let mut st = self.st.lock().unwrap();
        st.holds.clear();
        st.active = value;
        Ok(())
    }

    #[zbus(property)]
    fn profiles(&self) -> Vec<HashMap<String, OwnedValue>> {
        PROFILES
            .iter()
            .map(|p| dict(&[("Profile", p), ("Driver", "placeholder"), ("PlatformDriver", "placeholder")]))
            .collect()
    }

    #[zbus(property)]
    fn actions(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn performance_degraded(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn performance_inhibited(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn active_profile_holds(&self) -> Vec<HashMap<String, OwnedValue>> {
        self.st
            .lock()
            .unwrap()
            .holds
            .iter()
            .map(|(_, p, r, a)| dict(&[("Profile", p), ("Reason", r), ("ApplicationId", a)]))
            .collect()
    }

    #[zbus(property)]
    fn version(&self) -> &str {
        "0.30"
    }
}

/// Same object, legacy interface name, served at the legacy path.
#[derive(Clone)]
struct LegacyPowerProfiles(PowerProfiles);

#[zbus::interface(name = "net.hadess.PowerProfiles")]
impl LegacyPowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> String {
        self.0.active_profile()
    }
    #[zbus(property)]
    fn set_active_profile(&mut self, value: String) -> zbus::fdo::Result<()> {
        self.0.set_active_profile(value)
    }
    #[zbus(property)]
    fn profiles(&self) -> Vec<HashMap<String, OwnedValue>> {
        self.0.profiles()
    }
    #[zbus(property)]
    fn actions(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn performance_degraded(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn performance_inhibited(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn active_profile_holds(&self) -> Vec<HashMap<String, OwnedValue>> {
        self.0.active_profile_holds()
    }
    #[zbus(property)]
    fn version(&self) -> &str {
        "0.30"
    }
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    let pp = PowerProfiles {
        st: Arc::new(Mutex::new(State { active: "balanced".into(), holds: Vec::new(), next_cookie: 0 })),
    };
    builder
        .serve_at("/org/freedesktop/UPower/PowerProfiles", pp.clone())?
        .serve_at("/net/hadess/PowerProfiles", LegacyPowerProfiles(pp))?
        .name("org.freedesktop.UPower.PowerProfiles")?
        .name("net.hadess.PowerProfiles")?
        .build()
        .await
}
