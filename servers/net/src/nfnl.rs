//! NETLINK_NETFILTER / NFNL_SUBSYS_NFTABLES control plane (no datapath).
//!
//! Objects (tables, chains, rules, anonymous/named sets and their elements) are
//! stored per network namespace in their RESOLVED form: the IDs `nft` uses to
//! refer to objects created earlier in the same batch (CHAIN_ID, SET_ID,
//! POSITION_ID, the `__set%d` naming) are rewritten to names/handles at
//! commit, so a dump decodes back exactly like the Linux kernel's. Rule
//! expressions are kept as the raw NFTA_RULE_EXPRESSIONS bytes (after that
//! rewrite); the dump re-emits them, `nft` decompiles them. The expression VM
//! is a later lane; unknown expressions are refused (EOPNOTSUPP) so it is loud.
//!
//! A batch is applied to a shadow copy of the namespace state and swapped in at
//! BATCH_END (generation +1); the first error discards it.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use spin::Mutex;

pub const NETLINK_NETFILTER: usize = 12;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;
const NLM_F_ROOT: u16 = 0x100;
const NLM_F_MATCH: u16 = 0x200;
const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_APPEND: u16 = 0x800;
const NLA_F_NESTED: u16 = 0x8000;

const BATCH_BEGIN: u16 = 0x10;
const BATCH_END: u16 = 0x11;
const SUBSYS_NFTABLES: u16 = 10;

const NEWTABLE: u16 = 0;
const GETTABLE: u16 = 1;
const DELTABLE: u16 = 2;
const NEWCHAIN: u16 = 3;
const GETCHAIN: u16 = 4;
const DELCHAIN: u16 = 5;
const NEWRULE: u16 = 6;
const GETRULE: u16 = 7;
const DELRULE: u16 = 8;
const NEWSET: u16 = 9;
const GETSET: u16 = 10;
const DELSET: u16 = 11;
const NEWSETELEM: u16 = 12;
const GETSETELEM: u16 = 13;
const DELSETELEM: u16 = 14;
const NEWGEN: u16 = 15;
const GETGEN: u16 = 16;
const GETOBJ: u16 = 19;
const GETFLOWTABLE: u16 = 23;
const DESTROY_BASE: u16 = 26; // DESTROYTABLE..DESTROYOBJ = DEL* + 24 (table 26, chain 27, rule 28, set 29, elem 30)

const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EBUSY: i32 = 16;
const EEXIST: i32 = 17;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;
const ERESTART: i32 = 85;

const DGRAM_MAX: usize = 4096;

// ---- attribute helpers ----------------------------------------------------

fn pad4(n: usize) -> usize { (n + 3) & !3 }

/// Top-level attributes of `b` as (type without flags, payload).
fn attrs(b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut o = Vec::new();
    let mut p = 0usize;
    while p + 4 <= b.len() {
        let l = u16::from_ne_bytes([b[p], b[p + 1]]) as usize;
        let t = u16::from_ne_bytes([b[p + 2], b[p + 3]]) & 0x3fff;
        if l < 4 || p + l > b.len() { break; }
        o.push((t, &b[p + 4..p + l]));
        p += pad4(l);
    }
    o
}

fn find<'a>(a: &[(u16, &'a [u8])], t: u16) -> Option<&'a [u8]> {
    a.iter().find(|x| x.0 == t).map(|x| x.1)
}

fn put(o: &mut Vec<u8>, t: u16, body: &[u8]) {
    o.extend_from_slice(&((4 + body.len()) as u16).to_ne_bytes());
    o.extend_from_slice(&t.to_ne_bytes());
    o.extend_from_slice(body);
    while o.len() % 4 != 0 { o.push(0); }
}

fn put_str(o: &mut Vec<u8>, t: u16, s: &[u8]) {
    let mut v = s.to_vec();
    v.push(0);
    put(o, t, &v);
}

fn put_be32(o: &mut Vec<u8>, t: u16, v: u32) { put(o, t, &v.to_be_bytes()); }
fn put_be64(o: &mut Vec<u8>, t: u16, v: u64) { put(o, t, &v.to_be_bytes()); }

fn be32(b: &[u8]) -> Option<u32> { if b.len() >= 4 { Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]])) } else { None } }
fn be64(b: &[u8]) -> Option<u64> {
    if b.len() >= 8 { let mut x = [0u8; 8]; x.copy_from_slice(&b[..8]); Some(u64::from_be_bytes(x)) } else { None }
}

/// NUL-terminated string payload without the NUL.
fn cstr(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) { Some(i) => &b[..i], None => b }
}

fn nlmsg(ty: u16, flags: u16, seq: u32, pid: u32, family: u8, res_id: u16, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(20 + body.len());
    m.extend_from_slice(&((20 + body.len()) as u32).to_ne_bytes());
    m.extend_from_slice(&ty.to_ne_bytes());
    m.extend_from_slice(&flags.to_ne_bytes());
    m.extend_from_slice(&seq.to_ne_bytes());
    m.extend_from_slice(&pid.to_ne_bytes());
    m.push(family);
    m.push(0);
    m.extend_from_slice(&res_id.to_be_bytes());
    m.extend_from_slice(body);
    while m.len() % 4 != 0 { m.push(0); }
    m
}

/// NLMSG_ERROR for the request header `h` (16 bytes); errno 0 is an ACK.
fn nlerr(h: &[u8], pid: u32, errno: i32) -> Vec<u8> {
    let mut m = Vec::with_capacity(36);
    m.extend_from_slice(&36u32.to_ne_bytes());
    m.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
    m.extend_from_slice(&0u16.to_ne_bytes());
    m.extend_from_slice(&h[8..12]);
    m.extend_from_slice(&pid.to_ne_bytes());
    m.extend_from_slice(&(-errno).to_ne_bytes());
    m.extend_from_slice(&h[..16]);
    m
}

/// Pack reply messages into datagrams of at most DGRAM_MAX bytes.
fn pack(msgs: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for m in msgs {
        match out.last_mut() {
            Some(d) if d.len() + m.len() <= DGRAM_MAX => d.extend_from_slice(&m),
            _ => out.push(m),
        }
    }
    out
}

// ---- state ----------------------------------------------------------------

#[derive(Clone)]
struct Rule { handle: u64, id: u32, exprs: Vec<u8>, userdata: Option<Vec<u8>> }

#[derive(Clone)]
struct Chain {
    name: Vec<u8>, handle: u64, id: u32,
    /// Raw NFTA_CHAIN_HOOK body, TYPE string, POLICY (base chains only).
    hook: Option<Vec<u8>>, ctype: Option<Vec<u8>>, policy: Option<u32>,
    flags: Option<u32>, userdata: Option<Vec<u8>>,
    rules: Vec<Rule>,
}

#[derive(Clone)]
struct Set {
    name: Vec<u8>, handle: u64, id: u32,
    /// Remaining NFTA_SET_* attributes (flags, key/data type+len, policy, desc, userdata ...), raw.
    attrs: Vec<(u16, Vec<u8>)>,
    /// Raw NFTA_SET_ELEM_LIST_ELEMENTS entries (bodies of each LIST_ELEM).
    elems: Vec<Vec<u8>>,
}

#[derive(Clone)]
struct Table {
    family: u8, name: Vec<u8>, handle: u64, flags: u32, userdata: Option<Vec<u8>>,
    next_h: u64, chains: Vec<Chain>, sets: Vec<Set>,
}

#[derive(Clone, Default)]
pub struct NfState { gen: u32, next_table: u64, tables: Vec<Table> }

static NF: Mutex<BTreeMap<u16, NfState>> = Mutex::new(BTreeMap::new());

/// True when netns `ns` has at least one table (cheap gate for the later datapath).
/// Does any base chain of `ns` sit on `hook`?
pub fn has_hook(ns: u16, hook: u32) -> bool {
    let g = NF.lock();
    let Some(st) = g.get(&ns) else { return false };
    st.tables.iter().filter(|t| t.family == 1 || t.family == 2).any(|t| t.chains.iter().any(|c|
        c.hook.as_ref().map_or(false, |h| find(&attrs(h), 1).and_then(be32) == Some(hook))))
}

pub fn has_tables(ns: u16) -> bool { NF.lock().get(&ns).map_or(false, |s| !s.tables.is_empty()) }

/// Drop a dead namespace's rules.
pub fn forget(ns: u16) { NF.lock().remove(&ns); }

fn find_table<'a>(st: &'a mut NfState, family: u8, name: &[u8]) -> Option<&'a mut Table> {
    st.tables.iter_mut().find(|t| t.family == family && t.name == name)
}

/// Does any `immediate` verdict (jump/goto) in `exprs` name chain `name`?
fn rule_jumps_to(exprs: &[u8], name: &[u8]) -> bool {
    for (t, e) in attrs(exprs) {
        if t != 1 { continue; }
        let ea = attrs(e);
        if find(&ea, 1).map(cstr) != Some(b"immediate") { continue; }
        let Some(d) = find(&ea, 2) else { continue };
        let Some(data) = find(&attrs(d), 2) else { continue };
        let Some(vd) = find(&attrs(data), 2) else { continue };
        if find(&attrs(vd), 2).map(cstr) == Some(name) { return true; }
    }
    false
}

// ---- expression resolution -----------------------------------------------

const EXPR_OK: [&[u8]; 12] = [
    b"payload", b"meta", b"ct", b"cmp", b"bitwise", b"immediate", b"lookup", b"range", b"fib", b"nat", b"masq", b"counter",
];

/// Rewrite ID references in one rule's NFTA_RULE_EXPRESSIONS body to names.
fn resolve_exprs(body: &[u8], t: &Table) -> Result<Vec<u8>, i32> {
    let mut out = Vec::new();
    for (ty, e) in attrs(body) {
        let _ = ty; // libnftnl numbers list entries 1,2,3...; the kernel ignores the type
        let ea = attrs(e);
        let name = find(&ea, 1).map(cstr).ok_or(EINVAL)?;
        if !EXPR_OK.contains(&name) { return Err(EOPNOTSUPP); }
        let data = find(&ea, 2);
        let mut ne = Vec::new();
        put_str(&mut ne, 1, name);
        if let Some(d) = data {
            let nd = match name {
                b"lookup" => resolve_lookup(d, t)?,
                b"immediate" => resolve_immediate(d, t)?,
                _ => d.to_vec(),
            };
            put(&mut ne, 2 | NLA_F_NESTED, &nd);
        }
        put(&mut out, 1 | NLA_F_NESTED, &ne);
    }
    Ok(out)
}

fn resolve_lookup(d: &[u8], t: &Table) -> Result<Vec<u8>, i32> {
    let da = attrs(d);
    let mut set_name: Option<Vec<u8>> = find(&da, 1).map(|s| cstr(s).to_vec());
    if let Some(id) = find(&da, 4).and_then(be32) {
        let s = t.sets.iter().find(|s| s.id == id && id != 0).ok_or(ENOENT)?;
        set_name = Some(s.name.clone());
    }
    let sn = set_name.ok_or(EINVAL)?;
    if !t.sets.iter().any(|s| s.name == sn) { return Err(ENOENT); }
    let mut o = Vec::new();
    put_str(&mut o, 1, &sn);
    for (ty, b) in da { if ty != 1 && ty != 4 { put(&mut o, ty, b); } }
    // keep SREG/DREG/FLAGS order stable: type order of the kernel's dump
    Ok(sorted(&o))
}

fn resolve_immediate(d: &[u8], t: &Table) -> Result<Vec<u8>, i32> {
    let da = attrs(d);
    let Some(data) = find(&da, 2) else { return Ok(d.to_vec()) };
    let dd = attrs(data);
    let Some(vd) = find(&dd, 2) else { return Ok(d.to_vec()) };
    let va = attrs(vd);
    let mut chain: Option<Vec<u8>> = find(&va, 2).map(|s| cstr(s).to_vec());
    if let Some(id) = find(&va, 3).and_then(be32) {
        let c = t.chains.iter().find(|c| c.id == id && id != 0).ok_or(ENOENT)?;
        chain = Some(c.name.clone());
    }
    let mut nv = Vec::new();
    for (ty, b) in &va { if *ty == 1 { put(&mut nv, 1, b); } }
    if let Some(cn) = chain {
        if !t.chains.iter().any(|c| c.name == cn) { return Err(ENOENT); }
        put_str(&mut nv, 2, &cn);
    }
    let mut ndata = Vec::new();
    put(&mut ndata, 2 | NLA_F_NESTED, &nv);
    let mut o = Vec::new();
    for (ty, b) in &da {
        if *ty == 2 { put(&mut o, 2 | NLA_F_NESTED, &ndata); } else { put(&mut o, *ty, b); }
    }
    Ok(o)
}

/// Re-emit attributes ordered by type (the kernel's fill order is by type).
fn sorted(b: &[u8]) -> Vec<u8> {
    let mut a = attrs(b);
    a.sort_by_key(|x| x.0);
    let mut o = Vec::new();
    for (t, p) in a { put(&mut o, t, p); }
    o
}

// ---- emitters -------------------------------------------------------------

fn fam_res(_f: u8) -> u16 { 0 }

fn table_msg(t: &Table, ty: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, 1, &t.name);
    put_be32(&mut b, 2, t.flags);
    put_be32(&mut b, 3, t.chains.len() as u32);
    put_be64(&mut b, 4, t.handle);
    if let Some(u) = &t.userdata { put(&mut b, 6, u); }
    nlmsg((SUBSYS_NFTABLES << 8) | ty, flags, seq, pid, t.family, fam_res(t.family), &b)
}

fn chain_use(t: &Table, c: &Chain) -> u32 {
    let mut n = c.rules.len() as u32;
    for oc in &t.chains { for r in &oc.rules { if rule_jumps_to(&r.exprs, &c.name) { n += 1; } } }
    n
}

fn chain_msg(t: &Table, c: &Chain, ty: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, 1, &t.name);
    put_be64(&mut b, 2, c.handle);
    put_str(&mut b, 3, &c.name);
    if let Some(h) = &c.hook { put(&mut b, 4 | NLA_F_NESTED, h); }
    if let Some(p) = c.policy { put_be32(&mut b, 5, p); }
    put_be32(&mut b, 6, chain_use(t, c));
    if let Some(ty) = &c.ctype { put_str(&mut b, 7, ty); }
    if let Some(f) = c.flags { put_be32(&mut b, 10, f); }
    if let Some(u) = &c.userdata { put(&mut b, 12, u); }
    nlmsg((SUBSYS_NFTABLES << 8) | ty, flags, seq, pid, t.family, fam_res(t.family), &b)
}

fn rule_msg(t: &Table, c: &Chain, r: &Rule, ty: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, 1, &t.name);
    put_str(&mut b, 2, &c.name);
    put_be64(&mut b, 3, r.handle);
    put(&mut b, 4 | NLA_F_NESTED, &r.exprs);
    if let Some(u) = &r.userdata { put(&mut b, 7, u); }
    nlmsg((SUBSYS_NFTABLES << 8) | ty, flags, seq, pid, t.family, fam_res(t.family), &b)
}

fn set_msg(t: &Table, s: &Set, ty: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut a: Vec<(u16, Vec<u8>)> = Vec::new();
    let mut tb = Vec::new(); tb.extend_from_slice(&t.name); tb.push(0);
    let mut nb = Vec::new(); nb.extend_from_slice(&s.name); nb.push(0);
    a.push((1, tb));
    a.push((2, nb));
    a.push((15, s.handle.to_be_bytes().to_vec()));
    for (ty, p) in &s.attrs { a.push((*ty, p.clone())); }
    a.sort_by_key(|x| x.0);
    let mut b = Vec::new();
    for (ty, p) in &a { put(&mut b, if *ty == 9 || *ty == 16 || *ty == 17 { *ty | NLA_F_NESTED } else { *ty }, p); }
    nlmsg((SUBSYS_NFTABLES << 8) | ty, flags, seq, pid, t.family, fam_res(t.family), &b)
}

fn setelem_msg(t: &Table, s: &Set, elems: &[Vec<u8>], flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, 1, &t.name);
    put_str(&mut b, 2, &s.name);
    let mut l = Vec::new();
    for e in elems { put(&mut l, 1 | NLA_F_NESTED, e); }
    put(&mut b, 3 | NLA_F_NESTED, &l);
    nlmsg((SUBSYS_NFTABLES << 8) | NEWSETELEM, flags, seq, pid, t.family, fam_res(t.family), &b)
}

// ---- request processing ---------------------------------------------------

struct Hdr<'a> { ty: u16, flags: u16, seq: u32, family: u8, whole: &'a [u8], body: &'a [u8] }

fn is_mutating(msg: u16) -> bool {
    matches!(msg, NEWTABLE | DELTABLE | NEWCHAIN | DELCHAIN | NEWRULE | DELRULE | NEWSET | DELSET | NEWSETELEM | DELSETELEM)
        || (DESTROY_BASE..=DESTROY_BASE + 4).contains(&msg)
}

/// Handle one send(2) worth of netlink data from netns `ns`; returns reply datagrams.
pub fn handle(ns: u16, pid: u32, admin: bool, data: &[u8]) -> Vec<Vec<u8>> {
    let mut msgs: Vec<Hdr> = Vec::new();
    let mut off = 0usize;
    while off + 16 <= data.len() {
        let len = u32::from_ne_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        if len < 16 || off + len > data.len() { break; }
        let ty = u16::from_ne_bytes([data[off + 4], data[off + 5]]);
        let flags = u16::from_ne_bytes([data[off + 6], data[off + 7]]);
        let seq = u32::from_ne_bytes([data[off + 8], data[off + 9], data[off + 10], data[off + 11]]);
        let family = if len >= 17 { data[off + 16] } else { 0 };
        let body = if len >= 20 { &data[off + 20..off + len] } else { &data[off + 16..off + 16] };
        msgs.push(Hdr { ty, flags, seq, family, whole: &data[off..off + len], body });
        off += pad4(len);
    }

    let mut guard = NF.lock();
    let real = guard.entry(ns).or_default();
    let mut replies: Vec<Vec<u8>> = Vec::new();
    // Shadow copy for the current batch (and for stand-alone mutating messages).
    let mut shadow: Option<NfState> = None;
    let mut batch = false;
    let mut dirty = false;

    for m in &msgs {
        if m.ty == BATCH_BEGIN {
            batch = true;
            shadow = Some(real.clone());
            let a = attrs(m.body);
            if let Some(g) = find(&a, 1).and_then(be32) {
                if g != real.gen { replies.push(nlerr(m.whole, pid, ERESTART)); shadow = None; break; }
            }
            continue;
        }
        if m.ty == BATCH_END {
            if let Some(s) = shadow.take() {
                if dirty { *real = s; real.gen = real.gen.wrapping_add(1); clear_ids(real); }
            }
            batch = false;
            dirty = false;
            continue;
        }
        if m.ty >> 8 != SUBSYS_NFTABLES {
            if m.flags & NLM_F_ACK != 0 || m.flags & NLM_F_REQUEST != 0 { replies.push(nlerr(m.whole, pid, EOPNOTSUPP)); }
            continue;
        }
        if !admin { replies.push(nlerr(m.whole, pid, EPERM)); if batch { shadow = None; } break; }
        let msg = m.ty & 0xff;
        let mutating = is_mutating(msg);
        let mut stand = false;
        if mutating && shadow.is_none() { shadow = Some(real.clone()); stand = true; }
        let res = {
            let st: &mut NfState = if mutating { shadow.as_mut().unwrap() } else { &mut *real };
            if mutating { apply(st, msg, m) } else { query(st, msg, m, pid).map(|r| { replies.extend(r); }) }
        };
        match res {
            Ok(()) => {
                if mutating { dirty = true; }
                if m.flags & NLM_F_ACK != 0 { replies.push(nlerr(m.whole, pid, 0)); }
                if stand {
                    let mut s = shadow.take().unwrap();
                    clear_ids(&mut s);
                    s.gen = s.gen.wrapping_add(1);
                    *real = s;
                    dirty = false;
                }
            }
            Err(e) => {
                replies.push(nlerr(m.whole, pid, e));
                shadow = None; // abort the whole batch
                break;
            }
        }
    }
    // A batch with no END (or never closed) is discarded like the kernel's incomplete batch... except
    // when it ended with the data: commit what was applied.
    if batch {
        if let Some(s) = shadow.take() {
            if dirty { *real = s; real.gen = real.gen.wrapping_add(1); clear_ids(real); }
        }
    }
    drop(guard);
    pack(replies)
}

fn clear_ids(st: &mut NfState) {
    for t in &mut st.tables {
        for c in &mut t.chains { c.id = 0; for r in &mut c.rules { r.id = 0; } }
        for s in &mut t.sets { s.id = 0; }
    }
}

fn dump_done(h: &Hdr, pid: u32) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&20u32.to_ne_bytes());
    m.extend_from_slice(&NLMSG_DONE.to_ne_bytes());
    m.extend_from_slice(&NLM_F_MULTI.to_ne_bytes());
    m.extend_from_slice(&h.seq.to_ne_bytes());
    m.extend_from_slice(&pid.to_ne_bytes());
    m.extend_from_slice(&0u32.to_ne_bytes());
    m
}

fn query(st: &mut NfState, msg: u16, h: &Hdr, pid: u32) -> Result<Vec<Vec<u8>>, i32> {
    let dump = h.flags & NLM_F_DUMP == NLM_F_DUMP;
    let a = attrs(h.body);
    let fam_ok = |f: u8| h.family == 0 || h.family == f;
    let mut out = Vec::new();
    let mf = if dump { NLM_F_MULTI } else { 0 };
    let a_table = find(&a, 1).map(cstr);
    match msg {
        GETGEN => {
            let mut b = Vec::new();
            put_be32(&mut b, 1, st.gen);
            put_be32(&mut b, 2, pid);
            put_str(&mut b, 3, b"nft");
            out.push(nlmsg((SUBSYS_NFTABLES << 8) | NEWGEN, 0, h.seq, pid, h.family, 0, &b));
            return Ok(out);
        }
        GETOBJ | GETFLOWTABLE => {
            if dump { out.push(dump_done(h, pid)); return Ok(out); }
            return Err(ENOENT);
        }
        GETTABLE => {
            for t in &st.tables {
                if !fam_ok(t.family) { continue; }
                if !dump && Some(&t.name[..]) != a_table { continue; }
                out.push(table_msg(t, NEWTABLE, mf, h.seq, pid));
            }
        }
        GETCHAIN => {
            let a_chain = find(&a, 3).map(cstr);
            for t in &st.tables {
                if !fam_ok(t.family) { continue; }
                if let Some(n) = a_table { if t.name != n { continue; } }
                for c in &t.chains {
                    if !dump && Some(&c.name[..]) != a_chain { continue; }
                    out.push(chain_msg(t, c, NEWCHAIN, mf, h.seq, pid));
                }
            }
        }
        GETRULE => {
            let a_chain = find(&a, 2).map(cstr);
            let a_handle = find(&a, 3).and_then(be64);
            for t in &st.tables {
                if !fam_ok(t.family) { continue; }
                if let Some(n) = a_table { if t.name != n { continue; } }
                for c in &t.chains {
                    if let Some(n) = a_chain { if c.name != n { continue; } }
                    for r in &c.rules {
                        if !dump && Some(r.handle) != a_handle { continue; }
                        out.push(rule_msg(t, c, r, NEWRULE, mf, h.seq, pid));
                    }
                }
            }
        }
        GETSET => {
            let a_set = find(&a, 2).map(cstr);
            for t in &st.tables {
                if !fam_ok(t.family) { continue; }
                if let Some(n) = a_table { if t.name != n { continue; } }
                for s in &t.sets {
                    if !dump && Some(&s.name[..]) != a_set { continue; }
                    out.push(set_msg(t, s, NEWSET, mf, h.seq, pid));
                }
            }
        }
        GETSETELEM => {
            let a_set = find(&a, 2).map(cstr);
            for t in &st.tables {
                if !fam_ok(t.family) { continue; }
                if let Some(n) = a_table { if t.name != n { continue; } }
                for s in &t.sets {
                    if Some(&s.name[..]) != a_set { continue; }
                    // all elements of the set in datagram-sized messages
                    let mut chunk: Vec<Vec<u8>> = Vec::new();
                    let mut sz = 0usize;
                    for e in &s.elems {
                        if sz + e.len() + 8 > 1800 && !chunk.is_empty() {
                            out.push(setelem_msg(t, s, &chunk, mf, h.seq, pid));
                            chunk.clear(); sz = 0;
                        }
                        sz += e.len() + 8;
                        chunk.push(e.clone());
                    }
                    if !chunk.is_empty() || dump { out.push(setelem_msg(t, s, &chunk, mf, h.seq, pid)); }
                }
            }
            if !dump && out.is_empty() { return Err(ENOENT); }
        }
        _ => return Err(EOPNOTSUPP),
    }
    if dump { out.push(dump_done(h, pid)); } else if out.is_empty() { return Err(ENOENT); }
    Ok(out)
}

fn apply(st: &mut NfState, msg: u16, h: &Hdr) -> Result<(), i32> {
    let destroy = (DESTROY_BASE..=DESTROY_BASE + 4).contains(&msg);
    let msg = if destroy { [DELTABLE, DELCHAIN, DELRULE, DELSET, DELSETELEM][(msg - DESTROY_BASE) as usize] } else { msg };
    let r = apply_inner(st, msg, h);
    if destroy && r == Err(ENOENT) { return Ok(()); }
    r
}

fn apply_inner(st: &mut NfState, msg: u16, h: &Hdr) -> Result<(), i32> {
    let a = attrs(h.body);
    let fam = h.family;
    let tname = find(&a, 1).map(cstr);
    match msg {
        NEWTABLE => {
            let n = tname.ok_or(EINVAL)?;
            if let Some(t) = find_table(st, fam, n) {
                if h.flags & NLM_F_EXCL != 0 { return Err(EEXIST); }
                if let Some(f) = find(&a, 2).and_then(be32) { t.flags = f; }
                return Ok(());
            }
            st.next_table += 1;
            let handle = st.next_table;
            st.tables.push(Table {
                family: fam, name: n.to_vec(), handle, flags: find(&a, 2).and_then(be32).unwrap_or(0),
                userdata: find(&a, 6).map(|u| u.to_vec()), next_h: 0, chains: Vec::new(), sets: Vec::new(),
            });
            Ok(())
        }
        DELTABLE => {
            let hd = find(&a, 4).and_then(be64);
            if tname.is_none() && hd.is_none() {
                // flush ruleset: every table of this family (all when UNSPEC)
                st.tables.retain(|t| fam != 0 && t.family != fam);
                return Ok(());
            }
            let pos = st.tables.iter().position(|t| (fam == 0 || t.family == fam)
                && (hd.map_or(true, |x| x == t.handle)) && (tname.map_or(true, |n| n == &t.name[..])));
            match pos { Some(p) => { st.tables.remove(p); Ok(()) } None => Err(ENOENT) }
        }
        NEWCHAIN => {
            let t = find_table(st, fam, tname.ok_or(EINVAL)?).ok_or(ENOENT)?;
            let name = find(&a, 3).map(cstr);
            let hd = find(&a, 2).and_then(be64);
            let id = find(&a, 11).and_then(be32).unwrap_or(0);
            let existing = t.chains.iter().position(|c| name.map_or(false, |n| c.name == n) || hd.map_or(false, |x| x == c.handle));
            if let Some(i) = existing {
                if h.flags & NLM_F_EXCL != 0 { return Err(EEXIST); }
                let c = &mut t.chains[i];
                if c.hook.is_some() { if let Some(p) = find(&a, 5).and_then(be32) { c.policy = Some(p); } }
                if id != 0 { c.id = id; }
                return Ok(());
            }
            let name = name.ok_or(EINVAL)?;
            if name.is_empty() || name.len() > 255 { return Err(EINVAL); }
            let hook = find(&a, 4).map(|x| x.to_vec());
            let ctype = find(&a, 7).map(|x| cstr(x).to_vec());
            if hook.is_some() && ctype.is_none() { return Err(EINVAL); }
            let policy = if hook.is_some() { Some(find(&a, 5).and_then(be32).unwrap_or(1)) } else { None };
            t.next_h += 1;
            t.chains.push(Chain {
                name: name.to_vec(), handle: t.next_h, id, hook, ctype, policy,
                flags: find(&a, 10).and_then(be32), userdata: find(&a, 12).map(|u| u.to_vec()), rules: Vec::new(),
            });
            Ok(())
        }
        DELCHAIN => {
            let t = find_table(st, fam, tname.ok_or(EINVAL)?).ok_or(ENOENT)?;
            let hd = find(&a, 2).and_then(be64);
            let name = find(&a, 3).map(cstr);
            let id = find(&a, 11).and_then(be32).unwrap_or(0);
            let i = t.chains.iter().position(|c| name.map_or(false, |n| c.name == n)
                || hd.map_or(false, |x| x == c.handle) || (id != 0 && c.id == id)).ok_or(ENOENT)?;
            // Like nf_tables_delchain: the chain's own rules go with it (nft_delrule_by_chain);
            // only jumps from other chains keep it busy.
            let cn = t.chains[i].name.clone();
            let jumped = t.chains.iter().enumerate().any(|(j, oc)| j != i && oc.rules.iter().any(|r| rule_jumps_to(&r.exprs, &cn)));
            if jumped { return Err(EBUSY); }
            t.chains.remove(i);
            Ok(())
        }
        NEWRULE => {
            let t = find_table(st, fam, tname.ok_or(EINVAL)?).ok_or(ENOENT)?;
            let ci = if let Some(cid) = find(&a, 11).and_then(be32) {
                t.chains.iter().position(|c| c.id == cid && cid != 0)
            } else {
                let n = find(&a, 2).map(cstr).ok_or(EINVAL)?;
                t.chains.iter().position(|c| c.name == n)
            }.ok_or(ENOENT)?;
            let exprs = resolve_exprs(find(&a, 4).unwrap_or(&[]), t)?;
            let rid = find(&a, 9).and_then(be32).unwrap_or(0);
            let userdata = find(&a, 7).map(|u| u.to_vec());
            let mut pos = find(&a, 6).and_then(be64);
            if let Some(pid) = find(&a, 10).and_then(be32) {
                pos = t.chains[ci].rules.iter().find(|r| r.id == pid && pid != 0).map(|r| r.handle);
                if pos.is_none() { return Err(ENOENT); }
            }
            let hd_given = find(&a, 3).and_then(be64);
            // replace an existing rule by handle
            if h.flags & NLM_F_REPLACE != 0 {
                let hd = hd_given.ok_or(EINVAL)?;
                let c = &mut t.chains[ci];
                let i = c.rules.iter().position(|r| r.handle == hd).ok_or(ENOENT)?;
                c.rules[i].exprs = exprs; c.rules[i].userdata = userdata; c.rules[i].id = rid;
                return Ok(());
            }
            t.next_h += 1;
            let nr = Rule { handle: t.next_h, id: rid, exprs, userdata };
            let c = &mut t.chains[ci];
            let at = match pos {
                Some(p) => {
                    let i = c.rules.iter().position(|r| r.handle == p).ok_or(ENOENT)?;
                    if h.flags & NLM_F_APPEND != 0 { i + 1 } else { i }
                }
                None => if h.flags & NLM_F_APPEND != 0 { c.rules.len() } else { 0 },
            };
            c.rules.insert(at, nr);
            Ok(())
        }
        DELRULE => {
            let t = find_table(st, fam, tname.ok_or(EINVAL)?).ok_or(ENOENT)?;
            let cname = find(&a, 2).map(cstr);
            let hd = find(&a, 3).and_then(be64);
            let rid = find(&a, 9).and_then(be32).unwrap_or(0);
            if hd.is_none() && rid == 0 {
                // flush one chain, or every chain of the table
                let mut hit = cname.is_none();
                for c in &mut t.chains {
                    if cname.map_or(true, |n| c.name == n) { c.rules.clear(); hit = true; }
                }
                return if hit { Ok(()) } else { Err(ENOENT) };
            }
            for c in &mut t.chains {
                if cname.map_or(false, |n| c.name != n) { continue; }
                if let Some(i) = c.rules.iter().position(|r| hd.map_or(false, |x| x == r.handle) || (rid != 0 && r.id == rid)) {
                    c.rules.remove(i);
                    return Ok(());
                }
            }
            Err(ENOENT)
        }
        NEWSET => {
            let t = find_table(st, fam, tname.ok_or(EINVAL)?).ok_or(ENOENT)?;
            let name = find(&a, 2).map(cstr);
            let id = find(&a, 10).and_then(be32).unwrap_or(0);
            if find(&a, 5).is_none() { return Err(EINVAL); } // KEY_LEN
            if let Some(n) = name {
                if let Some(s) = t.sets.iter_mut().find(|s| s.name == n) {
                    if h.flags & NLM_F_EXCL != 0 { return Err(EEXIST); }
                    if id != 0 { s.id = id; }
                    return Ok(());
                }
            }
            let mut nm = name.ok_or(EINVAL)?.to_vec();
            if let Some(p) = nm.windows(2).position(|w| w == b"%d") {
                let mut k = 0u32;
                let cand = loop {
                    let mut c = nm[..p].to_vec();
                    c.extend_from_slice(alloc::format!("{}", k).as_bytes());
                    c.extend_from_slice(&nm[p + 2..]);
                    if !t.sets.iter().any(|s| s.name == c) { break c; }
                    k += 1;
                };
                nm = cand;
            }
            t.next_h += 1;
            let mut at: Vec<(u16, Vec<u8>)> = Vec::new();
            for (ty, p) in &a { if !matches!(*ty, 1 | 2 | 10 | 15) { at.push((*ty, p.to_vec())); } }
            t.sets.push(Set { name: nm, handle: t.next_h, id, attrs: at, elems: Vec::new() });
            Ok(())
        }
        DELSET => {
            let t = find_table(st, fam, tname.ok_or(EINVAL)?).ok_or(ENOENT)?;
            let name = find(&a, 2).map(cstr);
            let hd = find(&a, 15).and_then(be64);
            let id = find(&a, 10).and_then(be32).unwrap_or(0);
            let i = t.sets.iter().position(|s| name.map_or(false, |n| s.name == n)
                || hd.map_or(false, |x| x == s.handle) || (id != 0 && s.id == id)).ok_or(ENOENT)?;
            let sn = t.sets[i].name.clone();
            if set_in_use(t, &sn) { return Err(EBUSY); }
            t.sets.remove(i);
            Ok(())
        }
        NEWSETELEM | DELSETELEM => {
            let t = find_table(st, fam, tname.ok_or(EINVAL)?).ok_or(ENOENT)?;
            let sid = find(&a, 4).and_then(be32).unwrap_or(0);
            let sname = find(&a, 2).map(cstr);
            let si = t.sets.iter().position(|s| sname.map_or(false, |n| s.name == n) || (sid != 0 && s.id == sid)).ok_or(ENOENT)?;
            let list = find(&a, 3);
            let elems: Vec<&[u8]> = match list { Some(l) => attrs(l).into_iter().map(|x| x.1).collect(), None => Vec::new() };
            let s = &mut t.sets[si];
            if msg == NEWSETELEM {
                for e in elems {
                    let k = elem_key(e);
                    if s.elems.iter().any(|x| elem_key(x) == k) {
                        if h.flags & NLM_F_EXCL != 0 { return Err(EEXIST); }
                        continue;
                    }
                    s.elems.push(e.to_vec());
                }
            } else if list.is_none() {
                s.elems.clear();
            } else {
                for e in elems {
                    let k = elem_key(e);
                    let i = s.elems.iter().position(|x| elem_key(x) == k).ok_or(ENOENT)?;
                    s.elems.remove(i);
                }
            }
            Ok(())
        }
        _ => Err(EOPNOTSUPP),
    }
}

/// Identity of a set element for add/delete: KEY (1) + FLAGS (3: interval end) attributes.
fn elem_key(e: &[u8]) -> Vec<u8> {
    let a = attrs(e);
    let mut o = Vec::new();
    if let Some(k) = find(&a, 1) { o.extend_from_slice(k); }
    if let Some(f) = find(&a, 3) { o.push(0xfe); o.extend_from_slice(f); }
    o
}

fn set_in_use(t: &Table, name: &[u8]) -> bool {
    for c in &t.chains {
        for r in &c.rules {
            for (ty, e) in attrs(&r.exprs) {
                if ty != 1 { continue; }
                let ea = attrs(e);
                if find(&ea, 1).map(cstr) != Some(b"lookup") { continue; }
                if let Some(d) = find(&ea, 2) {
                    if find(&attrs(d), 1).map(cstr) == Some(name) { return true; }
                }
            }
        }
    }
    false
}

#[allow(dead_code)]
const _: u16 = NLM_F_CREATE;

// ---- evaluation VM (podnat L2) ----------------------------------------------
//
// A register machine over exactly the expressions netavark emits. Rules are
// run on NEW flows only; unknown expressions never reach it (refused at commit).

pub const H_PRE: u32 = 0;
pub const H_FWD: u32 = 2;
pub const H_OUT: u32 = 3;
pub const H_POST: u32 = 4;

/// What the VM sees of a packet.
pub struct Pkt {
    pub l4: u8, pub src: [u8; 4], pub dst: [u8; 4], pub sport: u16, pub dport: u16,
    /// Output interface name (zero padded), for `meta oifname`.
    pub oif: [u8; 16],
    pub mark: u32,
    /// `fib daddr type local`.
    pub local: bool,
    /// IPv6 source/destination (a v6 packet: `src`/`dst` unused, family ip6/inet).
    pub v6: Option<([u8; 16], [u8; 16])>,
    /// `ct state`: true = the packet belongs to an established flow (reply seen), else NEW.
    pub est: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Nat { None, Dnat([u8; 4], u16), Dnat6([u8; 16], u16), Masq }

#[derive(PartialEq, Clone, Copy)]
enum Flow { Fall, Ret, Accept, Drop }

struct Cx<'a> { t: &'a Table, m: &'a mut Pkt, nat: Nat, depth: u32 }

fn roff(r: u32) -> usize { if r >= 8 { (r as usize - 8) * 4 } else { (r as usize).saturating_sub(1) * 16 } }

fn rset(regs: &mut [u8; 64], r: u32, v: &[u8]) -> Option<()> {
    let o = roff(r);
    let n = pad4(v.len());
    if o + n > 64 { return None; }
    regs[o..o + v.len()].copy_from_slice(v);
    for b in &mut regs[o + v.len()..o + n] { *b = 0; }
    Some(())
}

fn rget(regs: &[u8; 64], r: u32, n: usize) -> Option<&[u8]> {
    let o = roff(r);
    if o + n > 64 { None } else { Some(&regs[o..o + n]) }
}

fn dval(d: &[u8]) -> Option<&[u8]> { find(&attrs(d), 1) }

fn run_chain(cx: &mut Cx, name: &[u8]) -> Flow {
    if cx.depth > 16 { return Flow::Fall; }
    let t = cx.t;
    let Some(c) = t.chains.iter().find(|c| c.name == name) else { return Flow::Fall };
    cx.depth += 1;
    let f = run_rules(cx, c);
    cx.depth -= 1;
    f
}

fn run_rules(cx: &mut Cx, c: &Chain) -> Flow {
    for r in &c.rules {
        match run_rule(cx, &r.exprs) { Flow::Fall => {}, Flow::Ret => return Flow::Fall, f => return f }
    }
    Flow::Fall
}

fn run_rule(cx: &mut Cx, exprs: &[u8]) -> Flow {
    rule_inner(cx, exprs).unwrap_or(Flow::Fall)
}

fn rule_inner(cx: &mut Cx, exprs: &[u8]) -> Option<Flow> {
    let mut regs = [0u8; 64];
    for (_, e) in attrs(exprs) {
        let ea = attrs(e);
        let name = find(&ea, 1).map(cstr).unwrap_or(b"");
        let d = attrs(find(&ea, 2).unwrap_or(&[]));
        let g = |t: u16| find(&d, t).and_then(be32);
        match name {
            b"payload" => {
                let (dreg, base, off, len) = (g(1)?, g(2)?, g(3)? as usize, g(4)? as usize);
                let mut net = [0u8; 40];
                if let Some((s6, d6)) = cx.m.v6 {
                    net[0] = 0x60; net[6] = cx.m.l4;
                    net[8..24].copy_from_slice(&s6); net[24..40].copy_from_slice(&d6);
                } else {
                    net[0] = 0x45; net[9] = cx.m.l4;
                    net[12..16].copy_from_slice(&cx.m.src); net[16..20].copy_from_slice(&cx.m.dst);
                }
                let nlen = if cx.m.v6.is_some() { 40 } else { 20 };
                let mut tr = [0u8; 4];
                tr[0..2].copy_from_slice(&cx.m.sport.to_be_bytes()); tr[2..4].copy_from_slice(&cx.m.dport.to_be_bytes());
                let h: &[u8] = match base { 1 => &net[..nlen], 2 => &tr, _ => return None };
                if off + len > h.len() { return None; }
                rset(&mut regs, dreg, &h[off..off + len])?;
            }
            b"meta" => {
                if let Some(sreg) = g(3) {
                    if g(2)? == 3 { let v = rget(&regs, sreg, 4)?; cx.m.mark = u32::from_ne_bytes([v[0], v[1], v[2], v[3]]); }
                } else {
                    let dreg = g(1)?;
                    match g(2)? {
                        3 => rset(&mut regs, dreg, &cx.m.mark.to_ne_bytes())?,
                        7 => { let o = cx.m.oif; rset(&mut regs, dreg, &o)? }
                        15 => rset(&mut regs, dreg, &[if cx.m.v6.is_some() { 10 } else { 2 }])?,
                        16 => rset(&mut regs, dreg, &[cx.m.l4])?,
                        _ => rset(&mut regs, dreg, &[0u8; 16])?,
                    }
                }
            }
            // ct state: ESTABLISHED (2) for a flow whose reply was seen, else NEW (8).
            b"ct" => { let dreg = g(1)?; let st: u32 = if cx.m.est { 2 } else { 8 }; rset(&mut regs, dreg, &st.to_ne_bytes())?; }
            b"fib" => {
                let dreg = g(1)?;
                let local = g(3)? & 2 != 0 && cx.m.local;
                rset(&mut regs, dreg, &(if local { 2u32 } else { 1u32 }).to_ne_bytes())?;
            }
            b"cmp" => {
                let v = dval(find(&d, 3)?)?;
                let r = rget(&regs, g(1)?, v.len())?;
                let ok = match g(2)? { 0 => r == v, 1 => r != v, 2 => r < v, 3 => r <= v, 4 => r > v, 5 => r >= v, _ => return None };
                if !ok { return None; }
            }
            b"bitwise" => {
                let (sreg, dreg, len) = (g(1)?, g(2)?, g(3)? as usize);
                let mask = dval(find(&d, 4)?)?; let xor = dval(find(&d, 5)?)?;
                if mask.len() < len || xor.len() < len { return None; }
                let mut out = [0u8; 16];
                if len > 16 { return None; }
                let s = rget(&regs, sreg, len)?;
                for i in 0..len { out[i] = (s[i] & mask[i]) ^ xor[i]; }
                rset(&mut regs, dreg, &out[..len])?;
            }
            b"range" => {
                let (from, to) = (dval(find(&d, 3)?)?, dval(find(&d, 4)?)?);
                let r = rget(&regs, g(1)?, from.len())?;
                let inr = r >= from && r <= to;
                if inr != (g(2)? == 0) { return None; }
            }
            b"lookup" => {
                let sn = find(&d, 1).map(cstr)?;
                let set = cx.t.sets.iter().find(|s| s.name == sn)?;
                let sreg = g(2)?;
                let mut hit = false;
                for el in &set.elems {
                    let Some(k) = find(&attrs(el), 1).and_then(dval) else { continue };
                    if rget(&regs, sreg, k.len()).map_or(false, |r| r == k) { hit = true; break; }
                }
                if hit == (g(5).unwrap_or(0) & 1 != 0) { return None; }
            }
            b"immediate" => {
                let dreg = g(1)?;
                let data = attrs(find(&d, 2)?);
                if dreg != 0 { rset(&mut regs, dreg, find(&data, 1)?)?; continue; }
                let vd = attrs(find(&data, 2)?);
                let code = find(&vd, 1).and_then(be32)? as i32;
                match code {
                    0 => return Some(Flow::Drop),
                    1 => return Some(Flow::Accept),
                    -5 => return Some(Flow::Ret),
                    -3 | -4 => {
                        let n = find(&vd, 2).map(cstr)?.to_vec();
                        let f = run_chain(cx, &n);
                        return Some(if f == Flow::Fall { if code == -4 { Flow::Ret } else { Flow::Fall } } else { f });
                    }
                    _ => {}
                }
            }
            b"nat" => {
                if g(1)? != 1 { return None; }
                // An inet-table chain mixes ip and ip6 DNAT targets: a nat expr of the other family never applies.
                if (g(2) == Some(10)) != cx.m.v6.is_some() { return None; }
                let port = match g(5) { Some(r) => { let p = rget(&regs, r, 2)?; u16::from_be_bytes([p[0], p[1]]) } None => 0 };
                if g(2) == Some(10) {
                    let a = rget(&regs, g(3)?, 16)?;
                    let mut addr = [0u8; 16];
                    addr.copy_from_slice(a);
                    cx.nat = Nat::Dnat6(addr, port);
                    return Some(Flow::Accept);
                }
                let a = rget(&regs, g(3)?, 4)?;
                let addr = [a[0], a[1], a[2], a[3]];
                cx.nat = Nat::Dnat(addr, port);
                return Some(Flow::Accept);
            }
            b"masq" => { cx.nat = Nat::Masq; return Some(Flow::Accept); }
            _ => {}
        }
    }
    Some(Flow::Fall)
}

/// Run every base chain of `ns` at `hook` in priority order. (accepted, nat action).
pub fn eval(ns: u16, hook: u32, m: &mut Pkt) -> (bool, Nat) {
    let g = NF.lock();
    let Some(st) = g.get(&ns) else { return (true, Nat::None) };
    let mut chains: Vec<(i32, &Table, &Chain)> = Vec::new();
    for t in &st.tables {
        if t.family != 1 && t.family != (if m.v6.is_some() { 10 } else { 2 }) { continue; }
        for c in &t.chains {
            let Some(h) = &c.hook else { continue };
            let ha = attrs(h);
            if find(&ha, 1).and_then(be32) == Some(hook) {
                chains.push((find(&ha, 2).and_then(be32).unwrap_or(0) as i32, t, c));
            }
        }
    }
    chains.sort_by_key(|x| x.0);
    let mut nat = Nat::None;
    for (_, t, c) in chains {
        let mut cx = Cx { t, m: &mut *m, nat, depth: 0 };
        let f = run_rules(&mut cx, c);
        nat = cx.nat;
        if f == Flow::Drop || (f == Flow::Fall && c.policy == Some(0)) { return (false, nat); }
    }
    (true, nat)
}

/// Destination ports that some DNAT rule matches (any protocol): the SNAT allocator must not hand them out.
pub fn dnat_ports(ns: u16) -> Vec<(u16, u16)> {
    let g = NF.lock();
    let mut out = Vec::new();
    let Some(st) = g.get(&ns) else { return out };
    for t in &st.tables { for c in &t.chains { for r in &c.rules {
        let ex: Vec<_> = attrs(&r.exprs).into_iter().map(|(_, e)| attrs(e)).collect();
        if !ex.iter().any(|ea| find(ea, 1).map(cstr) == Some(b"nat")) { continue; }
        for ea in &ex {
            let nm = find(ea, 1).map(cstr).unwrap_or(b"");
            let Some(d) = find(ea, 2).map(attrs) else { continue };
            let two = |v: &[u8]| if v.len() == 2 { Some(u16::from_be_bytes([v[0], v[1]])) } else { None };
            if nm == b"cmp" {
                if let Some(p) = find(&d, 3).and_then(dval).and_then(two) { out.push((p, p)); }
            } else if nm == b"range" {
                if let (Some(a), Some(b)) = (find(&d, 3).and_then(dval).and_then(two), find(&d, 4).and_then(dval).and_then(two)) { out.push((a, b)); }
            }
        }
    } } }
    out
}
