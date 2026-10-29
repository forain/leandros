//! Minimal `busctl` for LeandrOS: only `busctl [--user|--system] [-q] call DEST PATH IFACE METHOD [SIG ARGS...]`,
//! the form COSMIC's system_actions uses. SIG may combine s, o, g, i, u, x, t, b, d, y, n, q.
use zbus::zvariant::{ObjectPath, Signature, StructureBuilder, Value};
use zbus::Connection;

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

fn parse_arg(c: char, s: &str) -> Result<Value<'static>, String> {
    let bad = |_| format!("Failed to parse '{s}' as '{c}'");
    Ok(match c {
        's' => Value::from(s.to_string()),
        'o' => Value::from(ObjectPath::try_from(s.to_string()).map_err(|e| e.to_string())?),
        'g' => Value::from(Signature::try_from(s.to_string()).map_err(|e| e.to_string())?),
        'b' => Value::from(match s {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => return Err(format!("Failed to parse '{s}' as 'b'")),
        }),
        'y' => Value::from(s.parse::<u8>().map_err(bad)?),
        'n' => Value::from(s.parse::<i16>().map_err(bad)?),
        'q' => Value::from(s.parse::<u16>().map_err(bad)?),
        'i' => Value::from(s.parse::<i32>().map_err(bad)?),
        'u' => Value::from(s.parse::<u32>().map_err(bad)?),
        'x' => Value::from(s.parse::<i64>().map_err(bad)?),
        't' => Value::from(s.parse::<u64>().map_err(bad)?),
        'd' => Value::from(s.parse::<f64>().map_err(bad)?),
        _ => return Err(format!("Unsupported signature character '{c}'")),
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut system = false;
    let mut quiet = false;
    let mut pos: Vec<String> = Vec::new();
    for a in std::env::args().skip(1) {
        if pos.is_empty() {
            match a.as_str() {
                "--user" => { system = false; continue; }
                "--system" => { system = true; continue; }
                "-q" | "--quiet" => { quiet = true; continue; }
                "--no-pager" | "--no-legend" | "--json=short" => continue,
                _ => {}
            }
        }
        pos.push(a);
    }
    if pos.first().map(String::as_str) != Some("call") || pos.len() < 5 {
        fail("Usage: busctl [--user|--system] [-q] call SERVICE OBJECT INTERFACE METHOD [SIGNATURE [ARGUMENT...]]");
    }
    let (dest, path, iface, method) = (&pos[1], &pos[2], &pos[3], &pos[4]);
    let sig = pos.get(5).map(String::as_str).unwrap_or("");
    let args = if pos.len() > 6 { &pos[6..] } else { &[][..] };
    if sig.chars().count() != args.len() {
        fail("Invalid number of arguments for signature.");
    }
    let conn = if system { Connection::system().await } else { Connection::session().await }
        .unwrap_or_else(|e| fail(&format!("Failed to connect to bus: {e}")));
    let reply = if sig.is_empty() {
        conn.call_method(Some(dest.as_str()), path.as_str(), Some(iface.as_str()), method.as_str(), &()).await
    } else {
        let mut b = StructureBuilder::new();
        for (c, s) in sig.chars().zip(args) {
            b = b.append_field(parse_arg(c, s).unwrap_or_else(|e| fail(&e)));
        }
        let body = b.build().unwrap_or_else(|e| fail(&e.to_string()));
        conn.call_method(Some(dest.as_str()), path.as_str(), Some(iface.as_str()), method.as_str(), &body).await
    };
    match reply {
        Ok(msg) => {
            if quiet {
                return;
            }
            let body = msg.body();
            let sig = body.signature().to_string();
            if sig.is_empty() {
                return;
            }
            match body.deserialize::<Value>() {
                Ok(v) => println!("{sig} {v:?}"),
                Err(_) => println!("{sig}"),
            }
        }
        Err(e) => fail(&format!("Call failed: {e}")),
    }
}
