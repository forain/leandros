use alloc::string::String;
use alloc::vec::Vec;
use smoltcp::wire::{IpAddress, IpCidr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hook {
    Prerouting,
    Input,
    Forward,
    Output,
    Postrouting,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    Drop,
    Reject,
    Jump(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Verdict(Verdict),
    Snat(IpAddress),
    Dnat(IpAddress),
    Masquerade,
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub proto: Option<u8>,
    pub src_ip: Option<IpCidr>,
    pub dst_ip: Option<IpCidr>,
    pub src_port: Option<u16>,
    pub dst_port: Option<u16>,
    pub action: Action,
}

pub struct ConntrackEntry {
    pub proto: u8,
    pub orig_src: (IpAddress, u16),
    pub orig_dst: (IpAddress, u16),
    pub reply_src: (IpAddress, u16),
    pub reply_dst: (IpAddress, u16),
    pub age: u64,
}

pub struct FdbEntry {
    pub mac: [u8; 6],
    pub port: usize,
    pub last_seen: u64,
}

pub struct NftablesEngine {
    pub rules: Vec<(Hook, Rule)>,
    pub conntrack: Vec<ConntrackEntry>,
    pub fdb: Vec<FdbEntry>,
}

impl NftablesEngine {
    pub const fn new() -> Self {
        Self {
            rules: Vec::new(),
            conntrack: Vec::new(),
            fdb: Vec::new(),
        }
    }

    pub fn add_rule(&mut self, hook: Hook, rule: Rule) {
        self.rules.push((hook, rule));
    }

    pub fn evaluate(
        &mut self,
        hook: Hook,
        proto: u8,
        src_ip: IpAddress,
        dst_ip: IpAddress,
        src_port: Option<u16>,
        dst_port: Option<u16>,
    ) -> Verdict {
        for (h, rule) in &self.rules {
            if *h != hook {
                continue;
            }
            if let Some(r_proto) = rule.proto {
                if r_proto != proto {
                    continue;
                }
            }
            if let Some(r_src) = rule.src_ip {
                if !r_src.contains_addr(&src_ip) {
                    continue;
                }
            }
            if let Some(r_dst) = rule.dst_ip {
                if !r_dst.contains_addr(&dst_ip) {
                    continue;
                }
            }
            if let Some(r_src_port) = rule.src_port {
                if src_port != Some(r_src_port) {
                    continue;
                }
            }
            if let Some(r_dst_port) = rule.dst_port {
                if dst_port != Some(r_dst_port) {
                    continue;
                }
            }

            match &rule.action {
                Action::Verdict(v) => return v.clone(),
                _ => {}
            }
        }
        Verdict::Accept
    }

    pub fn handle_nat(
        &mut self,
        hook: Hook,
        _proto: u8,
        src_ip: &mut IpAddress,
        dst_ip: &mut IpAddress,
        _src_port: &mut Option<u16>,
        _dst_port: &mut Option<u16>,
    ) {
        for (h, rule) in &self.rules {
            if *h != hook {
                continue;
            }
            match &rule.action {
                Action::Snat(new_ip) => {
                    if hook == Hook::Postrouting {
                        *src_ip = *new_ip;
                    }
                }
                Action::Dnat(new_ip) => {
                    if hook == Hook::Prerouting {
                        *dst_ip = *new_ip;
                    }
                }
                Action::Masquerade => {
                    if hook == Hook::Postrouting {
                        // Simplified
                    }
                }
                _ => {}
            }
        }
    }

    pub fn bridge_forward(&mut self, mac: [u8; 6], incoming_port: usize) -> Option<usize> {
        let now = sched::ticks();
        if let Some(entry) = self.fdb.iter_mut().find(|e| e.mac == mac) {
            entry.port = incoming_port;
            entry.last_seen = now;
        } else {
            self.fdb.push(FdbEntry {
                mac,
                port: incoming_port,
                last_seen: now,
            });
        }

        self.fdb.retain(|e| now - e.last_seen < 3000);

        None
    }
}
