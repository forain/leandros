// Minimal real D-Bus client: connect to the session bus (sends Hello, awaits
// the unique-name reply). If busd never drives its per-peer socket_reader, this
// hangs in Connection::session() -> the watchdog reports W1 reproduced.
use std::io::Write;
use std::time::Duration;

fn mark(id: u64) { unsafe { libc::prctl(0x6d37c, id as libc::c_ulong, 0, 0, 0); } }

fn main() {
    let _ = writeln!(std::io::stderr(), "W1CLIENT: start");
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(8));
        let _ = writeln!(std::io::stderr(), "W1CLIENT: WATCHDOG — no Hello reply, W1 REPRODUCED");
        mark(911);
        std::process::exit(2);
    });
    mark(900);
    match zbus::blocking::Connection::session() {
        Ok(conn) => {
            mark(901);
            let _ = writeln!(std::io::stderr(), "W1CLIENT: CONNECTED unique_name={:?}", conn.unique_name());
            let _ = writeln!(std::io::stderr(), "W1CLIENT: SUCCESS");
            std::process::exit(0);
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "W1CLIENT: ERR {:?}", e);
            std::process::exit(3);
        }
    }
}
