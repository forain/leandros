//! Minimal zbus-based smoke test for a busd session bus (LeandrOS Wayland/COSMIC
//! S5 runtime roundtrip probe).
//!
//! What this proves (mirrors the M5 exit-criterion shape: "busd running; zbus
//! client owns a name"):
//!   1. Connection A connects to the bus, completes the Hello handshake
//!      (zbus does this automatically as part of `Builder::build()`), and
//!      owns the well-known name `org.leandros.Test`.
//!   2. Connection A exposes an object at `/org/leandros/Test` implementing
//!      `org.leandros.Test1.Ping`.
//!   3. Connection B (a second, independent connection to the same bus)
//!      calls that method by well-known name and gets the expected reply
//!      back, proving the broker actually routes messages between two
//!      distinct peers (not just a loopback/self-connection).
//!
//! Usage: zbus-test-client [bus-address]
//! If no address is given, falls back to $DBUS_SESSION_BUS_ADDRESS (the
//! same env var zbus's `Connection::session()` would read).

use std::env;

use anyhow::{anyhow, Context, Result};
use zbus::connection::Builder;

const WELL_KNOWN_NAME: &str = "org.leandros.Test";
const OBJECT_PATH: &str = "/org/leandros/Test";
const INTERFACE_NAME: &str = "org.leandros.Test1";

struct TestIface;

#[zbus::interface(interface = "org.leandros.Test1")]
impl TestIface {
    async fn ping(&self, msg: String) -> String {
        format!("pong:{msg}")
    }
}

fn bus_address() -> Result<String> {
    if let Some(addr) = env::args().nth(1) {
        return Ok(addr);
    }
    env::var("DBUS_SESSION_BUS_ADDRESS")
        .context("no bus address given as argv[1] and $DBUS_SESSION_BUS_ADDRESS is unset")
}

#[tokio::main]
async fn main() -> Result<()> {
    let address = bus_address()?;
    eprintln!("[zbus-test-client] connecting to `{address}` ...");

    // Connection A: owns the well-known name and serves the test object.
    let conn_a = Builder::address(address.as_str())?
        .serve_at(OBJECT_PATH, TestIface)?
        .name(WELL_KNOWN_NAME)?
        .build()
        .await
        .context("connection A: failed to connect/handshake/own name")?;
    eprintln!(
        "[zbus-test-client] connection A up: unique name = {}, owns `{}`",
        conn_a.unique_name().map(|n| n.as_str()).unwrap_or("<none>"),
        WELL_KNOWN_NAME
    );

    // Connection B: a fully independent second connection to the same bus.
    let conn_b = Builder::address(address.as_str())?
        .build()
        .await
        .context("connection B: failed to connect/handshake")?;
    eprintln!(
        "[zbus-test-client] connection B up: unique name = {}",
        conn_b.unique_name().map(|n| n.as_str()).unwrap_or("<none>")
    );

    if conn_a.unique_name() == conn_b.unique_name() {
        return Err(anyhow!(
            "connection A and B ended up with the same unique name — not independent peers"
        ));
    }

    // B calls A's Ping method by well-known name — this is the actual
    // broker-mediated round trip (message routed by busd from B to A).
    let reply = conn_b
        .call_method(
            Some(WELL_KNOWN_NAME),
            OBJECT_PATH,
            Some(INTERFACE_NAME),
            "Ping",
            &("hello-from-b",),
        )
        .await
        .context("method call B -> A via busd failed")?;

    let body: String = reply.body().deserialize().context("bad reply body")?;
    eprintln!("[zbus-test-client] reply body = {body:?}");

    if body != "pong:hello-from-b" {
        return Err(anyhow!("unexpected reply body: {body:?}"));
    }

    println!("ROUNDTRIP_OK");
    Ok(())
}
