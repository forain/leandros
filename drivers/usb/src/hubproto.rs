//! Hub protocol details that differ between USB 2.0 hubs and SuperSpeed
//! (USB 3.x) hubs, kept free of controller state so they run as host tests
//! (drivers/usb/msc-tests).
//!
//! USB 3.2 §10.15/§10.16 against USB 2.0 §11.23/§11.24:
//!   * Hub descriptor: type 0x29 (variable length, >= 7 bytes) vs 0x2A
//!     (12 bytes: bHubHdrDecLat, wHubDelay, 16-bit DeviceRemovable).
//!   * A SuperSpeed hub must learn its tier with SET_HUB_DEPTH (bRequest 12)
//!     before it can decode the route string of downstream packets.
//!   * wPortStatus: USB 2.0 has PORT_POWER at bit 8 and LOW/HIGH_SPEED at
//!     bits 9/10; SuperSpeed has the link state at bits 5..8, PORT_POWER at
//!     bit 9 and the connect speed at bits 10..12. Every device on a
//!     SuperSpeed hub's port is SuperSpeed (its USB 2.0 devices appear on the
//!     companion USB 2.0 hub instead).
//!   * wPortChange: SuperSpeed adds C_BH_PORT_RESET (bit 5, a warm reset
//!     finished), C_PORT_LINK_STATE (6) and C_PORT_CONFIG_ERROR (7).
//!   * No transaction translator on a SuperSpeed hub.

pub const DT_HUB: u8 = 0x29;
pub const DT_SS_HUB: u8 = 0x2A;
/// SET_HUB_DEPTH (USB 3.2 Table 10-8), bmRequestType 0x20 (class, device).
pub const REQ_SET_HUB_DEPTH: u8 = 12;
/// The route string has five 4-bit fields: hub ports 1..=15 only.
pub const ROUTE_MAX_PORT: u8 = 15;
/// Hub tiers the 20-bit route string allows below a root port.
pub const MAX_DEPTH: u8 = 5;

pub const PS_CONNECTION: u16 = 1 << 0;
pub const PS_ENABLE: u16 = 1 << 1;
const PS_LOW_SPEED: u16 = 1 << 9;
const PS_HIGH_SPEED: u16 = 1 << 10;

pub const PC_CONNECTION: u16 = 1 << 0;
pub const PC_RESET: u16 = 1 << 4;
pub const PC_BH_RESET: u16 = 1 << 5;

pub const FEAT_C_PORT_LINK_STATE: u16 = 25;
pub const FEAT_C_BH_PORT_RESET: u16 = 29;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildSpeed { Low, Full, High, Super }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubDesc {
    /// Downstream ports usable through the route string (<= 15).
    pub nports: u8,
    /// Ports the hub reports, before the route-string clamp.
    pub reported_ports: u8,
    pub characteristics: u16,
    /// Power-on to power-good wait, at least 100 ms.
    pub power_ms: u32,
}

/// GET_DESCRIPTOR(hub) wValue and length for a hub of this kind.
pub fn descriptor_request(superspeed: bool) -> (u16, usize) {
    if superspeed { ((DT_SS_HUB as u16) << 8, 12) } else { ((DT_HUB as u16) << 8, 9) }
}

/// Parse the class hub descriptor returned for `descriptor_request`.
pub fn parse_descriptor(superspeed: bool, d: &[u8]) -> Result<HubDesc, &'static str> {
    let (ty, min) = if superspeed { (DT_SS_HUB, 12) } else { (DT_HUB, 7) };
    if d.len() < min || (d[0] as usize) < min { return Err("short hub descriptor"); }
    if d[1] != ty { return Err("bad hub descriptor type"); }
    if d[2] == 0 { return Err("hub reports no ports"); }
    Ok(HubDesc {
        nports: d[2].min(ROUTE_MAX_PORT),
        reported_ports: d[2],
        characteristics: u16::from_le_bytes([d[3], d[4]]),
        power_ms: (d[5] as u32 * 2).max(100),
    })
}

/// Speed of the device on a hub port, from the port's wPortStatus.
pub fn child_speed(hub_superspeed: bool, status: u16) -> ChildSpeed {
    if hub_superspeed { ChildSpeed::Super }
    else if status & PS_LOW_SPEED != 0 { ChildSpeed::Low }
    else if status & PS_HIGH_SPEED != 0 { ChildSpeed::High }
    else { ChildSpeed::Full }
}

/// Whether a port reset finished, from wPortChange. A SuperSpeed hub may
/// escalate a hot reset to a warm (BH) reset and report that instead.
pub fn reset_done(hub_superspeed: bool, change: u16) -> bool {
    change & PC_RESET != 0 || (hub_superspeed && change & PC_BH_RESET != 0)
}

/// Tier of the hub whose own route string is `route` (0 = on a root port).
pub fn route_depth(route: u32) -> u8 {
    (0..5).take_while(|i| (route >> (4 * i)) & 0xF != 0).count() as u8
}

/// Route string of the device on `port` of a hub at `depth` with `route`.
/// None when the port cannot be expressed (port 0 or > 15, or too deep).
pub fn child_route(route: u32, depth: u8, port: u8) -> Option<u32> {
    if port == 0 || port > ROUTE_MAX_PORT || depth >= MAX_DEPTH { return None; }
    Some(route | ((port as u32) << (4 * depth as u32)))
}
