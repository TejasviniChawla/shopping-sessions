//! Shopping Sessions pipeline, v4: one materialization of the events.
//!   links  -> link map (user key -> person name key) before anything else
//!   parse  -> each event's person resolved on the spot; a 16 B (person, ts|packed) record goes straight to
//!             its person partition (global pool, stable index) and a 16 B copy record (event id, ingest,
//!             index) to an event-id partition; clicks parsed meanwhile and resolved after
//!   dedup  -> per event-id partition: two-bit filter, suspects, earliest-copy table; losers marked DROPPED
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use ts::{parse_f1, parse_ingest, parse_ts_cached, DAY};
use util::{pin_thread, prefetch, ChunkScatter, Lookup, PKey, Pool, PoolParts, Runs, Side, SmallTable};

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

/// A validated event routed to its person partition. `ts` becomes DROPPED when a copy of the same
/// event with a smaller ingest exists.
#[derive(Clone, Copy, Default)]
struct PRec {
    pkey: u64,
    ts: u32,
    packed: u32,
}

/// Marks a PRec that lost the dedup; sorts after every real timestamp.
const DROPPED: u32 = u32::MAX;
/// A copy that has no event record (it failed validation): nothing to drop if it loses.
const NO_EVENT: u32 = u32::MAX;

/// One copy of an event, routed by event id: its ingest time and the index of its event record.
#[derive(Clone, Copy, Default)]
struct Copy_ {
    eid: u64,
    ingest: i32,
    at: u32,
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
    state: Vec<u64>,
    suspects: Vec<Copy_>,
    table: Vec<Copy_>,
}

/// Timing accumulators (thread-nanoseconds), printed when TIMING is set.
static T_DEDUP: [std::sync::atomic::AtomicU64; 3] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];
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

/// Filter slots per copy: with 16, a few copies in a hundred share a slot by chance and become suspects.
const FILTER_SLOTS: usize = 16;

/// Dedup of one event-id partition. Only a few percent of events have more than one copy, so the work is
/// finding them: pass 1 marks, in two-bit counters indexed by key hash, the slots hit more than once;
/// pass 2 gathers the copies in those slots; pass 3 runs those through a small table that keeps the
/// earliest copy of each event and marks the event record of every loser DROPPED (an invalid earliest
/// copy has no record, but still beats the later ones). Returns the number of records dropped.
fn dedup_partition(copies: &PoolParts<Copy_>, p: usize, sc: &mut Scratch, events: &Pool<PRec>, log_p: u32, timing: bool) -> u64 {
    let mut t = Instant::now();
    let count = copies.len(p);
    if count < 2 {
        return 0;
    }
    let slots = (count * FILTER_SLOTS).next_power_of_two().max(1024);
    let slot_shift = 64 - slots.trailing_zeros();
    let state = &mut sc.state;
    state.clear();
    state.resize(slots / 32, 0);
    // the partition took the hash's top bits; the slot takes the ones below
    let slot = |h: u64| ((h << log_p) >> slot_shift) as usize;
    let mut late = 0usize;
    for sl in copies.slices(p) {
        for c in sl {
            let at = slot(PKey(c.eid).hash());
            unsafe {
                let word = state.get_unchecked_mut(at >> 5);
                let shift = (at & 31) * 2;
                let seen = (*word >> shift) & 1;
                late += seen as usize;
                *word |= (1 | (seen << 1)) << shift;
            }
        }
    }
    tick(timing, &T_DEDUP[0], &mut t);
    if late == 0 {
        return 0;
    }
    // a slot hit more than once holds one copy that was not late and at least one that was
    let suspects = &mut sc.suspects;
    suspects.clear();
    suspects.reserve(2 * late + 1);
    let mut found = 0usize;
    for sl in copies.slices(p) {
        for c in sl {
            let at = slot(PKey(c.eid).hash());
            let again = (unsafe { *state.get_unchecked(at >> 5) } >> ((at & 31) * 2 + 1)) & 1;
            unsafe { suspects.as_mut_ptr().add(found).write(*c) };
            found += again as usize;
        }
    }
    unsafe { suspects.set_len(found) };
    tick(timing, &T_DEDUP[1], &mut t);
    let cap = (found * 2).next_power_of_two().max(16);
    let mask = cap - 1;
    let table = &mut sc.table;
    table.clear();
    table.resize(cap, Copy_ { eid: 0, ingest: 0, at: 0 });
    let mut dropped = 0u64;
    for c in suspects.iter() {
        let mut s = (PKey(c.eid).hash() as usize) & mask;
        loop {
            let held = unsafe { table.get_unchecked_mut(s) };
            if held.eid == c.eid {
                // the later copy loses; with equal ingest the copies are identical and the new one goes
                let loser = if c.ingest < held.ingest {
                    let l = held.at;
                    *held = *c;
                    l
                } else {
                    c.at
                };
                if loser != NO_EVENT {
                    unsafe { (*events.at(loser)).ts = DROPPED };
                    dropped += 1;
                }
                break;
            }
            if held.eid == 0 {
                *held = *c;
                break;
            }
            s = (s + 1) & mask;
        }
    }
    tick(timing, &T_DEDUP[2], &mut t);
    dropped
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

/// Sort one person's events (u64 ts<<32|packed) and clicks, then walk them.
#[inline(always)]
fn sort_and_walk(w: &mut Walker, pkey: u64, ev: &mut [u64], cl: &mut [ClFinal], radix_tmp: &mut Vec<u64>, timing: bool, tm: &mut Instant) {
    if ev.len() <= 32 {
        insertion_sort_u64(ev);
    } else if ev.len() >= 4096 {
        radix_sort_u64(ev, radix_tmp);
    } else {
        ev.sort_unstable();
    }
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
        tick(true, &T_PERSON[2], tm);
    }
    w.walk(pkey, ev, cl);
    if timing {
        tick(true, &T_PERSON[3], tm);
    }
}

/// Grouped records of one partition: per local key, a contiguous run of events and of clicks.
struct Grouped {
    keys: Vec<u64>,
    estart: Vec<u32>,
    cstart: Vec<u32>,
    ev_sorted: Vec<u64>,
    cl_sorted: Vec<ClFinal>,
}

/// Local dense ids + counting sort of one partition's events (DROPPED ones left out) and clicks by key.
fn group_partition(q: usize, evs: &PoolParts<PRec>, cls: &PoolParts<CRec>, timing: bool, tm: &mut Instant) -> Grouped {
    let nev: usize = evs.len(q);
    let ncl: usize = cls.len(q);
    let mut ids = LocalIds::new(nev / 24 + ncl / 24 + 64);
    const D: usize = 8;
    let mut ev_lid: Vec<u32> = Vec::with_capacity(nev);
    let mut cnt: Vec<u32> = Vec::new();
    let mut kept = 0usize;
    for recs in evs.slices(q) {
        let n = recs.len();
        for i in 0..n.min(D) {
            ids.prefetch(PKey(recs[i].pkey).hash());
        }
        for (i, r) in recs.iter().enumerate() {
            if i + D < n {
                ids.prefetch(PKey(recs[i + D].pkey).hash());
            }
            if r.ts == DROPPED {
                ev_lid.push(u32::MAX);
                continue;
            }
            let l = ids.get_or_insert(r.pkey, PKey(r.pkey).hash());
            if l as usize >= cnt.len() {
                cnt.resize(l as usize + 1, 0);
            }
            cnt[l as usize] += 1;
            ev_lid.push(l);
            kept += 1;
        }
    }
    let mut cl_lid: Vec<u32> = Vec::with_capacity(ncl);
    let mut ccnt: Vec<u32> = Vec::new();
    for recs in cls.slices(q) {
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
    tick(timing, &T_PERSON[0], tm);
    let np = ids.keys.len();
    cnt.resize(np, 0);
    ccnt.resize(np, 0);
    let mut estart = vec![0u32; np + 1];
    let mut cstart = vec![0u32; np + 1];
    for i in 0..np {
        estart[i + 1] = estart[i] + cnt[i];
        cstart[i + 1] = cstart[i] + ccnt[i];
    }
    let mut ev_sorted: Vec<u64> = vec![0; kept];
    let mut pos: Vec<u32> = estart[..np].to_vec();
    let mut k = 0usize;
    for sl in evs.slices(q) {
        for r in sl {
            let l = ev_lid[k];
            k += 1;
            if l == u32::MAX {
                continue;
            }
            let d = pos[l as usize];
            pos[l as usize] += 1;
            ev_sorted[d as usize] = ((r.ts as u64) << 32) | r.packed as u64;
        }
    }
    let mut cl_sorted: Vec<ClFinal> = vec![ClFinal::default(); ncl];
    let mut cpos: Vec<u32> = cstart[..np].to_vec();
    let mut k = 0usize;
    for sl in cls.slices(q) {
        for r in sl {
            let l = cl_lid[k] as usize;
            k += 1;
            let d = cpos[l];
            cpos[l] += 1;
            cl_sorted[d as usize] = ClFinal { id: r.id, lex: r.lex, ts: r.ts, camp: r.camp };
        }
    }
    tick(timing, &T_PERSON[1], tm);
    Grouped { keys: ids.keys, estart, cstart, ev_sorted, cl_sorted }
}

/// One PERSON partition: every person's events and clicks are here (the parse resolved the person).
/// Clicks are deduplicated here: copies of a click are identical, so after the sort they are adjacent.
fn person_partition(q: usize, evs: &PoolParts<PRec>, cls: &PoolParts<CRec>, w: &mut Walker, radix_tmp: &mut Vec<u64>, timing: bool) {
    let mut tm = Instant::now();
    let mut g = group_partition(q, evs, cls, timing, &mut tm);
    for l in 0..g.keys.len() {
        let (es, ee) = (g.estart[l] as usize, g.estart[l + 1] as usize);
        if es == ee {
            continue;
        }
        let (cs, ce) = (g.cstart[l] as usize, g.cstart[l + 1] as usize);
        let pkey = g.keys[l];
        let (ev, cl) = (&mut g.ev_sorted[es..ee], &mut g.cl_sorted[cs..ce]);
        sort_and_walk(w, pkey, ev, cl, radix_tmp, timing, &mut tm);
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

    // Phase 0: links, parsed by every thread.
    let next = AtomicUsize::new(0);
    let link_lists: Vec<Vec<LinkRaw>> = std::thread::scope(|s| {
        let mut hs = Vec::new();
        for t in 0..nthreads {
            let next = &next;
            let inputs = &inputs;
            hs.push(s.spawn(move || {
                pin_thread(t);
                let mut buf = Buffer::new();
                let mut links: Vec<LinkRaw> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= inputs.links.len() {
                        break;
                    }
                    let u = inputs.links[i];
                    buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
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
                links
            }));
        }
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let links: Vec<LinkRaw> = link_lists.concat();
    drop(link_lists);
    log(&t0, &format!("links parsed: {}", links.len()));

    // Phase 1: thread 0 joins the links into the link map while the others parse clicks (unresolved);
    // once the map is up every thread parses event units: each line's person is resolved through the
    // map (slot prefetched 16 lines ahead) and the event goes straight to its person partition, with a
    // 16-byte copy record by event id for the dedup. Then the parsed clicks are resolved and scattered.
    let ev_pool: Pool<PRec> = Pool::new(n_est);
    let cp_pool: Pool<Copy_> = Pool::new(n_est);
    let n_cl_est = (cl_bytes / 45) as usize + 1024;
    let cl_pool: Pool<CRec> = Pool::new(n_cl_est);
    let lm_ready = AtomicBool::new(false);
    let mut lm_slot: Option<LinkMap> = None;
    let next_c = AtomicUsize::new(0);
    let next_e = AtomicUsize::new(0);
    let (ev_lists, cp_lists, cl_raw): (Vec<Vec<Runs>>, Vec<Vec<Runs>>, Vec<Vec<ClRaw>>) = {
        let lm_ptr = Shared(&mut lm_slot as *mut Option<LinkMap>);
        let links = &links;
        std::thread::scope(|s| {
            let mut hs = Vec::new();
            for t in 0..nthreads {
                let (next_c, next_e, lm_ready) = (&next_c, &next_e, &lm_ready);
                let (ctx, inputs, ev_pool, cp_pool) = (&ctx, &inputs, &ev_pool, &cp_pool);
                hs.push(s.spawn(move || {
                    pin_thread(t);
                    let mut side = Side::new();
                    let mut buf = Buffer::new();
                    let mut craw: Vec<ClRaw> = Vec::new();
                    if t == 0 || nthreads == 1 {
                        let t_links = Instant::now();
                        let lm = build_link_map(links);
                        if timing {
                            eprintln!("    link task: {} links, map built in {:.3}s", links.len(), t_links.elapsed().as_secs_f64());
                        }
                        unsafe { *lm_ptr.p() = Some(lm) };
                        lm_ready.store(true, Ordering::Release);
                    }
                    if t != 0 || nthreads == 1 {
                        loop {
                            let i = next_c.fetch_add(1, Ordering::Relaxed);
                            if i >= inputs.clicks.len() {
                                break;
                            }
                            let u = inputs.clicks[i];
                            buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
                            let (lo, hi) = (buf.lo, buf.hi);
                            let data = buf.data();
                            let mut scn = Scanner::new(data, lo, hi);
                            while let Some((line, masks)) = scn.next() {
                                if let Some(r) = parse_click_line(ctx, &mut side, line, masks) {
                                    if (r.flags & 7) == 7 {
                                        craw.push(r);
                                    }
                                }
                            }
                        }
                    }
                    while !lm_ready.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    let lm: &LinkMap = unsafe { (*lm_ptr.p()).as_ref().unwrap() };
                    let mut pout: ChunkScatter<PRec> = ChunkScatter::new(ev_pool, np);
                    let mut cout: ChunkScatter<Copy_> = ChunkScatter::new(cp_pool, np);
                    const D: usize = 16;
                    let mut ring: [EvRaw; D] = [EvRaw::default(); D];
                    let mut n = 0usize;
                    macro_rules! emit {
                        ($r:expr) => {{
                            let r: &EvRaw = $r;
                            let uh = r.user.hash();
                            let person = lm.get(r.user.0, uh).unwrap_or(r.user.0);
                            let at = if event_ok(r) {
                                pout.push(part(PKey(person).hash(), log_p), PRec { pkey: person, ts: r.ts, packed: pack_ev(r.price, r.cat, (r.flags >> 4) == 2) })
                            } else {
                                NO_EVENT
                            };
                            cout.push(part(r.id.hash(), log_p), Copy_ { eid: r.id.0, ingest: r.ingest, at });
                        }};
                    }
                    loop {
                        let i = next_e.fetch_add(1, Ordering::Relaxed);
                        if i >= inputs.events.len() {
                            break;
                        }
                        let u = inputs.events[i];
                        let mut tm = Instant::now();
                        buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
                        tick(timing, &T_PARSE[0], &mut tm);
                        let (lo, hi) = (buf.lo, buf.hi);
                        let data = buf.data();
                        let mut sc = Scanner::new(data, lo, hi);
                        while let Some((line, masks)) = sc.next() {
                            if let Some(r) = parse_event_line(ctx, &mut side, line, masks) {
                                lm.prefetch(r.user.hash());
                                if n >= D {
                                    emit!(&ring[n % D]);
                                }
                                ring[n % D] = r;
                                n += 1;
                            }
                        }
                        tick(timing, &T_PARSE[1], &mut tm);
                    }
                    for i in n.saturating_sub(D)..n {
                        emit!(&ring[i % D]);
                    }
                    (pout.finish(), cout.finish(), craw)
                }));
            }
            let mut a = Vec::new();
            let mut b = Vec::new();
            let mut c = Vec::new();
            for h in hs {
                let (x, y, z) = h.join().unwrap();
                a.push(x);
                b.push(y);
                c.push(z);
            }
            (a, b, c)
        })
    };
    let lm = lm_slot.expect("link map");
    drop(links);
    let evs = PoolParts { pool: &ev_pool, lists: ev_lists };
    let cps = PoolParts { pool: &cp_pool, lists: cp_lists };
    log(&t0, &format!("events parsed: {} records, {} copies", evs.total(), cps.total()));

    // Phase 2: dedup partitions and click resolution share one task queue.
    const CL_TASK: usize = 1 << 16;
    let mut cl_tasks: Vec<(usize, usize)> = Vec::new();
    for (t, v) in cl_raw.iter().enumerate() {
        let mut s = 0;
        while s < v.len() {
            cl_tasks.push((t, s));
            s += CL_TASK;
        }
    }
    let next = AtomicUsize::new(0);
    let dropped = std::sync::atomic::AtomicU64::new(0);
    let cl_lists: Vec<Vec<Runs>> = std::thread::scope(|s| {
        let mut hs = Vec::new();
        for t in 0..nthreads {
            let (next, dropped) = (&next, &dropped);
            let (cps, ev_pool, cl_pool, cl_raw, cl_tasks, lm) = (&cps, &ev_pool, &cl_pool, &cl_raw, &cl_tasks, &lm);
            hs.push(s.spawn(move || {
                pin_thread(t);
                let mut sc = Scratch::default();
                let mut cout: ChunkScatter<CRec> = ChunkScatter::new(cl_pool, np);
                let mut nd = 0u64;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i < np {
                        nd += dedup_partition(cps, i, &mut sc, ev_pool, log_p, timing);
                    } else if i < np + cl_tasks.len() {
                        let (tt, s0) = cl_tasks[i - np];
                        let v = &cl_raw[tt];
                        let slice = &v[s0..(s0 + CL_TASK).min(v.len())];
                        const D: usize = 16;
                        for r in slice.iter().take(D) {
                            lm.prefetch(r.user.hash());
                        }
                        for (k, r) in slice.iter().enumerate() {
                            if let Some(a) = slice.get(k + D) {
                                lm.prefetch(a.user.hash());
                            }
                            let person = lm.get(r.user.0, r.user.hash()).unwrap_or(r.user.0);
                            cout.push(part(PKey(person).hash(), log_p), CRec { pkey: person, id: r.id, lex: click_lex(r.id), ts: r.ts, camp: r.camp, _pad: [0; 2] });
                        }
                    } else {
                        break;
                    }
                }
                dropped.fetch_add(nd, Ordering::Relaxed);
                cout.finish()
            }));
        }
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    drop(cps);
    std::thread::spawn(move || drop(cp_pool));
    drop(cl_raw);
    drop(inputs);
    drop(lm);
    let cls = PoolParts { pool: &cl_pool, lists: cl_lists };
    log(&t0, &format!("dedup + click resolution done: {} events dropped, {} clicks routed", dropped.load(Ordering::Relaxed), cls.total()));

    // Phase 3: person partitions (largest first): local ids, counting sort, per-person sort + walk.
    // Each thread keeps one Walker and then writes its own user_daily part.
    let day0 = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - day0 + 1) as usize;
    let mut order: Vec<usize> = (0..np).collect();
    let sizes: Vec<usize> = (0..np).map(|q| evs.len(q)).collect();
    order.sort_by_key(|&q| std::cmp::Reverse(sizes[q]));
    let user_dir = outdir.join("user_daily");
    std::fs::create_dir_all(&user_dir).expect("mkdir user_daily");
    let walkers: Vec<Walker> = (0..nthreads).map(|_| Walker::new(&ctx, day0, ndays)).collect();
    let mut radix_tmps: Vec<Vec<u64>> = (0..nthreads).map(|_| Vec::new()).collect();
    let next = AtomicUsize::new(0);
    let results: Vec<(Vec<CampCell>, usize)> = std::thread::scope(|s| {
        let mut hs = Vec::new();
        for (t, (w, radix_tmp)) in walkers.into_iter().zip(radix_tmps.iter_mut()).enumerate() {
            let next = &next;
            let (ctx, evs, cls, order, user_dir) = (&ctx, &evs, &cls, &order, &user_dir);
            hs.push(s.spawn(move || {
                pin_thread(t);
                let mut w = w;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= np {
                        break;
                    }
                    person_partition(order[i], evs, cls, &mut w, radix_tmp, timing);
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
            "    thread-seconds: event pread {:.3}, event scan+parse {:.3}; dedup filter {:.3}, gather {:.3}, resolve {:.3}; person ids {:.3}, counting sort {:.3}, sorts {:.3}, walk {:.3}",
            g(&T_PARSE[0]), g(&T_PARSE[1]), g(&T_DEDUP[0]), g(&T_DEDUP[1]), g(&T_DEDUP[2]), g(&T_PERSON[0]), g(&T_PERSON[1]), g(&T_PERSON[2]), g(&T_PERSON[3])
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
