//! NM connection settings (`a{sa{sv}}`) <-> netctl profiles.
//!
//! GetSettings never carries secrets (NM omits them; GetSecrets is separate).
//! Only Wi-Fi profiles are stored in the kernel's slots; the wired profile is
//! synthetic. `key-mgmt` "wpa-psk" means "PSK, or SAE when both sides can" —
//! nmrs sends wpa-psk even for WPA3 APs — and is stored as akm AUTO.

use std::collections::HashMap;

use netctl_abi::profiles::Profile;
use netctl_abi::{akm, profile_flags, NcProfileMeta, IFNAMSIZ, PROFILE_ID_MAX, SECRET_MAX, SSID_MAX};
use zbus::zvariant::{OwnedValue, Value};

use super::model::{ETH_TYPE, WIFI_TYPE};
use super::NmError;

pub type Settings = HashMap<String, HashMap<String, OwnedValue>>;

pub fn ov<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("value without fds")
}

fn val<'s>(s: &'s Settings, sec: &str, key: &str) -> Option<&'s OwnedValue> {
    s.get(sec)?.get(key)
}

pub fn get_str(s: &Settings, sec: &str, key: &str) -> Option<String> {
    val(s, sec, key).and_then(|v| String::try_from(v.clone()).ok())
}
pub fn get_bytes(s: &Settings, sec: &str, key: &str) -> Option<Vec<u8>> {
    val(s, sec, key).and_then(|v| Vec::<u8>::try_from(v.clone()).ok())
}
pub fn get_bool(s: &Settings, sec: &str, key: &str) -> Option<bool> {
    val(s, sec, key).and_then(|v| bool::try_from(v.clone()).ok())
}
pub fn get_i32(s: &Settings, sec: &str, key: &str) -> Option<i32> {
    let v = val(s, sec, key)?;
    i32::try_from(v.clone()).ok().or_else(|| u32::try_from(v.clone()).ok().map(|u| u as i32))
}

fn section(pairs: Vec<(&str, OwnedValue)>) -> HashMap<String, OwnedValue> {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

pub fn wired_settings(id: &str, uuid: &str, ifname: &str) -> Settings {
    let mut s = Settings::new();
    s.insert(
        "connection".into(),
        section(vec![
            ("id", ov(id)),
            ("uuid", ov(uuid)),
            ("type", ov(ETH_TYPE)),
            ("interface-name", ov(ifname)),
            ("autoconnect", ov(true)),
            ("autoconnect-priority", ov(-999i32)),
            ("timestamp", ov(0u64)),
            ("permissions", ov(Vec::<String>::new())),
        ]),
    );
    s.insert("802-3-ethernet".into(), section(vec![("auto-negotiate", ov(true))]));
    s.insert("ipv4".into(), section(vec![("method", ov("auto"))]));
    s.insert("ipv6".into(), section(vec![("method", ov("ignore"))]));
    s
}

/// Secret-free settings of a kernel profile.
pub fn settings_from_meta(m: &NcProfileMeta, uuid: &str) -> Settings {
    let id = String::from_utf8_lossy(if m.id().is_empty() { m.ssid() } else { m.id() }).into_owned();
    let mut conn = vec![
        ("id", ov(id)),
        ("uuid", ov(uuid)),
        ("type", ov(WIFI_TYPE)),
        ("autoconnect", ov(m.autoconnect != 0)),
        ("autoconnect-priority", ov(m.priority)),
        ("timestamp", ov(m.timestamp_s)),
        ("permissions", ov(Vec::<String>::new())),
    ];
    if !m.iface().is_empty() {
        conn.push(("interface-name", ov(String::from_utf8_lossy(m.iface()).into_owned())));
    }
    let mut s = Settings::new();
    s.insert("connection".into(), section(conn));
    s.insert(
        "802-11-wireless".into(),
        section(vec![
            ("ssid", ov(m.ssid().to_vec())),
            ("mode", ov("infrastructure")),
            ("hidden", ov(m.flags & profile_flags::HIDDEN != 0)),
        ]),
    );
    if m.akm != akm::NONE {
        let km = if m.akm == akm::SAE { "sae" } else { "wpa-psk" };
        s.insert(
            "802-11-wireless-security".into(),
            section(vec![("key-mgmt", ov(km)), ("psk-flags", ov(0u32)), ("pmf", ov(m.pmf as i32))]),
        );
    }
    s.insert("ipv4".into(), section(vec![("method", ov("auto"))]));
    s.insert("ipv6".into(), section(vec![("method", ov("ignore"))]));
    s
}

pub fn secret_free(p: &Profile, uuid: &str) -> Settings {
    settings_from_meta(&p.meta(0, 0), uuid)
}

/// Drop secret keys (the overlay of an unsaved Update is returned by
/// GetSettings, which never carries secrets).
pub fn strip_secrets(s: &Settings) -> Settings {
    let mut s = s.clone();
    for sec in s.values_mut() {
        for k in ["psk", "wep-key0", "wep-key1", "wep-key2", "wep-key3", "password", "leap-password"] {
            sec.remove(k);
        }
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Wifi,
    Ethernet,
}

/// The parts of an NM settings dict the shim understands.
#[derive(Default)]
pub struct Parsed {
    pub kind: Option<Kind>,
    pub uuid: Option<[u8; 16]>,
    pub id: Option<String>,
    pub ssid: Option<Vec<u8>>,
    pub hidden: Option<bool>,
    pub key_mgmt: Option<String>,
    pub has_security: bool,
    pub psk: Option<Vec<u8>>,
    pub autoconnect: Option<bool>,
    pub priority: Option<i32>,
    pub iface: Option<String>,
    pub pmf: Option<i32>,
}

impl Drop for Parsed {
    fn drop(&mut self) {
        if let Some(p) = self.psk.as_mut() {
            p.iter_mut().for_each(|b| *b = 0);
        }
    }
}

pub fn parse(s: &Settings) -> Result<Parsed, NmError> {
    let mut p = Parsed::default();
    p.kind = match get_str(s, "connection", "type").as_deref() {
        Some(WIFI_TYPE) | Some("wifi") => Some(Kind::Wifi),
        Some(ETH_TYPE) | Some("ethernet") => Some(Kind::Ethernet),
        Some(other) => {
            return Err(NmError::InvalidConnection(format!("connection type {other:?} is not supported on LeandrOS")))
        }
        None if s.contains_key("802-11-wireless") => Some(Kind::Wifi),
        None if s.contains_key("802-3-ethernet") => Some(Kind::Ethernet),
        None => None,
    };
    p.uuid = get_str(s, "connection", "uuid").and_then(|u| netctl_abi::util::uuid_from_str(u.as_bytes()));
    p.id = get_str(s, "connection", "id").filter(|i| !i.is_empty());
    p.ssid = get_bytes(s, "802-11-wireless", "ssid").filter(|v| !v.is_empty());
    if let Some(ssid) = &p.ssid {
        if ssid.len() > SSID_MAX {
            return Err(NmError::InvalidConnection("SSID longer than 32 bytes".into()));
        }
    }
    p.hidden = get_bool(s, "802-11-wireless", "hidden");
    p.has_security = s.contains_key("802-11-wireless-security");
    p.key_mgmt = get_str(s, "802-11-wireless-security", "key-mgmt");
    p.psk = get_str(s, "802-11-wireless-security", "psk").map(String::into_bytes);
    p.autoconnect = get_bool(s, "connection", "autoconnect");
    p.priority = get_i32(s, "connection", "autoconnect-priority");
    p.iface = get_str(s, "connection", "interface-name");
    p.pmf = get_i32(s, "802-11-wireless-security", "pmf");
    if s.contains_key("802-1x") {
        return Err(NmError::InvalidConnection("802.1X (EAP) networks are not supported on LeandrOS yet".into()));
    }
    match p.key_mgmt.as_deref() {
        None | Some("wpa-psk") | Some("sae") | Some("wpa-psk-sha256") => {}
        Some("none") if !p.has_security || get_str(s, "802-11-wireless-security", "wep-key0").is_none() => {}
        Some(other) => {
            return Err(NmError::InvalidConnection(format!("key-mgmt {other:?} is not supported on LeandrOS")))
        }
    }
    Ok(p)
}

impl Parsed {
    pub fn akm(&self) -> u8 {
        match self.key_mgmt.as_deref() {
            // "sae" is stored as AUTO too: AUTO already means "SAE when the
            // AP and the kernel can", and SAE-only would fail until WPA3.
            Some("wpa-psk") | Some("sae") | Some("wpa-psk-sha256") => akm::AUTO,
            _ => akm::NONE,
        }
    }
}

/// Build the profile to store: `p` over `existing` (which keeps its uuid,
/// timestamp and — when `p` carries no psk — its secret).
pub fn to_profile(p: &Parsed, uuid: [u8; 16], ssid: &[u8], existing: Option<&Profile>) -> Result<Profile, NmError> {
    let akm_ = if p.key_mgmt.is_some() || p.has_security { p.akm() } else { existing.map(|e| e.akm).unwrap_or(akm::NONE) };
    let secret: Vec<u8> = match (&p.psk, existing) {
        (Some(psk), _) => psk.clone(),
        (None, Some(e)) if akm_ != akm::NONE => e.secret().to_vec(),
        _ => Vec::new(),
    };
    if akm_ == akm::NONE && !secret.is_empty() {
        return Err(NmError::InvalidConnection("a psk without key-mgmt".into()));
    }
    if secret.len() > SECRET_MAX {
        return Err(NmError::InvalidConnection("psk too long".into()));
    }
    let mut prof = Profile::new_wifi(uuid, ssid, akm_, &secret)
        .ok_or_else(|| NmError::InvalidConnection("invalid SSID or psk".into()))?;
    let mut secret = secret;
    secret.iter_mut().for_each(|b| *b = 0);
    let id = p
        .id
        .clone()
        .or_else(|| existing.map(|e| String::from_utf8_lossy(e.id()).into_owned()))
        .unwrap_or_else(|| String::from_utf8_lossy(ssid).into_owned());
    prof.set_id(&id.as_bytes()[..id.len().min(PROFILE_ID_MAX)]);
    prof.autoconnect = p.autoconnect.or(existing.map(|e| e.autoconnect)).unwrap_or(true);
    prof.priority = p.priority.or(existing.map(|e| e.priority)).unwrap_or(0);
    prof.timestamp_s = existing.map(|e| e.timestamp_s).unwrap_or(0);
    let hidden = p.hidden.or(existing.map(|e| e.hidden())).unwrap_or(false);
    prof.flags = if hidden { profile_flags::HIDDEN } else { 0 };
    prof.pmf = p.pmf.map(|v| v.clamp(0, 3) as u8).or(existing.map(|e| e.pmf)).unwrap_or(0);
    let iface = p.iface.clone().unwrap_or_else(|| existing.map(|e| String::from_utf8_lossy(e.iface()).into_owned()).unwrap_or_default());
    if iface.len() < IFNAMSIZ {
        netctl_abi::util::set_cstr(&mut prof.iface, iface.as_bytes());
    }
    prof.validate().map_err(|e| NmError::InvalidConnection(format!("profile rejected: {e:?}")))?;
    Ok(prof)
}

/// A fresh random (version 4) uuid.
pub fn random_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut u)).is_ok();
    if !ok {
        let (wall, mono) = crate::now_usec();
        u[..8].copy_from_slice(&wall.to_le_bytes());
        u[8..].copy_from_slice(&(mono ^ std::process::id() as u64).to_le_bytes());
    }
    u[6] = (u[6] & 0x0F) | 0x40;
    u[8] = (u[8] & 0x3F) | 0x80;
    u
}
