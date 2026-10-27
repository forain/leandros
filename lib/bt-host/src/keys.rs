//! `/var/lib/bluetooth/linkkeys`: one bonded device per line,
//! `AA:BB:CC:DD:EE:FF <32 hex key> <key type> <name>`. Blank and `#` lines ignored.
use crate::Addr;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub addr: Addr,
    pub key: [u8; 16],
    pub ktype: u8,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyStore {
    pub keys: Vec<Key>,
}

impl KeyStore {
    pub fn parse(text: &str) -> KeyStore {
        let mut ks = KeyStore::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let mut it = line.splitn(4, ' ');
            let (Some(a), Some(k), Some(t)) = (it.next(), it.next(), it.next()) else { continue };
            let (Some(addr), Some(key), Ok(ktype)) = (Addr::parse(a), unhex16(k), t.parse::<u8>()) else { continue };
            ks.set(Key { addr, key, ktype, name: String::from(it.next().unwrap_or("")) });
        }
        ks
    }

    pub fn serialize(&self) -> String {
        let mut s = String::new();
        for k in &self.keys {
            let _ = write!(s, "{} ", k.addr);
            // Stored as transmitted on the wire (HCI order), hex, no separators.
            for b in k.key { let _ = write!(s, "{:02x}", b); }
            let _ = writeln!(s, " {} {}", k.ktype, k.name.replace('\n', " "));
        }
        s
    }

    pub fn get(&self, a: Addr) -> Option<&Key> { self.keys.iter().find(|k| k.addr == a) }

    pub fn set(&mut self, k: Key) {
        match self.keys.iter_mut().find(|e| e.addr == k.addr) {
            Some(e) => *e = k,
            None => self.keys.push(k),
        }
    }

    pub fn remove(&mut self, a: Addr) -> bool {
        let n = self.keys.len();
        self.keys.retain(|k| k.addr != a);
        n != self.keys.len()
    }
}

fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 { return None; }
    let mut k = [0u8; 16];
    for i in 0..16 { k[i] = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?; }
    Some(k)
}
