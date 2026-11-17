//! net.hadess.SwitcherooControl: the GPU list COSMIC Settings and GTK apps
//! read to offer "launch with discrete GPU".
//!
//! LeandrOS has at most one render GPU per session, so HasDualGpu is false.
//! GPUs is built from /sys/class/drm/card*: Name (the driver, falling back
//! to the PCI id), Default (the first card), Environment (empty: no PRIME
//! offload variables apply) and Discrete (false). With no drm card the list
//! is empty and NumGPUs is 0.

use zbus::zvariant::{OwnedValue, Value};

const NAME: &str = "net.hadess.SwitcherooControl";
const PATH: &str = "/net/hadess/SwitcherooControl";

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub(crate) fn gpu_cards() -> Vec<String> {
    let mut cards: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("card") && n[4..].chars().all(|c| c.is_ascii_digit()) && n.len() > 4)
                .collect()
        })
        .unwrap_or_default();
    cards.sort_by_key(|c| c[4..].parse::<u32>().unwrap_or(0));
    cards
}

fn describe(card: &str, default: bool) -> std::collections::HashMap<String, OwnedValue> {
    let dev = format!("/sys/class/drm/{card}/device");
    let driver = std::fs::read_link(format!("{dev}/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let pci = match (read(&format!("{dev}/vendor")), read(&format!("{dev}/device"))) {
        (Some(v), Some(d)) => Some(format!("{v}:{d}")),
        _ => None,
    };
    let name = driver.or(pci).unwrap_or_else(|| card.to_string());
    let mut m = std::collections::HashMap::new();
    m.insert("Name".to_string(), OwnedValue::try_from(Value::from(name)).unwrap());
    m.insert("Environment".to_string(), OwnedValue::try_from(Value::from(Vec::<String>::new())).unwrap());
    m.insert("Default".to_string(), OwnedValue::try_from(Value::from(default)).unwrap());
    m.insert("Discrete".to_string(), OwnedValue::try_from(Value::from(false)).unwrap());
    m
}

struct Switcheroo;

#[zbus::interface(name = "net.hadess.SwitcherooControl")]
impl Switcheroo {
    #[zbus(property)]
    fn has_dual_gpu(&self) -> bool {
        false
    }
    #[zbus(property, name = "NumGPUs")]
    fn num_gpus(&self) -> u32 {
        gpu_cards().len() as u32
    }
    #[zbus(property, name = "GPUs")]
    fn gpus(&self) -> Vec<std::collections::HashMap<String, OwnedValue>> {
        gpu_cards().iter().enumerate().map(|(i, c)| describe(c, i == 0)).collect()
    }
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    builder.serve_at(PATH, Switcheroo)?.name(NAME)?.build().await
}
