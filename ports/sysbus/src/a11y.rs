//! org.a11y.Bus: the accessibility bus rendezvous.
//!
//! GTK, libcosmic (via atspi) and Qt call `org.a11y.Bus.GetAddress` on the
//! session bus at start-up to find the AT-SPI bus; with no owner they wait
//! for the 25 s activation timeout first. LeandrOS ships no at-spi registry
//! and no screen reader, so there is nothing to put on a private bus: the
//! service answers GetAddress with the session bus itself, which lets the
//! toolkits connect immediately and find no registry (they then run without
//! accessibility, which is the truth). org.a11y.Status reports IsEnabled and
//! ScreenReaderEnabled false; setting them is accepted and remembered for
//! the life of the service.

use std::sync::atomic::{AtomicBool, Ordering};

const NAME: &str = "org.a11y.Bus";
const PATH: &str = "/org/a11y/bus";

fn session_address(own: &str) -> String {
    std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| own.to_string())
}

struct Bus {
    own_address: String,
}

#[zbus::interface(name = "org.a11y.Bus")]
impl Bus {
    fn get_address(&self) -> String {
        session_address(&self.own_address)
    }
}

struct Status {
    enabled: AtomicBool,
    screen_reader: AtomicBool,
}

#[zbus::interface(name = "org.a11y.Status")]
impl Status {
    #[zbus(property)]
    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    #[zbus(property)]
    fn set_is_enabled(&self, v: bool) {
        self.enabled.store(v, Ordering::Relaxed);
    }
    #[zbus(property)]
    fn screen_reader_enabled(&self) -> bool {
        self.screen_reader.load(Ordering::Relaxed)
    }
    #[zbus(property)]
    fn set_screen_reader_enabled(&self, v: bool) {
        self.screen_reader.store(v, Ordering::Relaxed);
    }
}

pub async fn serve(builder: zbus::connection::Builder<'_>, own_address: &str) -> zbus::Result<zbus::Connection> {
    builder
        .serve_at(PATH, Bus { own_address: own_address.to_string() })?
        .serve_at(PATH, Status { enabled: AtomicBool::new(false), screen_reader: AtomicBool::new(false) })?
        .name(NAME)?
        .build()
        .await
}
