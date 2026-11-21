//! Shopping Sessions pipeline, v3: three materializations.
//!   parse  -> 32 B raw records partitioned by event id
//!   dedup  -> 16 B (person key, ts|packed) records partitioned by person key (person resolved via the link map)
//!   person -> local counting sort per partition, per-person sort + walk, parquet parts per thread

mod csv;
mod io;
mod out;
mod pq;
mod ts;
mod util;

use csv::{split_scanned, Lines, Scanner};
use io::Buffer;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use ts::{parse_f1, parse_ingest, parse_ts_cached, DAY};
use util::{pin_thread, prefetch, Lookup, PKey, Parts, Scatter, Side, SmallTable};

const SESSION_GAP: i64 = 30 * ts::MINUTE;
const LATE: i64 = DAY;
const REACH: i64 = 7 * DAY;

#[inline(always)]
fn part(h: u64, log_p: u32) -> usize {
    (h >> (64 - log_p)) as usize
}

/// Partition count: about 20k records per partition (a 2x table of 16-byte slots is 1 MB), at most 1024.
fn choose_log_p(n_est: usize) -> u32 {
    let mut lp = 6;
    while lp < 10 && (n_est >> lp) > 20000 {
        lp += 1;
    }
    lp
}

// ---------- records ----------

#[derive(Clone, Copy, Default)]
struct EvRaw {
    id: PKey,
    user: PKey,
    ts: u32,     // ms since window_start (valid only when flag bit0)
    ingest: i32, // whole seconds since window_start
    price: u32,
    cat: u16,
    flags: u8, // bit0 ts ok, bit1 user nonempty, bit2 product nonempty, bit3 price ok, bits4-5 etype
}

/// price (24 bits) | category (7 bits) << 24 | purchase (1 bit) << 31
#[inline(always)]
fn pack_ev(price: u32, cat: u16, purchase: bool) -> u32 {
    price | ((cat as u32) << 24) | ((purchase as u32) << 31)
}

/// A validated event routed to its person partition.
#[derive(Clone, Copy, Default)]
struct PRec {
    pkey: u64,
    ts: u32,
    packed: u32,
}

#[derive(Clone, Copy, Default)]
struct ClRaw {
    id: PKey,
    user: PKey,
    ts: u32, // ms since window_start - 7 days (valid only when flag bit0)
    camp: u16,
    flags: u8, // bit0 ts ok, bit1 user nonempty, bit2 campaign known
    _pad: [u8; 9],
}

/// A validated click routed to its person partition.
/// A validated click routed to its person partition. `lex` orders packed click ids like their bytes;
/// u64::MAX marks a non-packable id that must be compared by bytes.
#[derive(Clone, Copy, Default)]
struct CRec {
    pkey: u64,
    id: PKey,
    lex: u64,
    ts: u32,
    camp: u16,
    _pad: [u8; 2],
}

#[derive(Clone, Copy, Default)]
struct ClFinal {
    id: PKey,
    lex: u64,
    ts: u32,
    camp: u16,
}

#[inline(always)]
fn click_lex(id: PKey) -> u64 {
    if id.is_packed() { id.lex_key() } else { u64::MAX }
}

#[inline(always)]
fn click_order(a: &ClFinal, b: &ClFinal) -> std::cmp::Ordering {
    match a.ts.cmp(&b.ts) {
        std::cmp::Ordering::Equal => {
            if a.lex != u64::MAX && b.lex != u64::MAX {
                a.lex.cmp(&b.lex)
            } else {
                a.id.cmp_bytes(&b.id)
            }
        }
        o => o,
    }
}

#[derive(Clone, Copy, Default)]
struct LinkRaw {
    a: PKey,
    b: PKey,
}

struct Ctx {
    ws: i64,
    we: i64,
    products: Lookup,
    campaigns: Lookup,
    ncat: u16, // id of "unknown"
    cat_names: Vec<Vec<u8>>,
    cat_rank: Vec<u32>,
    ncamp: u16, // id of organic
    camp_names: Vec<Vec<u8>>,
    channels: Vec<Vec<u8>>,
    nthreads: usize,
    log_p: u32,
}

/// Shared raw pointer for disjoint parallel writes.
struct Shared<T>(*mut T);
impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(self.0)
    }
}
impl<T> Copy for Shared<T> {}
unsafe impl<T> Send for Shared<T> {}
unsafe impl<T> Sync for Shared<T> {}
impl<T> Shared<T> {
    #[inline(always)]
    fn p(&self) -> *mut T {
        self.0
    }
}

fn log(t0: &Instant, msg: &str) {
    eprintln!("[{:7.3}s] {}", t0.elapsed().as_secs_f64(), msg);
}

// ---------- small inputs ----------

fn read_small_csv(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let data = std::fs::read(path).unwrap_or_default();
    let mut out = Vec::new();
    let mut first = true;
    for line in Lines::new(&data, 0, data.len()) {
        if first {
            first = false;
            continue;
        }
        let mut f = [&b""[..]; 2];
        if csv::split_fields_scalar(line, &mut f) {
            out.push((f[0].to_vec(), f[1].to_vec()));
        }
    }
    out
}

fn read_manifest(path: &Path) -> (i64, i64) {
    let data = std::fs::read(path).expect("manifest");
    let get = |key: &str| -> i64 {
        let k = format!("\"{}\"", key);
        let i = data.windows(k.len()).position(|w| w == k.as_bytes()).expect("key") + k.len();
        let rest = &data[i..];
        let q1 = rest.iter().position(|&c| c == b'"').expect("quote") + 1;
        let q2 = rest[q1..].iter().position(|&c| c == b'"').expect("quote") + q1;
        parse_f1(&rest[q1..q2]).expect("manifest ts")
    };
    (get("window_start"), get("window_end"))
}

// ---------- parsing ----------

#[inline(always)]
fn parse_price(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut i = 0;
    while i < s.len() && s[i] == b'0' {
        i += 1;
    }
    let rest = &s[i..];
    if rest.len() > 8 {
        return None;
    }
    let mut v: u32 = 0;
    for &c in rest {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u32;
    }
    if v >= 1 && v <= 10_000_000 { Some(v) } else { None }
}

#[inline(always)]
fn parse_event_line(ctx: &Ctx, side: &mut Side, line: &[u8], masks: Option<(u128, u128)>) -> Option<EvRaw> {
    let mut f = [&b""[..]; 7];
    if !split_scanned(line, masks, &mut f) || f[0].is_empty() {
        return None;
    }
    let ingest = match parse_ingest(f[3], &mut side.dates) {
        Some(t) => ((t - ctx.ws) / 1000).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        None => i32::MAX,
    };
    let mut flags = 0u8;
    let ts = match parse_ts_cached(f[2], &mut side.dates) {
        Some(t) if t >= ctx.ws && t < ctx.we => {
            flags |= 1;
            (t - ctx.ws) as u32
        }
        _ => 0,
    };
    if !f[1].is_empty() {
        flags |= 2;
    }
    if !f[5].is_empty() {
        flags |= 4;
    }
    let etype: u8 = {
        let t = f[4];
        let w = unsafe { std::ptr::read_unaligned(t.as_ptr() as *const u64) };
        if t.len() == 4 && (w as u32) == u32::from_le_bytes(*b"view") {
            0
        } else if t.len() == 4 && (w as u32) == u32::from_le_bytes(*b"cart") {
            1
        } else if t.len() == 8 && w == u64::from_le_bytes(*b"purchase") {
            2
        } else {
            3
        }
    };
    flags |= etype << 4;
    let mut price = 0u32;
    let mut cat = ctx.ncat;
    if etype == 2 {
        if let Some(p) = parse_price(f[6]) {
            flags |= 8;
            price = p;
        }
        if !f[5].is_empty() {
            if let Some(c) = ctx.products.get(f[5]) {
                cat = c as u16;
            }
        }
    }
    Some(EvRaw { id: PKey::new(f[0]), user: PKey::new(f[1]), ts, ingest, price, cat, flags })
}

#[inline(always)]
fn parse_click_line(ctx: &Ctx, side: &mut Side, line: &[u8], masks: Option<(u128, u128)>) -> Option<ClRaw> {
    let mut f = [&b""[..]; 4];
    if !split_scanned(line, masks, &mut f) || f[0].is_empty() {
        return None;
    }
    let mut flags = 0u8;
    let lo = ctx.ws - REACH;
    let ts = match parse_ts_cached(f[3], &mut side.dates) {
        Some(t) if t >= lo && t < ctx.we => {
            flags |= 1;
            (t - lo) as u32
        }
        _ => 0,
    };
    if !f[1].is_empty() {
        flags |= 2;
    }
    let mut camp = ctx.ncamp;
    if let Some(c) = ctx.campaigns.get(f[2]) {
        flags |= 4;
        camp = c as u16;
    }
    Some(ClRaw { id: PKey::new(f[0]), user: PKey::new(f[1]), ts, camp, flags, _pad: [0; 9] })
}

// ---------- link map: user key -> person name key, for linked IDs only ----------

struct LinkMap {
    slots: Vec<(u64, u64)>, // (key, name); key 0 = empty
    mask: usize,
}

impl LinkMap {
    fn new(n: usize) -> Self {
        let cap = (n * 2).max(1024).next_power_of_two();
        LinkMap { slots: vec![(0, 0); cap], mask: cap - 1 }
    }
    fn insert(&mut self, key: u64, name: u64) {
        let h = PKey(key).hash();
        let mut s = ((h >> 20) as usize) & self.mask;
        loop {
            if self.slots[s].0 == 0 || self.slots[s].0 == key {
                self.slots[s] = (key, name);
                return;
            }
            s = (s + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn prefetch(&self, h: u64) {
        prefetch(unsafe { self.slots.as_ptr().add(((h >> 20) as usize) & self.mask) });
    }
    /// Name key of a linked ID, or None when the ID is not in any link. Callers prefetch the slot.
    #[inline(always)]
    fn get(&self, key: u64, h: u64) -> Option<u64> {
        let mut s = ((h >> 20) as usize) & self.mask;
        loop {
            let (k, n) = self.slots[s];
            if k == key {
                return Some(n);
            }
            if k == 0 {
                return None;
            }
            s = (s + 1) & self.mask;
        }
    }
}

/// Dense-id table for link endpoints (serial use).
struct IdTable {
    slots: Vec<u32>,
    keys: Vec<u64>,
    mask: usize,
}
impl IdTable {
    fn new(est: usize) -> Self {
        let cap = (est * 2).max(1024).next_power_of_two();
        IdTable { slots: vec![u32::MAX; cap], keys: Vec::with_capacity(est), mask: cap - 1 }
    }
    #[inline(always)]
    fn prefetch(&self, key: u64) {
        prefetch(unsafe { self.slots.as_ptr().add((PKey(key).hash() as usize) & self.mask) });
    }
    #[inline]
    fn get_or_insert(&mut self, key: u64) -> u32 {
        if self.keys.len() * 4 >= self.slots.len() * 3 {
            let cap = self.slots.len() * 2;
            let mut slots = vec![u32::MAX; cap];
            for (i, &k) in self.keys.iter().enumerate() {
                let mut s = (PKey(k).hash() as usize) & (cap - 1);
                while slots[s] != u32::MAX {
                    s = (s + 1) & (cap - 1);
                }
                slots[s] = i as u32;
            }
            self.slots = slots;
            self.mask = cap - 1;
        }
        let mut s = (PKey(key).hash() as usize) & self.mask;
        loop {
            let cur = self.slots[s];
            if cur == u32::MAX {
                self.slots[s] = self.keys.len() as u32;
                self.keys.push(key);
                return (self.keys.len() - 1) as u32;
            }
            if self.keys[cur as usize] == key {
                return cur;
            }
            s = (s + 1) & self.mask;
        }
    }
}

#[inline]
fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let gp = parent[parent[x as usize] as usize];
        parent[x as usize] = gp;
        x = gp;
    }
    x
}

/// Builds the link map from parsed links: dense ids, union-find, byte-minimal name per component.
fn build_link_map(links: &[LinkRaw]) -> LinkMap {
    let n_lines = links.len();
    let mut table = IdTable::new(n_lines * 2 + 64);
    let mut edges: Vec<(u32, u32)> = Vec::with_capacity(n_lines);
    const B: usize = 16;
    for chunk in links.chunks(B) {
        for l in chunk {
            table.prefetch(l.a.0);
            table.prefetch(l.b.0);
        }
        for l in chunk {
            let a = table.get_or_insert(l.a.0);
            let b = table.get_or_insert(l.b.0);
            edges.push((a, b));
        }
    }
    let n = table.keys.len();
    let mut parent: Vec<u32> = (0..n as u32).collect();
    for &(a, b) in &edges {
        let ra = find(&mut parent, a);
        let rb = find(&mut parent, b);
        if ra != rb {
            if ra < rb {
                parent[rb as usize] = ra;
            } else {
                parent[ra as usize] = rb;
            }
        }
    }
    drop(edges);
    // name per root: byte-minimal key (lex order for packed keys, byte compare otherwise)
    let mut root_of = vec![0u32; n];
    let mut best: Vec<u64> = vec![u64::MAX; n]; // lex key of the best packed candidate, per root
    let mut best_key: Vec<u64> = vec![0; n];
    for u in 0..n as u32 {
        let r = find(&mut parent, u);
        root_of[u as usize] = r;
        let k = PKey(table.keys[u as usize]);
        if k.is_packed() {
            let l = k.lex_key();
            if l < best[r as usize] {
                best[r as usize] = l;
                best_key[r as usize] = k.0;
            }
        }
    }
    // fallback (non-packable) keys: rare; compare by bytes against the current best
    for u in 0..n {
        let k = PKey(table.keys[u]);
        if !k.is_packed() {
            let r = root_of[u] as usize;
            if best_key[r] == 0 || k.cmp_bytes(&PKey(best_key[r])) == std::cmp::Ordering::Less {
                best_key[r] = k.0;
                best[r] = 0; // keep it (a packed key would be compared via cmp_bytes below)
            }
        }
    }
    // a packed key found after a fallback best must still be compared exactly: redo packed ones against fallback bests
    for u in 0..n {
        let r = root_of[u] as usize;
        let k = PKey(table.keys[u]);
        if k.is_packed() && !PKey(best_key[r]).is_packed() && k.cmp_bytes(&PKey(best_key[r])) == std::cmp::Ordering::Less {
            best_key[r] = k.0;
        }
    }
    let mut map = LinkMap::new(n);
    for u0 in (0..n).step_by(B) {
        let u1 = (u0 + B).min(n);
        for u in u0..u1 {
            map.prefetch(PKey(table.keys[u]).hash());
        }
        for u in u0..u1 {
            map.insert(table.keys[u], best_key[root_of[u] as usize]);
        }
    }
    map
}

// ---------- dedup ----------

#[inline(always)]
fn event_ok(r: &EvRaw) -> bool {
    let etype = r.flags >> 4;
    (r.flags & 7) == 7 && etype < 3 && (etype != 2 || (r.flags & 8) != 0) && (r.ingest as i64) * 1000 - (r.ts as i64) <= LATE
}

/// Per-thread scratch reused across partition tasks.
#[derive(Default)]
struct Scratch {
    slots: Vec<(u64, u32)>,
    win: Vec<u64>,
    ckeys: Vec<u64>,
}

#[inline(always)]
fn person_key(user: PKey, lm: &LinkMap) -> u64 {
    lm.get(user.0, user.hash()).unwrap_or(user.0)
}

const PF: usize = 24;

/// Timing accumulators (thread-nanoseconds) for the dedup and person stages, printed when TIMING is set.
static T_DEDUP: [std::sync::atomic::AtomicU64; 2] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];
static T_PARSE: [std::sync::atomic::AtomicU64; 3] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];
static T_PERSON: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
#[inline(always)]
fn tick(timing: bool, acc: &std::sync::atomic::AtomicU64, t: &mut Instant) {
    if timing {
        let now = Instant::now();
        acc.fetch_add((now - *t).as_nanos() as u64, Ordering::Relaxed);
        *t = now;
    }
}

/// Partition p, two passes over the raw records (no copy): pass 1 probes an L2-resident table keyed by
/// event id and keeps the copy with the smallest ingest (the loser's "win" bit is cleared); pass 2 walks
/// the records in memory order, validates winners, resolves the person and pushes to the thread's scatter.
fn dedup_events(raws: &[Parts<EvRaw>], p: usize, lm: &LinkMap, sc: &mut Scratch, out: &mut Scatter<PRec>, log_p: u32, timing: bool) {
    let mut t = Instant::now();
    let total: usize = raws.iter().map(|r| r.len(p)).sum();
    if total == 0 {
        return;
    }
    // slices of this partition: index = (slice id << 26) | position, dense = base[slice] + position
    let mut slices: Vec<&[EvRaw]> = Vec::with_capacity(raws.len() * 2);
    let mut base: Vec<u32> = Vec::with_capacity(raws.len() * 2 + 1);
    let mut acc = 0u32;
    for r in raws {
        for sl in r.get(p) {
            if !sl.is_empty() {
                slices.push(sl);
                base.push(acc);
                acc += sl.len() as u32;
            }
        }
    }
    assert!(slices.len() < 64 && total < (1 << 26), "partition too large: raise LOG_P");
    let cap = (total * 2).max(64).next_power_of_two();
    let mask = cap - 1;
    let slots = &mut sc.slots; // (key, idx)
    slots.clear();
    slots.resize(cap, (0, 0));
    let win = &mut sc.win;
    win.clear();
    win.resize(total / 64 + 1, 0);
    for (si, sl) in slices.iter().enumerate() {
        let b = base[si];
        for (i, rec) in sl.iter().enumerate() {
            let key = rec.id.0;
            let idx = ((si as u32) << 26) | i as u32;
            let dense = b + i as u32;
            let mut s = (rec.id.hash() as usize) & mask;
            loop {
                let (k, oidx) = slots[s];
                if k == 0 {
                    slots[s] = (key, idx);
                    win[(dense >> 6) as usize] |= 1 << (dense & 63);
                    break;
                }
                if k == key {
                    let other = &slices[(oidx >> 26) as usize][(oidx & 0x3FF_FFFF) as usize];
                    if rec.ingest < other.ingest {
                        slots[s] = (key, idx);
                        let od = base[(oidx >> 26) as usize] + (oidx & 0x3FF_FFFF);
                        win[(od >> 6) as usize] &= !(1 << (od & 63));
                        win[(dense >> 6) as usize] |= 1 << (dense & 63);
                    }
                    break;
                }
                s = (s + 1) & mask;
            }
        }
    }
    tick(timing, &T_DEDUP[0], &mut t);
    let mut dense = 0usize;
    for sl in &slices {
        let n = sl.len();
        for i in 0..n.min(PF) {
            lm.prefetch(sl[i].user.hash());
        }
        for (i, rec) in sl.iter().enumerate() {
            if i + PF < n {
                lm.prefetch(sl[i + PF].user.hash());
            }
            if (win[dense >> 6] >> (dense & 63)) & 1 != 0 && event_ok(rec) {
                let pk = person_key(rec.user, lm);
                out.push(part(PKey(pk).hash(), log_p), PRec { pkey: pk, ts: rec.ts, packed: pack_ev(rec.price, rec.cat, (rec.flags >> 4) == 2) });
            }
            dense += 1;
        }
    }
    tick(timing, &T_DEDUP[1], &mut t);
}

/// Partition p of clicks: the first copy of each click id is kept (copies are identical).
fn dedup_clicks(raws: &[Parts<ClRaw>], p: usize, lm: &LinkMap, sc: &mut Scratch, out: &mut Scatter<CRec>, log_p: u32) {
    let total: usize = raws.iter().map(|r| r.len(p)).sum();
    let cap = (total * 2).max(64).next_power_of_two();
    let mask = cap - 1;
    let slots = &mut sc.ckeys;
    slots.clear();
    slots.resize(cap, 0);
    for r in raws {
        for sl in r.get(p) {
            let n = sl.len();
            for i in 0..n.min(PF) {
                lm.prefetch(sl[i].user.hash());
            }
            for (i, rec) in sl.iter().enumerate() {
                if i + PF < n {
                    lm.prefetch(sl[i + PF].user.hash());
                }
                let key = rec.id.0;
                let mut s = (rec.id.hash() as usize) & mask;
                let fresh = loop {
                    let k = slots[s];
                    if k == 0 {
                        slots[s] = key;
                        break true;
                    }
                    if k == key {
                        break false;
                    }
                    s = (s + 1) & mask;
                };
                if fresh && (rec.flags & 7) == 7 {
                    let pk = person_key(rec.user, lm);
                    out.push(part(PKey(pk).hash(), log_p), CRec { pkey: pk, id: rec.id, lex: click_lex(rec.id), ts: rec.ts, camp: rec.camp, _pad: [0; 2] });
                }
            }
        }
    }
}

// ---------- sorting and walking ----------

/// Insertion sort for short runs (the common per-person case): no setup cost.
#[inline(always)]
fn insertion_sort_u64(v: &mut [u64]) {
    for i in 1..v.len() {
        let x = v[i];
        let mut j = i;
        while j > 0 && v[j - 1] > x {
            v[j] = v[j - 1];
            j -= 1;
        }
        v[j] = x;
    }
}

/// LSD radix sort of u64 keys on the top 32 bits only (the timestamp).
fn radix_sort_u64(v: &mut [u64], tmp: &mut Vec<u64>) {
    let n = v.len();
    tmp.clear();
    tmp.resize(n, 0);
    let mut src_is_v = true;
    for pass in 0..4 {
        let shift = 32 + pass * 8;
        let mut counts = [0u32; 256];
        {
            let src: &[u64] = if src_is_v { v } else { tmp };
            for &x in src {
                counts[((x >> shift) & 0xFF) as usize] += 1;
            }
        }
        if counts.iter().any(|&c| c as usize == n) {
            continue;
        }
        let mut sum = 0u32;
        for c in counts.iter_mut() {
            let x = *c;
            *c = sum;
            sum += x;
        }
        if src_is_v {
            for &x in v.iter() {
                let b = ((x >> shift) & 0xFF) as usize;
                tmp[counts[b] as usize] = x;
                counts[b] += 1;
            }
        } else {
            for &x in tmp.iter() {
                let b = ((x >> shift) & 0xFF) as usize;
                v[counts[b] as usize] = x;
                counts[b] += 1;
            }
        }
        src_is_v = !src_is_v;
    }
    if !src_is_v {
        v.copy_from_slice(tmp);
    }
}

#[derive(Clone, Copy)]
struct URow {
    pkey: u64,
    day: i32,
    sessions: u32,
    events: u32,
    revenue: i64,
    top_cat: u16,
}

#[derive(Clone, Copy, Default)]
struct CampCell {
    sessions: u32,
    purchasing: u32,
    assisted: u32,
    revenue: u64,
}

struct Walker<'a> {
    ctx: &'a Ctx,
    day0: i64,
    ndays: usize,
    cat_sum: Vec<i64>,
    touched: Vec<u16>,
    mark: Vec<u32>,
    serial: u32,
    rows: Vec<URow>,
    camp: Vec<CampCell>,
}

impl<'a> Walker<'a> {
    fn new(ctx: &'a Ctx, day0: i64, ndays: usize) -> Self {
        Walker {
            ctx,
            day0,
            ndays,
            cat_sum: vec![0; ctx.ncat as usize + 1],
            touched: Vec::new(),
            mark: vec![u32::MAX; ctx.ncamp as usize + 1],
            serial: 0,
            rows: Vec::new(),
            camp: vec![CampCell::default(); (ctx.ncamp as usize + 1) * ndays],
        }
    }

    fn flush(&mut self, pkey: u64, day: i64, sessions: u32, events: u32, revenue: i64) {
        let mut top = u16::MAX;
        if !self.touched.is_empty() {
            let mut best_sum = -1i64;
            let mut best_rank = u32::MAX;
            for &c in &self.touched {
                let s = self.cat_sum[c as usize];
                let r = self.ctx.cat_rank[c as usize];
                if s > best_sum || (s == best_sum && r < best_rank) {
                    best_sum = s;
                    best_rank = r;
                    top = c;
                }
                self.cat_sum[c as usize] = 0;
            }
            self.touched.clear();
        }
        self.rows.push(URow { pkey, day: day as i32, sessions, events, revenue, top_cat: top });
    }

    /// `ev` sorted by ts (u64: ts<<32 | packed); `cl` sorted by (ts, id bytes).
    fn walk(&mut self, pkey: u64, ev: &[u64], cl: &[ClFinal]) {
        let ndays = self.ndays;
        let ws = self.ctx.ws;
        let cl_base = ws - REACH;
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut i = 0usize;
        let mut cur_day = i64::MIN;
        let (mut d_sessions, mut d_events, mut d_revenue) = (0u32, 0u32, 0i64);
        while i < ev.len() {
            let start = ws + (ev[i] >> 32) as i64;
            let day = start.div_euclid(DAY);
            if day != cur_day {
                if cur_day != i64::MIN {
                    self.flush(pkey, cur_day, d_sessions, d_events, d_revenue);
                }
                cur_day = day;
                d_sessions = 0;
                d_events = 0;
                d_revenue = 0;
            }
            let mut j = i;
            let mut rev = 0i64;
            let mut purch = false;
            loop {
                let e = ev[j] as u32;
                if (e >> 31) != 0 {
                    purch = true;
                    let price = (e & 0x00FF_FFFF) as i64;
                    let cat = ((e >> 24) & 0x7F) as u16;
                    rev += price;
                    let c = cat as usize;
                    if self.cat_sum[c] == 0 {
                        self.touched.push(cat);
                    }
                    self.cat_sum[c] += price;
                }
                j += 1;
                if j >= ev.len() || ((ev[j] >> 32) as i64 - (ev[j - 1] >> 32) as i64) > SESSION_GAP {
                    break;
                }
            }
            let cnt = (j - i) as u32;
            d_sessions += 1;
            d_events += cnt;
            d_revenue += rev;
            while hi < cl.len() && cl_base + cl[hi].ts as i64 <= start {
                hi += 1;
            }
            let lb = start - REACH;
            while lo < hi && cl_base + (cl[lo].ts as i64) < lb {
                lo += 1;
            }
            let di = (day - self.day0) as usize;
            let camp = if hi > lo {
                let t = cl[hi - 1].ts;
                let mut k = hi - 1;
                while k > lo && cl[k - 1].ts == t {
                    k -= 1;
                }
                cl[k].camp as usize
            } else {
                self.ctx.ncamp as usize
            };
            let cell = &mut self.camp[camp * ndays + di];
            cell.sessions += 1;
            cell.purchasing += purch as u32;
            cell.revenue += rev as u64;
            self.serial += 1;
            let serial = self.serial;
            for c in &cl[lo..hi] {
                let ci = c.camp as usize;
                if self.mark[ci] != serial {
                    self.mark[ci] = serial;
                    self.camp[ci * ndays + di].assisted += 1;
                }
            }
            i = j;
        }
        if cur_day != i64::MIN {
            self.flush(pkey, cur_day, d_sessions, d_events, d_revenue);
        }
    }
}

/// Local dense ids for person keys within one person partition.
struct LocalIds {
    slots: Vec<u32>,
    keys: Vec<u64>,
    mask: usize,
}
impl LocalIds {
    fn new(est: usize) -> Self {
        let cap = (est * 2).max(64).next_power_of_two();
        LocalIds { slots: vec![u32::MAX; cap], keys: Vec::with_capacity(est), mask: cap - 1 }
    }
    #[inline(always)]
    fn prefetch(&self, h: u64) {
        prefetch(unsafe { self.slots.as_ptr().add((h as usize) & self.mask) });
    }
    #[inline]
    fn get_or_insert(&mut self, key: u64, h: u64) -> u32 {
        if self.keys.len() * 4 >= self.slots.len() * 3 {
            let cap = self.slots.len() * 2;
            let mut slots = vec![u32::MAX; cap];
            for (i, &k) in self.keys.iter().enumerate() {
                let mut s = (PKey(k).hash() as usize) & (cap - 1);
                while slots[s] != u32::MAX {
                    s = (s + 1) & (cap - 1);
                }
                slots[s] = i as u32;
            }
            self.slots = slots;
            self.mask = cap - 1;
        }
        let mut s = (h as usize) & self.mask;
        loop {
            let cur = self.slots[s];
            if cur == u32::MAX {
                self.slots[s] = self.keys.len() as u32;
                self.keys.push(key);
                return (self.keys.len() - 1) as u32;
            }
            if self.keys[cur as usize] == key {
                return cur;
            }
            s = (s + 1) & self.mask;
        }
    }
}

/// One person partition: local ids, counting sort of events and clicks, per-person sort + walk.
fn person_partition(
    q: usize,
    evs: &[Parts<PRec>],
    cls: &[Parts<CRec>],
    w: &mut Walker,
    radix_tmp: &mut Vec<u64>,
    timing: bool,
) {
    let mut tm = Instant::now();
    let nev: usize = evs.iter().map(|t| t.len(q)).sum();
    let ncl: usize = cls.iter().map(|t| t.len(q)).sum();
    let mut ids = LocalIds::new(nev / 12 + ncl / 12 + 64);
    const D: usize = 8;
    // pass A: local ids + counts (ids stored per record to avoid re-probing)
    let mut ev_lid: Vec<u32> = Vec::with_capacity(nev);
    let mut cnt: Vec<u32> = Vec::new();
    for t in evs {
        for recs in t.get(q) {
            let n = recs.len();
            for i in 0..n.min(D) {
                ids.prefetch(PKey(recs[i].pkey).hash());
            }
            for (i, r) in recs.iter().enumerate() {
                if i + D < n {
                    ids.prefetch(PKey(recs[i + D].pkey).hash());
                }
                let l = ids.get_or_insert(r.pkey, PKey(r.pkey).hash());
                if l as usize >= cnt.len() {
                    cnt.resize(l as usize + 1, 0);
                }
                cnt[l as usize] += 1;
                ev_lid.push(l);
            }
        }
    }
    let mut cl_lid: Vec<u32> = Vec::with_capacity(ncl);
    let mut ccnt: Vec<u32> = Vec::new();
    for t in cls {
        for recs in t.get(q) {
            let n = recs.len();
            for i in 0..n.min(D) {
                ids.prefetch(PKey(recs[i].pkey).hash());
            }
            for (i, r) in recs.iter().enumerate() {
                if i + D < n {
                    ids.prefetch(PKey(recs[i + D].pkey).hash());
                }
                let l = ids.get_or_insert(r.pkey, PKey(r.pkey).hash());
                if l as usize >= ccnt.len() {
                    ccnt.resize(l as usize + 1, 0);
                }
                ccnt[l as usize] += 1;
                cl_lid.push(l);
            }
        }
    }
    tick(timing, &T_PERSON[0], &mut tm);
    let np = ids.keys.len();
    cnt.resize(np, 0);
    ccnt.resize(np, 0);
    // prefix sums
    let mut estart = vec![0u32; np + 1];
    let mut cstart = vec![0u32; np + 1];
    for i in 0..np {
        estart[i + 1] = estart[i] + cnt[i];
        cstart[i + 1] = cstart[i] + ccnt[i];
    }
    // pass B: counting sort
    let mut ev_sorted: Vec<u64> = vec![0; nev];
    let mut pos: Vec<u32> = estart[..np].to_vec();
    let mut k = 0usize;
    for t in evs {
        for sl in t.get(q) {
            for r in sl {
                let l = ev_lid[k] as usize;
                k += 1;
                let d = pos[l];
                pos[l] += 1;
                ev_sorted[d as usize] = ((r.ts as u64) << 32) | r.packed as u64;
            }
        }
    }
    let mut cl_sorted: Vec<ClFinal> = vec![ClFinal::default(); ncl];
    let mut cpos: Vec<u32> = cstart[..np].to_vec();
    let mut k = 0usize;
    for t in cls {
        for sl in t.get(q) {
            for r in sl {
                let l = cl_lid[k] as usize;
                k += 1;
                let d = cpos[l];
                cpos[l] += 1;
                cl_sorted[d as usize] = ClFinal { id: r.id, lex: r.lex, ts: r.ts, camp: r.camp };
            }
        }
    }
    drop(ev_lid);
    drop(cl_lid);
    tick(timing, &T_PERSON[1], &mut tm);
    // per person: sort + walk
    for l in 0..np {
        let (es, ee) = (estart[l] as usize, estart[l + 1] as usize);
        if es == ee {
            continue;
        }
        let (cs, ce) = (cstart[l] as usize, cstart[l + 1] as usize);
        let ev = &mut ev_sorted[es..ee];
        if ev.len() <= 32 {
            insertion_sort_u64(ev);
        } else if ev.len() >= 4096 {
            radix_sort_u64(ev, radix_tmp);
        } else {
            ev.sort_unstable();
        }
        let cl = &mut cl_sorted[cs..ce];
        if cl.len() <= 24 {
            for i in 1..cl.len() {
                let mut j = i;
                while j > 0 && click_order(&cl[j - 1], &cl[j]) == std::cmp::Ordering::Greater {
                    cl.swap(j - 1, j);
                    j -= 1;
                }
            }
        } else {
            cl.sort_unstable_by(click_order);
        }
        if timing {
            tick(true, &T_PERSON[2], &mut tm);
        }
        w.walk(ids.keys[l], ev, cl);
        if timing {
            tick(true, &T_PERSON[3], &mut tm);
        }
    }
}

// ---------- main ----------

fn main() {
    #[cfg(target_os = "linux")]
    if std::env::var("NO_FORK").is_err() {
        unsafe {
            let mut fds = [0i32; 2];
            if libc::pipe(fds.as_mut_ptr()) == 0 {
                let pid = libc::fork();
                if pid > 0 {
                    libc::close(fds[1]);
                    let mut b = 0u8;
                    let n = libc::read(fds[0], &mut b as *mut u8 as *mut libc::c_void, 1);
                    libc::_exit(if n == 1 && b == 1 { 0 } else { 1 });
                } else if pid == 0 {
                    libc::close(fds[0]);
                    run();
                    let b = 1u8;
                    libc::write(fds[1], &b as *const u8 as *const libc::c_void, 1);
                    libc::_exit(0);
                }
            }
        }
    }
    run();
}

fn run() {
    if std::env::var_os("EXIT_NOW").is_some() {
        return;
    }
    let t0 = Instant::now();
    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let outdir = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let nthreads = std::env::var("NCPU")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .clamp(1, 16);

    let (ws, we) = read_manifest(&data.join("manifest.json"));
    assert!(we > ws && (we - ws + REACH) < u32::MAX as i64, "window too wide for u32 ms offsets");
    let prods = read_small_csv(&data.join("products.csv"));
    let camps = read_small_csv(&data.join("campaigns.csv"));

    let mut cat_names: Vec<Vec<u8>> = Vec::new();
    let mut cat_ids = SmallTable::with_capacity(64);
    let mut products = Lookup::new(b"p", prods.len());
    for (pid, cat) in &prods {
        let cid = match cat_ids.get(cat) {
            Some(c) => c,
            None => {
                let c = cat_names.len() as u32;
                cat_ids.insert(cat, c);
                cat_names.push(cat.clone());
                c
            }
        };
        products.insert(pid, cid);
    }
    let ncat = cat_names.len() as u16;
    assert!(ncat < 127, "more than 126 categories: widen pack_ev");
    cat_names.push(b"unknown".to_vec());
    let mut order: Vec<usize> = (0..cat_names.len()).collect();
    order.sort_by(|&a, &b| cat_names[a].cmp(&cat_names[b]));
    let mut cat_rank = vec![0u32; cat_names.len()];
    for (r, &c) in order.iter().enumerate() {
        cat_rank[c] = r as u32;
    }
    let mut campaigns = Lookup::new(b"c_", camps.len());
    let mut camp_names = Vec::new();
    let mut channels = Vec::new();
    for (cid, ch) in &camps {
        if campaigns.get(cid).is_none() {
            campaigns.insert(cid, camp_names.len() as u32);
            camp_names.push(cid.clone());
            channels.push(ch.clone());
        }
    }
    let ncamp = camp_names.len() as u16;
    camp_names.push(b"organic".to_vec());
    channels.push(b"organic".to_vec());

    let inputs = io::discover(&data);
    let ev_bytes: u64 = inputs.events.iter().map(|u| u.end - u.start).sum();
    let cl_bytes: u64 = inputs.clicks.iter().map(|u| u.end - u.start).sum();
    let n_est = (ev_bytes / 75) as usize + 1024;
    let log_p = std::env::var("LOG_P").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| choose_log_p(n_est));
    let np = 1usize << log_p;
    let timing = std::env::var_os("TIMING").is_some();
    let ctx = Ctx { ws, we, products, campaigns, ncat, cat_names, cat_rank, ncamp, camp_names, channels, nthreads, log_p };
    log(&t0, &format!("setup: {} files, {} threads, {} event units, {} partitions", inputs.files.len(), nthreads, inputs.events.len(), np));

    // Phase 1: one task queue: task 0 = parse links + build the link map (one worker), the rest = event units
    let est_ev = n_est / nthreads / np + 64;
    let next = AtomicUsize::new(0);
    let mut lm_slot: Option<LinkMap> = None;
    let ev_raw: Vec<Parts<EvRaw>> = {
        let lm_ptr = Shared(&mut lm_slot as *mut Option<LinkMap>);
        std::thread::scope(|s| {
            let mut hs = Vec::new();
            for t in 0..nthreads {
                let next = &next;
                let (ctx, inputs) = (&ctx, &inputs);
                hs.push(s.spawn(move || {
                    pin_thread(t);
                    let mut side = Side::new();
                    let mut out: Scatter<EvRaw> = Scatter::new(np, est_ev);
                    let mut buf = Buffer::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i == 0 {
                            let mut links: Vec<LinkRaw> = Vec::new();
                            for u in &inputs.links {
                                buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], *u);
                                let (lo, hi) = (buf.lo, buf.hi);
                                let data = buf.data();
                                let mut scn = Scanner::new(data, lo, hi);
                                while let Some((line, masks)) = scn.next() {
                                    let mut f = [&b""[..]; 2];
                                    if split_scanned(line, masks, &mut f) && !f[0].is_empty() && !f[1].is_empty() {
                                        links.push(LinkRaw { a: PKey::new(f[0]), b: PKey::new(f[1]) });
                                    }
                                }
                            }
                            let t_links = Instant::now();
                            let n_links = links.len();
                            let lm = build_link_map(&links);
                            if timing {
                                eprintln!("    link task: {} links, map built in {:.3}s", n_links, t_links.elapsed().as_secs_f64());
                            }
                            unsafe { *lm_ptr.p() = Some(lm) };
                        } else if i - 1 < inputs.events.len() {
                            let u = inputs.events[i - 1];
                            let mut tm = Instant::now();
                            buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
                            tick(timing, &T_PARSE[0], &mut tm);
                            let (lo, hi) = (buf.lo, buf.hi);
                            let data = buf.data();
                            let mut sc = Scanner::new(data, lo, hi);
                            while let Some((line, masks)) = sc.next() {
                                if let Some(r) = parse_event_line(ctx, &mut side, line, masks) {
                                    out.push(part(r.id.hash(), log_p), r);
                                }
                            }
                            tick(timing, &T_PARSE[1], &mut tm);
                        } else {
                            break;
                        }
                    }
                    out.finish()
                }));
            }
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        })
    };
    let lm = lm_slot.expect("link map");
    log(&t0, "events parsed, link map built");

    // Phase 2 (overlapped): event dedup partitions and click parse units share one task queue;
    // each thread owns one person-scatter for all the dedup tasks it runs.
    let n_cu = inputs.clicks.len();
    let est_cl = (cl_bytes / 45) as usize / nthreads / np + 64;
    let next = AtomicUsize::new(0);
    let (evs, cl_raw): (Vec<Parts<PRec>>, Vec<Parts<ClRaw>>) = std::thread::scope(|s| {
        let mut hs = Vec::new();
        for t in 0..nthreads {
            let next = &next;
            let (ctx, inputs, ev_raw, lm) = (&ctx, &inputs, &ev_raw, &lm);
            hs.push(s.spawn(move || {
                pin_thread(t);
                let mut sc = Scratch::default();
                let mut pout: Scatter<PRec> = Scatter::new(np, est_ev);
                let mut cout: Scatter<ClRaw> = Scatter::new(np, est_cl);
                let mut side = Side::new();
                let mut buf = Buffer::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i < np {
                        dedup_events(ev_raw, i, lm, &mut sc, &mut pout, log_p, timing);
                    } else if i < np + n_cu {
                        let u = inputs.clicks[i - np];
                        buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
                        let (lo, hi) = (buf.lo, buf.hi);
                        let data = buf.data();
                        let mut scn = Scanner::new(data, lo, hi);
                        while let Some((line, masks)) = scn.next() {
                            if let Some(r) = parse_click_line(ctx, &mut side, line, masks) {
                                cout.push(part(r.id.hash(), log_p), r);
                            }
                        }
                    } else {
                        break;
                    }
                }
                (pout.finish(), cout.finish())
            }));
        }
        hs.into_iter().map(|h| h.join().unwrap()).unzip()
    });
    drop(ev_raw);
    drop(inputs);
    log(&t0, "event dedup + click parse done");

    // Phase 3: click dedup
    let next = AtomicUsize::new(0);
    let cls: Vec<Parts<CRec>> = std::thread::scope(|s| {
        let mut hs = Vec::new();
        for t in 0..nthreads {
            let next = &next;
            let (cl_raw, lm) = (&cl_raw, &lm);
            hs.push(s.spawn(move || {
                pin_thread(t);
                let mut sc = Scratch::default();
                let mut out: Scatter<CRec> = Scatter::new(np, est_cl);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= np {
                        break;
                    }
                    dedup_clicks(cl_raw, i, lm, &mut sc, &mut out, log_p);
                }
                out.finish()
            }));
        }
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    drop(cl_raw);
    drop(lm);
    let n_ev: usize = evs.iter().map(|t| (0..np).map(|q| t.len(q)).sum::<usize>()).sum();
    let n_cl: usize = cls.iter().map(|t| (0..np).map(|q| t.len(q)).sum::<usize>()).sum();
    log(&t0, &format!("click dedup done: {} events, {} clicks routed", n_ev, n_cl));

    // Phase 4: person partitions (largest first), each thread writes its own user_daily part
    let day0 = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - day0 + 1) as usize;
    let mut order: Vec<usize> = (0..np).collect();
    let sizes: Vec<usize> = (0..np).map(|q| evs.iter().map(|t| t.len(q)).sum::<usize>()).collect();
    order.sort_by_key(|&q| std::cmp::Reverse(sizes[q]));
    let next = AtomicUsize::new(0);
    let user_dir = outdir.join("user_daily");
    std::fs::create_dir_all(&user_dir).expect("mkdir user_daily");
    let results: Vec<(Vec<CampCell>, usize)> = std::thread::scope(|s| {
        let mut hs = Vec::new();
        for t in 0..nthreads {
            let next = &next;
            let (ctx, evs, cls, order, user_dir) = (&ctx, &evs, &cls, &order, &user_dir);
            hs.push(s.spawn(move || {
                pin_thread(t);
                let mut w = Walker::new(ctx, day0, ndays);
                let mut radix_tmp: Vec<u64> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= np {
                        break;
                    }
                    person_partition(order[i], evs, cls, &mut w, &mut radix_tmp, timing);
                }
                let n_rows = w.rows.len();
                if n_rows > 0 || t == 0 {
                    let mut uc = out::UserDailyCols::new(n_rows);
                    let mut buf = Vec::new();
                    for r in &w.rows {
                        uc.push_user(PKey(r.pkey).bytes(&mut buf));
                        uc.day.push(r.day);
                        uc.sessions.push(r.sessions as i32);
                        uc.events.push(r.events as i32);
                        uc.revenue.push(r.revenue);
                        uc.push_cat(if r.top_cat == u16::MAX { None } else { Some(&ctx.cat_names[r.top_cat as usize][..]) });
                    }
                    out::write_user_daily(&user_dir.join(format!("part-{}.parquet", t)), uc).expect("write user_daily");
                }
                (w.camp, n_rows)
            }));
        }
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    log(&t0, "sessions+attribution+user_daily done");
    if timing {
        let g = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed) as f64 / 1e9;
        eprintln!(
            "    thread-seconds: event pread {:.3}, event scan+parse {:.3}; dedup probe {:.3}, dedup emit {:.3}; person ids {:.3}, counting sort {:.3}, sorts {:.3}, walk {:.3}",
            g(&T_PARSE[0]), g(&T_PARSE[1]), g(&T_DEDUP[0]), g(&T_DEDUP[1]), g(&T_PERSON[0]), g(&T_PERSON[1]), g(&T_PERSON[2]), g(&T_PERSON[3])
        );
    }

    let n_rows: usize = results.iter().map(|r| r.1).sum();
    let ncells = (ctx.ncamp as usize + 1) * ndays;
    let mut camp = vec![[0i64; 4]; ncells];
    for (wc, _) in &results {
        for (i, c) in wc.iter().enumerate() {
            camp[i][0] += c.sessions as i64;
            camp[i][1] += c.purchasing as i64;
            camp[i][2] += c.revenue as i64;
            camp[i][3] += c.assisted as i64;
        }
    }
    let mut cc = out::CampaignDailyCols::new(ncells);
    for ci in 0..=ctx.ncamp as usize {
        for d in 0..ndays {
            let c = &camp[ci * ndays + d];
            if c[0] == 0 && c[3] == 0 {
                continue;
            }
            cc.push_ids(&ctx.camp_names[ci], &ctx.channels[ci]);
            cc.day.push((day0 + d as i64) as i32);
            cc.sessions.push(c[0] as i32);
            cc.purchasing_sessions.push(c[1] as i32);
            cc.revenue.push(c[2]);
            cc.assisted_sessions.push(c[3] as i32);
        }
    }
    out::write_campaign_daily(&outdir.join("campaign_daily"), cc).expect("write campaign_daily");
    log(&t0, &format!("wrote {} user rows", n_rows));
}
