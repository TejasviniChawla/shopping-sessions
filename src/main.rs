//! Shopping Sessions pipeline: one static binary, all phases parallel.

mod csv;
mod io;
mod out;
mod pq;
mod ts;
mod util;

use csv::{split_scanned, Lines, Scanner};
use io::{Buffer, Unit};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use ts::{parse_f1, parse_f1_cached, parse_ingest, parse_ts, parse_ts_cached, DAY};
use util::{pin_thread, pool_drain_parallel, prefetch, HugeVec, PKey, Scatter, Side, Sides, SmallTable};

const LOG_P: u32 = 5;
const P: usize = 1 << LOG_P;
const SESSION_GAP: i64 = 30 * ts::MINUTE;
const LATE: i64 = DAY;
const REACH: i64 = 7 * DAY;

#[inline(always)]
fn part(h: u64) -> usize {
    (h >> (64 - LOG_P)) as usize
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

#[derive(Clone, Copy, Default)]
struct EvValid {
    user: PKey,
    ts: u32,
    packed: u32,
}

#[derive(Clone, Copy, Default)]
struct EvMid {
    ts: u32,
    uid: u32,
    packed: u32,
}

/// ts (u32 ms offset) << 32 | packed: sorting the u64 sorts by ts.
type EvFinal = u64;

#[derive(Clone, Copy, Default)]
struct ClRaw {
    id: PKey,
    user: PKey,
    ts: u32, // ms since window_start - 7 days (valid only when flag bit0)
    camp: u16,
    flags: u8, // bit0 ts ok, bit1 user nonempty, bit2 campaign known
}

#[derive(Clone, Copy, Default)]
struct ClValid {
    id: PKey,
    user: PKey,
    ts: u32,
    camp: u16,
}

#[derive(Clone, Copy, Default)]
struct ClMid {
    id: PKey,
    ts: u32,
    uid: u32,
    camp: u16,
}

#[derive(Clone, Copy, Default)]
struct ClFinal {
    id: PKey,
    ts: u32,
    camp: u16,
}

#[derive(Clone, Copy, Default)]
struct LinkRaw {
    a: PKey,
    b: PKey,
}

struct Ctx {
    ws: i64,
    we: i64,
    products: SmallTable,
    campaigns: SmallTable,
    ncat: u16, // id of "unknown"
    cat_names: Vec<Vec<u8>>,
    cat_rank: Vec<u32>,
    ncamp: u16, // id of organic
    camp_names: Vec<Vec<u8>>,
    channels: Vec<Vec<u8>>,
    nthreads: usize,
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
fn parse_event_line(ctx: &Ctx, aid: usize, side: &mut Side, line: &[u8], masks: Option<(u128, u128)>) -> Option<EvRaw> {
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
        // over-read is safe: the line sits in a padded buffer
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
    Some(EvRaw { id: PKey::new(f[0], aid, side), user: PKey::new(f[1], aid, side), ts, ingest, price, cat, flags })
}

#[inline(always)]
fn parse_click_line(ctx: &Ctx, aid: usize, side: &mut Side, line: &[u8], masks: Option<(u128, u128)>) -> Option<ClRaw> {
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
    Some(ClRaw { id: PKey::new(f[0], aid, side), user: PKey::new(f[1], aid, side), ts, camp, flags })
}

/// Generic parallel parse over work units; each thread produces P partition vectors and one Side table.
fn parse_parallel<T: Copy + Default + Send, F>(
    ctx: &Ctx,
    inputs: &io::Inputs,
    units: &[Unit],
    aid_base: usize,
    f: F,
) -> (Vec<Vec<HugeVec<T>>>, Vec<Side>)
where
    F: Fn(&Ctx, usize, &mut Side, &[u8], Option<(u128, u128)>, &mut Scatter<T, P>) -> Option<(usize, T)> + Sync,
{
    let next = AtomicUsize::new(0);
    let total_bytes: u64 = units.iter().map(|u| u.end - u.start).sum();
    let est = (total_bytes / 60) as usize / ctx.nthreads / P + 1024;
    let results: Vec<(Vec<HugeVec<T>>, Side)> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for t in 0..ctx.nthreads {
            let next = &next;
            let f = &f;
            handles.push(s.spawn(move || {
                pin_thread(t);
                let aid = aid_base + t;
                let mut side = Side::new();
                let mut out: Scatter<T, P> = Scatter::new(est);
                let mut buf = Buffer::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= units.len() {
                        break;
                    }
                    let u = units[i];
                    buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
                    let (lo, hi) = (buf.lo, buf.hi);
                    let data = buf.data();
                    let mut sc = Scanner::new(data, lo, hi);
                    while let Some((line, masks)) = sc.next() {
                        if let Some((p, rec)) = f(ctx, aid, &mut side, line, masks, &mut out) {
                            out.push(p, rec);
                        }
                    }
                }
                (out.finish(), side)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut outs = Vec::with_capacity(results.len());
    let mut sides = Vec::with_capacity(results.len());
    for (o, a) in results {
        outs.push(o);
        sides.push(a);
    }
    (outs, sides)
}

// ---------- dedup ----------

/// 2-bit saturating counters, 16 slots per record: a record whose slot count is < 2 has a unique key in
/// this partition and skips the exact table. Fits in L2 for typical partition sizes.
struct Sketch {
    words: Vec<u64>,
    mask: usize,
}
impl Sketch {
    fn new(total: usize) -> Self {
        let slots = (total * 32).max(1024).next_power_of_two();
        Sketch { words: vec![0u64; slots / 32], mask: slots - 1 }
    }
    /// Reuse an existing buffer (no fresh page faults): resize and clear.
    fn reuse(mut words: Vec<u64>, total: usize) -> Self {
        let slots = (total * 32).max(1024).next_power_of_two();
        words.clear();
        words.resize(slots / 32, 0);
        Sketch { words, mask: slots - 1 }
    }
    #[inline(always)]
    fn bump(&mut self, h: u64) {
        let slot = (h as usize) & self.mask;
        let w = &mut self.words[slot >> 5];
        let sh = (slot & 31) * 2;
        let v = (*w >> sh) & 3;
        *w += ((v < 2) as u64) << sh;
    }
    #[inline(always)]
    fn count(&self, h: u64) -> u64 {
        let slot = (h as usize) & self.mask;
        (self.words[slot >> 5] >> ((slot & 31) * 2)) & 3
    }
    #[inline(always)]
    fn prefetch(&self, h: u64) {
        prefetch(unsafe { self.words.as_ptr().add(((h as usize) & self.mask) >> 5) });
    }
}

#[inline(always)]
fn event_ok(r: &EvRaw) -> bool {
    let etype = r.flags >> 4;
    (r.flags & 7) == 7 && etype < 3 && (etype != 2 || (r.flags & 8) != 0) && (r.ingest as i64) * 1000 - (r.ts as i64) <= LATE
}

static WALK_NS: [std::sync::atomic::AtomicU64; 3] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
static DEDUP_NS: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Per partition p: dedup event copies by event_id keeping min ingest, validate, scatter by user hash.
/// Per-thread scratch reused across partition tasks so the kernel does not fault fresh pages 32 times per phase.
#[derive(Default)]
struct Scratch {
    sketch: Vec<u64>,
    slots: Vec<(u32, PKey)>,
    win: Vec<u64>,
    cands: Vec<u32>,
    used: Vec<bool>,
    ckeys: Vec<PKey>,
}

fn dedup_events(sides: &Sides, raws: &[Vec<HugeVec<EvRaw>>], p: usize, sc: &mut Scratch) -> Vec<HugeVec<EvValid>> {
    let total: usize = raws.iter().map(|t| t[p].len()).sum();
    const D: usize = 16;
    let tp = Instant::now();
    // pass 1: count sketch
    let mut sk = Sketch::reuse(std::mem::take(&mut sc.sketch), total);
    for tv in raws.iter() {
        let recs = &tv[p][..];
        let n = recs.len();
        for i in 0..n.min(D) {
            sk.prefetch(recs[i].id.hash(sides));
        }
        for i in 0..n {
            if i + D < n {
                sk.prefetch(recs[i + D].id.hash(sides));
            }
            sk.bump(recs[i].id.hash(sides));
        }
    }
    DEDUP_NS[0].fetch_add(tp.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let tp = Instant::now();
    // pass 2: unique keys are winners and are emitted now; candidates are collected for a prefetched pass 3
    let skip_emit = std::env::var_os("DEDUP_SKIP_EMIT").is_some();
    let skip_cands = std::env::var_os("DEDUP_SKIP_CANDS").is_some();
    let mut out: Scatter<EvValid, P> = Scatter::new(total / P + 1024);
    let mut cands: Vec<u32> = std::mem::take(&mut sc.cands);
    cands.clear();
    let mut off = vec![0usize; raws.len() + 1];
    for t in 0..raws.len() {
        off[t + 1] = off[t] + raws[t][p].len();
    }
    for (t, tv) in raws.iter().enumerate() {
        let recs = &tv[p][..];
        let n = recs.len();
        for i in 0..n.min(D) {
            sk.prefetch(recs[i].id.hash(sides));
        }
        for (i, r) in recs.iter().enumerate() {
            if i + D < n {
                sk.prefetch(recs[i + D].id.hash(sides));
            }
            let h = r.id.hash(sides);
            if sk.count(h) < 2 || skip_cands {
                if event_ok(r) && !skip_emit {
                    out.push(part(r.user.hash(sides)), EvValid { user: r.user, ts: r.ts, packed: pack_ev(r.price, r.cat, (r.flags >> 4) == 2) });
                }
            } else {
                cands.push(((t as u32) << 27) | (i as u32));
            }
        }
    }
    DEDUP_NS[1].fetch_add(tp.elapsed().as_nanos() as u64, Ordering::Relaxed);
    DEDUP_NS[3].fetch_add(cands.len() as u64, Ordering::Relaxed);
    let tp = Instant::now();
    // pass 3: exact table over candidates only (min ingest wins), slot prefetched D ahead
    let cap = (cands.len() * 2).max(64).next_power_of_two();
    let mask = cap - 1;
    let mut slots = std::mem::take(&mut sc.slots);
    slots.clear();
    slots.resize(cap, (u32::MAX, PKey(0)));
    let mut win = std::mem::take(&mut sc.win);
    win.clear();
    win.resize((total + 63) / 64 + 1, 0);
    let get = |packed: u32| -> &EvRaw { &raws[(packed >> 27) as usize][p][(packed & 0x7FF_FFFF) as usize] };
    let nc = cands.len();
    for k in 0..nc.min(D) {
        prefetch(unsafe { slots.as_ptr().add((get(cands[k]).id.hash(sides) as usize) & mask) });
    }
    for k in 0..nc {
        if k + D < nc {
            prefetch(unsafe { slots.as_ptr().add((get(cands[k + D]).id.hash(sides) as usize) & mask) });
        }
        let packed = cands[k];
        let r = get(packed);
        let h = r.id.hash(sides);
        let dense = off[(packed >> 27) as usize] + (packed & 0x7FF_FFFF) as usize;
        let mut s = (h as usize) & mask;
        loop {
            let (cur, ck) = slots[s];
            if cur == u32::MAX {
                slots[s] = (packed, r.id);
                win[dense >> 6] |= 1 << (dense & 63);
                break;
            }
            if ck.same(&r.id, sides) {
                let o = get(cur);
                if r.ingest < o.ingest {
                    slots[s] = (packed, r.id);
                    let od = off[(cur >> 27) as usize] + (cur & 0x7FF_FFFF) as usize;
                    win[od >> 6] &= !(1 << (od & 63));
                    win[dense >> 6] |= 1 << (dense & 63);
                }
                break;
            }
            s = (s + 1) & mask;
        }
    }
    // candidate winners
    for &packed in &cands {
        let dense = off[(packed >> 27) as usize] + (packed & 0x7FF_FFFF) as usize;
        if (win[dense >> 6] >> (dense & 63)) & 1 == 0 {
            continue;
        }
        let r = get(packed);
        if event_ok(r) {
            out.push(part(r.user.hash(sides)), EvValid { user: r.user, ts: r.ts, packed: pack_ev(r.price, r.cat, (r.flags >> 4) == 2) });
        }
    }
    let o = out.finish();
    DEDUP_NS[2].fetch_add(tp.elapsed().as_nanos() as u64, Ordering::Relaxed);
    sc.sketch = sk.words;
    sc.slots = slots;
    sc.win = win;
    sc.cands = cands;
    o
}

fn dedup_clicks(sides: &Sides, raws: &[Vec<HugeVec<ClRaw>>], p: usize, sc: &mut Scratch) -> Vec<HugeVec<ClValid>> {
    let total: usize = raws.iter().map(|t| t[p].len()).sum();
    const D: usize = 16;
    let mut sk = Sketch::reuse(std::mem::take(&mut sc.sketch), total);
    for tv in raws.iter() {
        let recs = &tv[p][..];
        let n = recs.len();
        for i in 0..n.min(D) {
            sk.prefetch(recs[i].id.hash(sides));
        }
        for i in 0..n {
            if i + D < n {
                sk.prefetch(recs[i + D].id.hash(sides));
            }
            sk.bump(recs[i].id.hash(sides));
        }
    }
    let cap = (total / 4).max(64).next_power_of_two();
    let mask = cap - 1;
    let mut slots = std::mem::take(&mut sc.ckeys);
    slots.clear();
    slots.resize(cap, PKey(0));
    let mut used = std::mem::take(&mut sc.used);
    used.clear();
    used.resize(cap, false);
    let mut out: Scatter<ClValid, P> = Scatter::new(total / P + 1024);
    for tv in raws.iter() {
        let recs = &tv[p][..];
        let n = recs.len();
        for i in 0..n.min(D) {
            sk.prefetch(recs[i].id.hash(sides));
        }
        for (i, r) in recs.iter().enumerate() {
            if i + D < n {
                sk.prefetch(recs[i + D].id.hash(sides));
            }
            let h = r.id.hash(sides);
            let fresh = if sk.count(h) < 2 {
                true
            } else {
                let mut s = (h as usize) & mask;
                loop {
                    if !used[s] {
                        used[s] = true;
                        slots[s] = r.id;
                        break true;
                    }
                    if slots[s].same(&r.id, sides) {
                        break false;
                    }
                    s = (s + 1) & mask;
                }
            };
            if fresh && (r.flags & 7) == 7 {
                out.push(part(r.user.hash(sides)), ClValid { id: r.id, user: r.user, ts: r.ts, camp: r.camp });
            }
        }
    }
    sc.sketch = sk.words;
    sc.ckeys = slots;
    sc.used = used;
    out.finish()
}

// ---------- user interning ----------

struct Intern {
    slots: HugeVec<u32>,
    keys: Vec<PKey>,
    mask: usize,
}

impl Intern {
    fn new(est: usize) -> Self {
        let cap = (est * 2).max(64).next_power_of_two();
        let mut slots: HugeVec<u32> = HugeVec::with_len_uninit(cap);
        slots.fill(u32::MAX);
        Intern { slots, keys: Vec::with_capacity(est), mask: cap - 1 }
    }
    fn grow(&mut self, sides: &Sides) {
        let cap = self.slots.len() * 2;
        let mask = cap - 1;
        let mut slots: HugeVec<u32> = HugeVec::with_len_uninit(cap);
        slots.fill(u32::MAX);
        for (i, k) in self.keys.iter().enumerate() {
            let mut s = (k.hash(sides) as usize) & mask;
            while slots[s] != u32::MAX {
                s = (s + 1) & mask;
            }
            slots[s] = i as u32;
        }
        self.slots = slots;
        self.mask = mask;
    }
    #[inline]
    fn insert(&mut self, k: PKey, sides: &Sides) -> u32 {
        if self.keys.len() * 4 >= self.slots.len() * 3 {
            self.grow(sides);
        }
        let mut s = (k.hash(sides) as usize) & self.mask;
        loop {
            let cur = self.slots[s];
            if cur == u32::MAX {
                self.slots[s] = self.keys.len() as u32;
                self.keys.push(k);
                return (self.keys.len() - 1) as u32;
            }
            if self.keys[cur as usize].same(&k, sides) {
                return cur;
            }
            s = (s + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn prefetch(&self, k: &PKey, sides: &Sides) {
        prefetch(unsafe { self.slots.as_ptr().add((k.hash(sides) as usize) & self.mask) });
    }
    #[inline]
    fn lookup(&self, k: &PKey, sides: &Sides) -> Option<u32> {
        let mut s = (k.hash(sides) as usize) & self.mask;
        loop {
            let cur = self.slots[s];
            if cur == u32::MAX {
                return None;
            }
            if self.keys[cur as usize].same(k, sides) {
                return Some(cur);
            }
            s = (s + 1) & self.mask;
        }
    }
}

struct Partition {
    table: Intern,
    events: HugeVec<EvMid>,
    clicks: HugeVec<ClMid>,
    /// per local uid: number of events / clicks (the person histogram is derived from these)
    ev_cnt: Vec<u32>,
    cl_cnt: Vec<u32>,
}

fn intern_partition(
    sides: &Sides,
    links: &[Vec<HugeVec<LinkRaw>>],
    evv: &[Vec<HugeVec<EvValid>>],
    clv: &[Vec<HugeVec<ClValid>>],
    q: usize,
) -> Partition {
    let nev: usize = evv.iter().map(|v| v[q].len()).sum();
    let ncl: usize = clv.iter().map(|v| v[q].len()).sum();
    let nl: usize = links.iter().map(|t| t[q].len()).sum();
    // ~1 ID per 13 events in the generator's data; the table grows if a dataset has more
    let mut table = Intern::new(nl + nev / 12 + ncl / 12 + 64);
    for t in links {
        for l in t[q].iter() {
            table.insert(l.a, sides);
        }
    }
    const D: usize = 8;
    let mut ev_cnt: Vec<u32> = vec![0; table.keys.len()];
    let mut events: HugeVec<EvMid> = HugeVec::with_capacity(nev);
    for v in evv {
        let recs = &v[q][..];
        let n = recs.len();
        for i in 0..n.min(D) {
            table.prefetch(&recs[i].user, sides);
        }
        for (i, e) in recs.iter().enumerate() {
            if i + D < n {
                table.prefetch(&recs[i + D].user, sides);
            }
            let uid = table.insert(e.user, sides);
            if uid as usize >= ev_cnt.len() {
                ev_cnt.resize(uid as usize + 1, 0);
            }
            ev_cnt[uid as usize] += 1;
            events.push(EvMid { ts: e.ts, uid, packed: e.packed });
        }
    }
    let mut cl_cnt: Vec<u32> = vec![0; table.keys.len()];
    let mut clicks: HugeVec<ClMid> = HugeVec::with_capacity(ncl);
    for v in clv {
        let recs = &v[q][..];
        let n = recs.len();
        for i in 0..n.min(D) {
            table.prefetch(&recs[i].user, sides);
        }
        for (i, c) in recs.iter().enumerate() {
            if i + D < n {
                table.prefetch(&recs[i + D].user, sides);
            }
            let uid = table.insert(c.user, sides);
            if uid as usize >= cl_cnt.len() {
                cl_cnt.resize(uid as usize + 1, 0);
            }
            cl_cnt[uid as usize] += 1;
            clicks.push(ClMid { id: c.id, ts: c.ts, uid, camp: c.camp });
        }
    }
    ev_cnt.resize(table.keys.len(), 0);
    cl_cnt.resize(table.keys.len(), 0);
    Partition { table, events, clicks, ev_cnt, cl_cnt }
}

// ---------- union find ----------

#[inline]
fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let gp = parent[parent[x as usize] as usize];
        parent[x as usize] = gp;
        x = gp;
    }
    x
}

/// LSD radix sort of u64 keys on the top 32 bits only (the timestamp); the low 32 bits need no order.
/// Four 8-bit passes between `v` and `tmp`.
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
        // skip a pass where every key shares the byte
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

// ---------- per-person walk ----------

#[derive(Clone, Copy)]
struct URow {
    person: u32,
    day: i32,
    sessions: u32,
    events: u32,
    revenue: i64,
    top_cat: u16,
}

struct Walker<'a> {
    ctx: &'a Ctx,
    sides: &'a Sides,
    day0: i64,
    ndays: usize,
    cat_sum: Vec<i64>,
    touched: Vec<u16>,
    mark: Vec<u32>,
    serial: u32,
    rows: Vec<URow>,
    camp: Vec<[i64; 4]>, // [sessions, purchasing, revenue, assisted] indexed camp*ndays+day
}

impl<'a> Walker<'a> {
    fn new(ctx: &'a Ctx, sides: &'a Sides, day0: i64, ndays: usize) -> Self {
        Walker {
            ctx,
            sides,
            day0,
            ndays,
            cat_sum: vec![0; ctx.ncat as usize + 1],
            touched: Vec::new(),
            mark: vec![u32::MAX; ctx.ncamp as usize + 1],
            serial: 0,
            rows: Vec::new(),
            camp: vec![[0; 4]; (ctx.ncamp as usize + 1) * ndays],
        }
    }

    fn flush(&mut self, person: u32, day: i64, sessions: u32, events: u32, revenue: i64) {
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
        self.rows.push(URow { person, day: day as i32, sessions, events, revenue, top_cat: top });
    }

    /// `ev` sorted by ts; `cl` sorted by (ts, id bytes). Timestamps are u32 offsets: events from ws, clicks from ws-7d.
    fn walk(&mut self, person: u32, ev: &[EvFinal], cl: &[ClFinal]) {
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
                    self.flush(person, cur_day, d_sessions, d_events, d_revenue);
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
            // attribution: click window [start - 7d, start]
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
            cell[0] += 1;
            cell[1] += purch as i64;
            cell[2] += rev;
            self.serial += 1;
            let serial = self.serial;
            for c in &cl[lo..hi] {
                let ci = c.camp as usize;
                if self.mark[ci] != serial {
                    self.mark[ci] = serial;
                    self.camp[ci * ndays + di][3] += 1;
                }
            }
            i = j;
        }
        if cur_day != i64::MIN {
            self.flush(person, cur_day, d_sessions, d_events, d_revenue);
        }
    }
}

/// Micro-benchmark of the parse stages on the event files, single-threaded. Enabled with BENCH=1.
/// Every stage keeps its results alive with black_box so nothing is optimized away.
fn bench(ctx: &Ctx, inputs: &io::Inputs) {
    use std::hint::black_box;
    let units = &inputs.events;
    let mut buf = Buffer::new();
    macro_rules! stage {
        ($name:expr, |$line:ident, $m:ident, $side:ident| $body:block) => {{
            let t = Instant::now();
            let mut $side = Side::new();
            let mut lines = 0usize;
            for &u in units {
                buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
                let mut sc = Scanner::new(buf.data(), buf.lo, buf.hi);
                while let Some(($line, $m)) = sc.next() {
                    lines += 1;
                    $body
                }
            }
            eprintln!("bench {:<28} {:.3}s  ({} lines)", $name, t.elapsed().as_secs_f64(), lines);
        }};
    }
    stage!("read+scan", |l, m, _sd| { black_box(l.len()); black_box(m); });
    stage!("+split", |l, m, _sd| {
        let mut f = [&b""[..]; 7];
        if split_scanned(l, m, &mut f) { black_box(&f); }
    });
    stage!("+split+ts", |l, m, sd| {
        let mut f = [&b""[..]; 7];
        if split_scanned(l, m, &mut f) { black_box(parse_ts_cached(f[2], &mut sd.dates)); }
    });
    stage!("+split+ts+ingest", |l, m, sd| {
        let mut f = [&b""[..]; 7];
        if split_scanned(l, m, &mut f) {
            black_box(parse_ts_cached(f[2], &mut sd.dates));
            black_box(parse_ingest(f[3], &mut sd.dates));
        }
    });
    stage!("+split+keys", |l, m, sd| {
        let mut f = [&b""[..]; 7];
        if split_scanned(l, m, &mut f) {
            black_box(PKey::new(f[0], 0, &mut sd));
            black_box(PKey::new(f[1], 0, &mut sd));
        }
    });
    stage!("+split+type+price+hash", |l, m, sd| {
        let mut f = [&b""[..]; 7];
        if split_scanned(l, m, &mut f) {
            let t = f[4];
            let w = unsafe { std::ptr::read_unaligned(t.as_ptr() as *const u64) };
            let et: u8 = if t.len() == 4 && (w as u32) == u32::from_le_bytes(*b"view") { 0 }
                else if t.len() == 4 && (w as u32) == u32::from_le_bytes(*b"cart") { 1 }
                else if t.len() == 8 && w == u64::from_le_bytes(*b"purchase") { 2 } else { 3 };
            black_box(et);
            if et == 2 { black_box(parse_price(f[6])); black_box(ctx.products.get(f[5])); }
            let k = PKey::new(f[0], 0, &mut sd);
            black_box(part(k.hash_in(&sd)));
        }
    });
    stage!("full parse_event_line", |l, m, sd| { black_box(parse_event_line(ctx, 0, &mut sd, l, m)); });
    {
        let t = Instant::now();
        let mut side = Side::new();
        let mut out: Scatter<EvRaw, P> = Scatter::new(1 << 16);
        let mut n = 0usize;
        for &u in units {
            buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
            let mut sc = Scanner::new(buf.data(), buf.lo, buf.hi);
            while let Some((l, m)) = sc.next() {
                if let Some(r) = parse_event_line(ctx, 0, &mut side, l, m) {
                    out.push(part(r.id.hash_in(&side)), r);
                    n += 1;
                }
            }
        }
        let o = out.finish();
        eprintln!("bench {:<28} {:.3}s  ({} records, {} vecs)", "full + scatter push", t.elapsed().as_secs_f64(), n, o.len());
    }
}

// ---------- main ----------

fn main() {
    // 1BRC trick: a child does the work; the parent exits the moment the child reports success, so the
    // kernel's teardown of the child's address space is not on the container's clock.
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
        return; // measures container start/stop overhead alone
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

    // categories
    let mut cat_names: Vec<Vec<u8>> = Vec::new();
    let mut cat_ids = SmallTable::with_capacity(64);
    let mut products = SmallTable::with_capacity(prods.len());
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
    let mut campaigns = SmallTable::with_capacity(camps.len());
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
    let ctx = Ctx { ws, we, products, campaigns, ncat, cat_names, cat_rank, ncamp, camp_names, channels, nthreads };
    log(&t0, &format!("setup: {} files, {} threads, {} event units", inputs.files.len(), nthreads, inputs.events.len()));
    if std::env::var("BENCH").is_ok() {
        bench(&ctx, &inputs);
        return;
    }

    // Phase 1: parse
    let (ev_raw, sd_e) = parse_parallel(&ctx, &inputs, &inputs.events, 0, |c, aid, side, line, masks, _| {
        parse_event_line(c, aid, side, line, masks).map(|r| (part(r.id.hash_in(side)), r))
    });
    log(&t0, "events parsed");
    // Phase 2 (overlapped): event dedup partitions, click parse units and link parse units share one task queue.
    // Event dedup only needs the event parse; clicks and links are parsed by whichever threads are free.
    let mut sides_v: Vec<Side> = Vec::with_capacity(3 * nthreads);
    sides_v.extend(sd_e);
    for _ in 0..2 * nthreads {
        sides_v.push(Side::new());
    }
    let n_cu = inputs.clicks.len();
    let n_lu = inputs.links.len();
    let cl_est = (inputs.clicks.iter().map(|u| u.end - u.start).sum::<u64>() / 40) as usize / nthreads / P + 1024;
    let lk_est = (inputs.links.iter().map(|u| u.end - u.start).sum::<u64>() / 20) as usize / nthreads / P + 1024;
    let next = AtomicUsize::new(0);
    let mut evv: Vec<Option<Vec<HugeVec<EvValid>>>> = (0..P).map(|_| None).collect();
    struct Phase2Out {
        clicks: Vec<HugeVec<ClRaw>>,
        links: Vec<HugeVec<LinkRaw>>,
        cside: Side,
        lside: Side,
    }
    let p2: Vec<Phase2Out> = {
        let evp = Shared(evv.as_mut_ptr());
        let sides: &Sides = &sides_v[..];
        std::thread::scope(|s| {
            let mut hs = Vec::new();
            for t in 0..nthreads {
                let next = &next;
                let (ctx, inputs, ev_raw) = (&ctx, &inputs, &ev_raw);
                hs.push(s.spawn(move || {
                    pin_thread(t);
                    let mut sc = Scratch::default();
                    let mut cout: Scatter<ClRaw, P> = Scatter::new(cl_est);
                    let mut lout: Scatter<LinkRaw, P> = Scatter::new(lk_est);
                    let mut cside = Side::new();
                    let mut lside = Side::new();
                    let mut buf = Buffer::new();
                    let (caid, laid) = (nthreads + t, 2 * nthreads + t);
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i < P {
                            let r = dedup_events(sides, ev_raw, i, &mut sc);
                            unsafe { *evp.p().add(i) = Some(r) };
                        } else if i < P + n_cu {
                            let u = inputs.clicks[i - P];
                            buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
                            let (lo, hi) = (buf.lo, buf.hi);
                            let data = buf.data();
                            let mut scn = Scanner::new(data, lo, hi);
                            while let Some((line, masks)) = scn.next() {
                                if let Some(r) = parse_click_line(ctx, caid, &mut cside, line, masks) {
                                    cout.push(part(r.id.hash_in(&cside)), r);
                                }
                            }
                        } else if i < P + n_cu + n_lu {
                            let u = inputs.links[i - P - n_cu];
                            buf.load(&inputs.files[u.file as usize], inputs.sizes[u.file as usize], u);
                            let (lo, hi) = (buf.lo, buf.hi);
                            let data = buf.data();
                            let mut scn = Scanner::new(data, lo, hi);
                            while let Some((line, masks)) = scn.next() {
                                let mut f = [&b""[..]; 2];
                                if !split_scanned(line, masks, &mut f) || f[0].is_empty() || f[1].is_empty() {
                                    continue;
                                }
                                let a = PKey::new(f[0], laid, &mut lside);
                                let b = PKey::new(f[1], laid, &mut lside);
                                let (pa, pb) = (part(a.hash_in(&lside)), part(b.hash_in(&lside)));
                                lout.push(pb, LinkRaw { a: b, b: a });
                                lout.push(pa, LinkRaw { a, b });
                            }
                        } else {
                            break;
                        }
                    }
                    Phase2Out { clicks: cout.finish(), links: lout.finish(), cside, lside }
                }));
            }
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        })
    };
    let mut cl_raw: Vec<Vec<HugeVec<ClRaw>>> = Vec::with_capacity(nthreads);
    let mut links: Vec<Vec<HugeVec<LinkRaw>>> = Vec::with_capacity(nthreads);
    for (t, o) in p2.into_iter().enumerate() {
        cl_raw.push(o.clicks);
        links.push(o.links);
        sides_v[nthreads + t] = o.cside;
        sides_v[2 * nthreads + t] = o.lside;
    }
    let sides: &Sides = &sides_v[..];
    drop(inputs);
    log(&t0, "event dedup + clicks/links parse done");
    // click dedup
    let next = AtomicUsize::new(0);
    let mut clv: Vec<Option<Vec<HugeVec<ClValid>>>> = (0..P).map(|_| None).collect();
    {
        let clp = Shared(clv.as_mut_ptr());
        std::thread::scope(|s| {
            for t in 0..nthreads {
                let next = &next;
                let cl_raw = &cl_raw;
                s.spawn(move || {
                    pin_thread(t);
                    let mut sc = Scratch::default();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= P {
                            break;
                        }
                        let r = dedup_clicks(sides, cl_raw, i, &mut sc);
                        unsafe { *clp.p().add(i) = Some(r) };
                    }
                });
            }
        });
    }
    drop(ev_raw);
    drop(cl_raw);
    let evv: Vec<Vec<HugeVec<EvValid>>> = evv.into_iter().map(|x| x.unwrap()).collect();
    let clv: Vec<Vec<HugeVec<ClValid>>> = clv.into_iter().map(|x| x.unwrap()).collect();
    log(
        &t0,
        &format!(
            "dedup+validate done (thread-seconds: sketch {:.3}, pass2 {:.3}, pass3 {:.3}; candidates {})",
            DEDUP_NS[0].load(Ordering::Relaxed) as f64 / 1e9,
            DEDUP_NS[1].load(Ordering::Relaxed) as f64 / 1e9,
            DEDUP_NS[2].load(Ordering::Relaxed) as f64 / 1e9,
            DEDUP_NS[3].load(Ordering::Relaxed)
        ),
    );

    // Phase 3: intern users per partition
    let next = AtomicUsize::new(0);
    let mut parts: Vec<Option<Partition>> = (0..P).map(|_| None).collect();
    {
        let pp = Shared(parts.as_mut_ptr());
        std::thread::scope(|s| {
            for _ in 0..nthreads {
                let next = &next;
                let (links, evv, clv) = (&links, &evv, &clv);
                s.spawn(move || loop {
                    let q = next.fetch_add(1, Ordering::Relaxed);
                    if q >= P {
                        break;
                    }
                    let r = intern_partition(sides, links, evv, clv, q);
                    unsafe { *pp.p().add(q) = Some(r) };
                });
            }
        });
    }
    drop(evv);
    drop(clv);
    let parts: Vec<Partition> = parts.into_iter().map(|x| x.unwrap()).collect();
    let mut base = vec![0u32; P + 1];
    for q in 0..P {
        base[q + 1] = base[q] + parts[q].table.keys.len() as u32;
    }
    let n_ids = base[P] as usize;
    log(&t0, &format!("interned {} user ids", n_ids));

    // Phase 3b: resolve link endpoints in parallel (read-only tables), then a sequential union-find
    let edges: Vec<Vec<(u32, u32)>> = {
        let next = AtomicUsize::new(0);
        let mut es: Vec<Option<Vec<(u32, u32)>>> = (0..P).map(|_| None).collect();
        let ep = Shared(es.as_mut_ptr());
        std::thread::scope(|s| {
            for _ in 0..nthreads {
                let next = &next;
                let (links, parts, base) = (&links, &parts, &base);
                s.spawn(move || loop {
                    let q = next.fetch_add(1, Ordering::Relaxed);
                    if q >= P {
                        break;
                    }
                    let mut v = Vec::new();
                    for t in links.iter() {
                        for l in t[q].iter() {
                            let qb = part(l.b.hash(sides));
                            let ua = base[q] + parts[q].table.lookup(&l.a, sides).expect("link a");
                            let ub = base[qb] + parts[qb].table.lookup(&l.b, sides).expect("link b");
                            v.push((ua, ub));
                        }
                    }
                    unsafe { *ep.p().add(q) = Some(v) };
                });
            }
        });
        es.into_iter().map(|x| x.unwrap()).collect()
    };
    drop(links);
    let mut parent: Vec<u32> = (0..n_ids as u32).collect();
    for v in &edges {
        for &(ua, ub) in v {
            let ra = find(&mut parent, ua);
            let rb = find(&mut parent, ub);
            if ra != rb {
                if ra < rb {
                    parent[rb as usize] = ra;
                } else {
                    parent[ra as usize] = rb;
                }
            }
        }
    }
    drop(edges);
    let mut id_key: Vec<PKey> = Vec::with_capacity(n_ids);
    for q in 0..P {
        id_key.extend_from_slice(&parts[q].table.keys);
    }
    // roots and person indexes (sequential, cheap), then names by parallel atomic-min over lex keys
    let mut person_of_uid = vec![u32::MAX; n_ids];
    let mut root_of_person: Vec<u32> = Vec::new();
    for u in 0..n_ids as u32 {
        let r = find(&mut parent, u);
        if person_of_uid[r as usize] == u32::MAX {
            person_of_uid[r as usize] = root_of_person.len() as u32;
            root_of_person.push(r);
        }
        person_of_uid[u as usize] = person_of_uid[r as usize];
    }
    let np = root_of_person.len();
    // packed keys: numeric min of lex_key; fallback keys handled sequentially afterwards
    let best: Vec<std::sync::atomic::AtomicU64> = (0..np).map(|_| std::sync::atomic::AtomicU64::new(u64::MAX)).collect();
    let fallback_ids: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
    {
        let chunk = (n_ids + nthreads - 1) / nthreads;
        std::thread::scope(|s| {
            for t in 0..nthreads {
                let (best, fallback_ids, id_key, person_of_uid) = (&best, &fallback_ids, &id_key, &person_of_uid);
                s.spawn(move || {
                    let lo = t * chunk;
                    let hi = ((t + 1) * chunk).min(n_ids);
                    let mut fb = Vec::new();
                    for u in lo..hi {
                        let k = id_key[u];
                        if k.is_packed() {
                            best[person_of_uid[u] as usize].fetch_min(k.lex_key(), Ordering::Relaxed);
                        } else {
                            fb.push(u as u32);
                        }
                    }
                    if !fb.is_empty() {
                        fallback_ids.lock().unwrap().extend(fb);
                    }
                });
            }
        });
    }
    let mut person_name: Vec<PKey> = Vec::with_capacity(np);
    for pi in 0..np {
        let b = best[pi].load(Ordering::Relaxed);
        person_name.push(if b == u64::MAX { PKey(u64::MAX) } else { PKey::from_lex_key(b) });
    }
    for &u in fallback_ids.lock().unwrap().iter() {
        let pi = person_of_uid[u as usize] as usize;
        let cur = person_name[pi];
        if cur.0 == u64::MAX || id_key[u as usize].cmp_bytes(&cur, sides) == std::cmp::Ordering::Less {
            person_name[pi] = id_key[u as usize];
        }
    }
    let n_persons = person_name.len();
    drop(parent);
    log(&t0, &format!("{} persons", n_persons));

    // Phase 4: counting sort events and clicks by person
    let n_ev: usize = parts.iter().map(|p| p.events.len()).sum();
    let n_cl: usize = parts.iter().map(|p| p.clicks.len()).sum();
    let (ev_final, ev_start) = scatter_by_person(
        &ctx,
        n_persons,
        n_ev,
        &person_of_uid,
        &base,
        |q| &parts[q].events[..],
        |q| &parts[q].ev_cnt[..],
        |e| e.uid,
        |e| ((e.ts as u64) << 32) | e.packed as u64,
    );
    let (cl_final, cl_start) = scatter_by_person(
        &ctx,
        n_persons,
        n_cl,
        &person_of_uid,
        &base,
        |q| &parts[q].clicks[..],
        |q| &parts[q].cl_cnt[..],
        |c| c.uid,
        |c| ClFinal { id: c.id, ts: c.ts, camp: c.camp },
    );
    drop(parts);
    log(&t0, &format!("grouped {} events, {} clicks", n_ev, n_cl));

    // Phase 5: per-person walk, each thread writes its own user_daily part
    let day0 = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - day0 + 1) as usize;
    let next = AtomicUsize::new(0);
    const BLOCK: usize = 512;
    let user_dir = outdir.join("user_daily");
    std::fs::create_dir_all(&user_dir).expect("mkdir user_daily");
    let results: Vec<(Vec<[i64; 4]>, usize)> = std::thread::scope(|s| {
        let mut hs = Vec::new();
        let evp = Shared(ev_final.as_ptr() as *mut EvFinal);
        let clp = Shared(cl_final.as_ptr() as *mut ClFinal);
        for t in 0..nthreads {
            let next = &next;
            let ctx = &ctx;
            let (ev_start, cl_start) = (&ev_start, &cl_start);
            let (person_name, user_dir) = (&person_name, &user_dir);
            let t0 = &t0;
            hs.push(s.spawn(move || {
                pin_thread(t);
                let (mut sort_ns, mut walk_ns) = (0u64, 0u64);
                let mut radix_tmp: Vec<u64> = Vec::new();
                let mut w = Walker::new(ctx, sides, day0, ndays);
                loop {
                    let b = next.fetch_add(1, Ordering::Relaxed) * BLOCK;
                    if b >= n_persons {
                        break;
                    }
                    for p in b..(b + BLOCK).min(n_persons) {
                        let (es, ee) = (ev_start[p] as usize, ev_start[p + 1] as usize);
                        if es == ee {
                            continue;
                        }
                        let (cs, ce) = (cl_start[p] as usize, cl_start[p + 1] as usize);
                        let ev = unsafe { std::slice::from_raw_parts_mut(evp.p().add(es), ee - es) };
                        let cl = unsafe { std::slice::from_raw_parts_mut(clp.p().add(cs), ce - cs) };
                        let ts0 = Instant::now();
                        if ev.len() >= 4096 {
                            radix_sort_u64(ev, &mut radix_tmp);
                        } else {
                            ev.sort_unstable();
                        }
                        cl.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp_bytes(&b.id, sides)));
                        sort_ns += ts0.elapsed().as_nanos() as u64;
                        let tw0 = Instant::now();
                        w.walk(p as u32, ev, cl);
                        walk_ns += tw0.elapsed().as_nanos() as u64;
                    }
                }
                WALK_NS[0].fetch_add(sort_ns, Ordering::Relaxed);
                WALK_NS[1].fetch_add(walk_ns, Ordering::Relaxed);
                let tp0 = Instant::now();
                if t == 0 {
                    eprintln!("[{:7.3}s] thread 0 walk done", t0.elapsed().as_secs_f64());
                }
                let n_rows = w.rows.len();
                if n_rows > 0 || t == 0 {
                    let mut uc = out::UserDailyCols::new(n_rows);
                    let mut buf = [0u8; 12];
                    for r in &w.rows {
                        let name = person_name[r.person as usize].bytes(sides, &mut buf);
                        uc.push_user(name);
                        uc.day.push(r.day);
                        uc.sessions.push(r.sessions as i32);
                        uc.events.push(r.events as i32);
                        uc.revenue.push(r.revenue);
                        uc.push_cat(if r.top_cat == u16::MAX { None } else { Some(&ctx.cat_names[r.top_cat as usize][..]) });
                    }
                    out::write_user_daily(&user_dir.join(format!("part-{}.parquet", t)), uc).expect("write user_daily");
                }
                WALK_NS[2].fetch_add(tp0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                (w.camp, n_rows)
            }));
        }
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    log(
        &t0,
        &format!(
            "sessions+attribution+user_daily done (thread-seconds: sort {:.3}, walk {:.3}, parquet {:.3})",
            WALK_NS[0].load(Ordering::Relaxed) as f64 / 1e9,
            WALK_NS[1].load(Ordering::Relaxed) as f64 / 1e9,
            WALK_NS[2].load(Ordering::Relaxed) as f64 / 1e9
        ),
    );

    // campaign_daily
    let n_rows: usize = results.iter().map(|r| r.1).sum();
    let ncells = (ctx.ncamp as usize + 1) * ndays;
    let mut camp = vec![[0i64; 4]; ncells];
    for (wc, _) in &results {
        for (i, c) in wc.iter().enumerate() {
            for k in 0..4 {
                camp[i][k] += c[k];
            }
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
    // release the big arrays, then unmap the pool in parallel so exit is cheap
    drop(ev_final);
    drop(cl_final);
    pool_drain_parallel(nthreads);
    log(&t0, "memory released");
}

/// Counting-sort records from P partition vectors into one array grouped by person. Returns (array, start offsets).
/// Thread t owns the source partitions q with q % nthreads == t, so there are nthreads histograms, not P.
fn scatter_by_person<'a, S: Copy + Sync + 'a, T: Copy + Default + Send + Sync>(
    ctx: &Ctx,
    n_persons: usize,
    n: usize,
    person_of_uid: &[u32],
    base: &[u32],
    src: impl Fn(usize) -> &'a [S] + Sync + Send + Copy,
    cnt_of: impl Fn(usize) -> &'a [u32] + Sync + Send + Copy,
    uid_of: impl Fn(&S) -> u32 + Sync + Send + Copy,
    conv: impl Fn(&S) -> T + Sync + Send + Copy,
) -> (HugeVec<T>, Vec<u32>) {
    let nt = ctx.nthreads;
    // per-thread person histograms from the per-uid counts gathered while interning (no pass over records)
    let mut hist: Vec<Vec<u32>> = Vec::with_capacity(nt);
    {
        let mut hs: Vec<Option<Vec<u32>>> = (0..nt).map(|_| None).collect();
        let hp = Shared(hs.as_mut_ptr());
        std::thread::scope(|s| {
            for t in 0..nt {
                s.spawn(move || {
                    let mut h = vec![0u32; n_persons];
                    let mut q = t;
                    while q < P {
                        let b = base[q] as usize;
                        for (u, &c) in cnt_of(q).iter().enumerate() {
                            if c != 0 {
                                h[person_of_uid[b + u] as usize] += c;
                            }
                        }
                        q += nt;
                    }
                    unsafe { *hp.p().add(t) = Some(h) };
                });
            }
        });
        for h in hs {
            hist.push(h.unwrap());
        }
    }
    let mut start = vec![0u32; n_persons + 1];
    for p in 0..n_persons {
        let mut tot = 0u32;
        for t in 0..nt {
            let c = hist[t][p];
            hist[t][p] = start[p] + tot;
            tot += c;
        }
        start[p + 1] = start[p] + tot;
    }
    debug_assert_eq!(start[n_persons] as usize, n);
    let mut out: HugeVec<T> = HugeVec::with_len_uninit(n);
    {
        let op = Shared(out.as_mut_ptr());
        let hist = &mut hist;
        let hp = Shared(hist.as_mut_ptr());
        std::thread::scope(|s| {
            for t in 0..nt {
                s.spawn(move || {
                    let pos: &mut Vec<u32> = unsafe { &mut *hp.p().add(t) };
                    let mut q = t;
                    while q < P {
                        let b = base[q] as usize;
                        for r in src(q) {
                            let p = person_of_uid[b + uid_of(r) as usize] as usize;
                            let d = pos[p];
                            pos[p] += 1;
                            unsafe { *op.p().add(d as usize) = conv(r) };
                        }
                        q += nt;
                    }
                });
            }
        });
    }
    (out, start)
}
