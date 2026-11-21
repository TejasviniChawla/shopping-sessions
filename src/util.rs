//! ID keys, hashing, and small open-addressing tables.

#[inline(always)]
pub fn hash_bytes(b: &[u8]) -> u64 {
    const K1: u64 = 0x9E37_79B9_7F4A_7C15;
    const K2: u64 = 0xD6E8_FEB8_6659_FD93;
    let mut h: u64 = K1 ^ (b.len() as u64).wrapping_mul(K2);
    let mut p = b;
    while p.len() >= 8 {
        let w = u64::from_le_bytes([p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]]);
        h = (h ^ w).wrapping_mul(K2);
        h ^= h >> 32;
        p = &p[8..];
    }
    if !p.is_empty() {
        let mut w: u64 = 0;
        for (i, &c) in p.iter().enumerate() {
            w |= (c as u64) << (8 * i);
        }
        h = (h ^ w).wrapping_mul(K2);
        h ^= h >> 32;
    }
    h = h.wrapping_mul(K1);
    h ^= h >> 29;
    h
}

/// Open-addressing table mapping byte keys to u32 values; keys are owned (small lookups: products, campaigns).
pub struct SmallTable {
    slots: Vec<u32>, // index+1 into entries, 0 = empty
    entries: Vec<(u64, Vec<u8>, u32)>,
    mask: usize,
}

impl SmallTable {
    pub fn with_capacity(n: usize) -> Self {
        let cap = (n * 2).max(16).next_power_of_two();
        SmallTable { slots: vec![0; cap], entries: Vec::with_capacity(n), mask: cap - 1 }
    }
    pub fn insert(&mut self, key: &[u8], value: u32) {
        let h = hash_bytes(key);
        let mut i = (h as usize) & self.mask;
        loop {
            let s = self.slots[i];
            if s == 0 {
                self.entries.push((h, key.to_vec(), value));
                self.slots[i] = self.entries.len() as u32;
                return;
            }
            let e = &self.entries[s as usize - 1];
            if e.0 == h && e.1 == key {
                return; // keep first
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline]
    pub fn get(&self, key: &[u8]) -> Option<u32> {
        let h = hash_bytes(key);
        let mut i = (h as usize) & self.mask;
        loop {
            let s = self.slots[i];
            if s == 0 {
                return None;
            }
            let e = &self.entries[s as usize - 1];
            if e.0 == h && e.1 == key {
                return Some(e.2);
            }
            i = (i + 1) & self.mask;
        }
    }
}

/// Thread-local scratch kept alongside parsing (the date cache).
pub struct Side {
    pub dates: crate::ts::DateCache,
}
impl Side {
    pub fn new() -> Self {
        Side { dates: [Default::default(); 32] }
    }
}

/// Global canonical registry for IDs that do not pack into a u64 (rare: uppercase, spaces, >12 chars, ...).
/// Equal strings get equal indexes from any thread, so a PKey compares by its u64 alone.
struct Fallback {
    map: std::collections::HashMap<Vec<u8>, u32>,
    keys: Vec<Vec<u8>>,
}
static FALLBACK: std::sync::Mutex<Option<Fallback>> = std::sync::Mutex::new(None);

fn fallback_intern(s: &[u8]) -> u32 {
    let mut g = FALLBACK.lock().unwrap();
    let fb = g.get_or_insert_with(|| Fallback { map: std::collections::HashMap::new(), keys: Vec::new() });
    if let Some(&i) = fb.map.get(s) {
        return i;
    }
    let i = fb.keys.len() as u32;
    fb.keys.push(s.to_vec());
    fb.map.insert(s.to_vec(), i);
    i
}

fn fallback_bytes(i: u32) -> Vec<u8> {
    let g = FALLBACK.lock().unwrap();
    g.as_ref().unwrap().keys[i as usize].clone()
}

/// A packed ID: lowercase base-36 strings of 1..=12 chars are encoded as a base-37 number (digits 1..=36),
/// which is injective and fits in 63 bits. Anything else is interned in the global registry and referenced
/// as (1<<63 | index). Two keys are equal iff their u64s are equal. A key is never 0.
#[derive(Clone, Copy, PartialEq, Eq, Default, Hash, Debug)]
#[repr(transparent)]
pub struct PKey(pub u64);

static CHAR_VAL: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 10 {
        t[b'0' as usize + i] = (i + 1) as u8;
        i += 1;
    }
    let mut j = 0;
    while j < 26 {
        t[b'a' as usize + j] = (j + 11) as u8;
        j += 1;
    }
    t
};
const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Base-37 digit values (1..=36) of 8 bytes, one per byte of the result, or None if any byte is not [0-9a-z].
/// Pure SWAR: digits 0x30..0x39 map to 1..10, letters 0x61..0x7a to 11..36; the result is verified by
/// reconstructing the original bytes and bounding each value.
#[inline(always)]
fn digits8(w: u64) -> Option<u64> {
    const LO: u64 = 0x0101_0101_0101_0101;
    let alpha = (w >> 6) & LO; // 1 for letters (bit 6 set), 0 for digits
    let t = (w & (LO * 0x1F)).wrapping_add(alpha * 25).wrapping_sub(LO * 0x0F);
    // reconstruct: digit -> t - 1 + 0x30 = t + 0x2F ; letter -> t - 11 + 0x61 = t + 0x56 = t + 0x2F + 0x27
    let back = t.wrapping_add(LO * 0x2F).wrapping_add(alpha * 0x27);
    // any borrow in t leaves a value byte >= 0xF1 (and could be cancelled by a carry in back), so reject
    // those first; afterwards every byte of t is < 0x80 and the bound test below cannot carry.
    if back != w || (t & (LO * 0x80)) != 0 {
        return None;
    }
    // bound: digit value <= 10, letter value <= 36
    let limit = (LO * 0x0A) + alpha * 0x1A;
    let over = (t.wrapping_add(LO * 0x7F).wrapping_sub(limit)) & (LO * 0x80);
    // lower bound: digit value >= 1, letter value >= 11, i.e. (t - 10*alpha) has no zero byte
    let tl = t.wrapping_sub(alpha * 10);
    let zero = tl.wrapping_sub(LO) & (LO * 0x80);
    if (over | zero) != 0 {
        return None;
    }
    Some(t)
}

/// Base-37 value of 8 chars (s[0] most significant). None if any char is invalid.
#[inline(always)]
fn pack8(s: &[u8]) -> Option<u64> {
    let w = u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]);
    let t = digits8(w)?;
    let e = t & 0x00FF_00FF_00FF_00FF;
    let o = (t >> 8) & 0x00FF_00FF_00FF_00FF;
    let pair = e * 37 + o;
    let pe = pair & 0x0000_FFFF_0000_FFFF;
    let po = (pair >> 16) & 0x0000_FFFF_0000_FFFF;
    let quad = pe * 1369 + po;
    Some((quad & 0xFFFF_FFFF) * 1_874_161 + (quad >> 32))
}

/// Table-driven reference of `pack8` (tests only).
#[cfg(test)]
fn pack8_ref(s: &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for &c in &s[..8] {
        let t = CHAR_VAL[c as usize];
        if t == 0 {
            return None;
        }
        v = v * 37 + t as u64;
    }
    Some(v)
}

const POW37_8: u64 = 3_512_479_453_921; // 37^8
const POW37: [u64; 13] = {
    let mut t = [1u64; 13];
    let mut i = 1;
    while i < 13 {
        t[i] = t[i - 1] * 37;
        i += 1;
    }
    t
};

impl PKey {
    #[inline(always)]
    pub fn new(s: &[u8]) -> PKey {
        let n = s.len();
        if n == 9 {
            let t0 = CHAR_VAL[s[0] as usize] as u64;
            if t0 != 0 {
                if let Some(v) = pack8(&s[1..9]) {
                    return PKey(t0 * POW37_8 + v);
                }
            }
        } else if n == 8 {
            if let Some(v) = pack8(s) {
                return PKey(v);
            }
        } else if n >= 1 && n <= 12 {
            let mut v: u64 = 0;
            let mut bad = 0u8;
            for &c in s {
                let t = CHAR_VAL[c as usize];
                bad |= (t == 0) as u8;
                v = v * 37 + t as u64;
            }
            if bad == 0 {
                return PKey(v);
            }
        }
        PKey((1u64 << 63) | fallback_intern(s) as u64)
    }
    #[inline(always)]
    pub fn is_packed(&self) -> bool {
        (self.0 >> 63) == 0
    }
    #[inline(always)]
    pub fn hash(&self) -> u64 {
        let h = self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^ (h >> 29)
    }
    /// The ID's bytes, written into `buf`.
    #[inline]
    pub fn bytes<'a>(&self, buf: &'a mut Vec<u8>) -> &'a [u8] {
        buf.clear();
        if self.is_packed() {
            let mut v = self.0;
            let mut tmp = [0u8; 12];
            let mut n = 0;
            while v != 0 {
                n += 1;
                tmp[12 - n] = DIGITS[(v % 37) as usize - 1];
                v /= 37;
            }
            buf.extend_from_slice(&tmp[12 - n..]);
        } else {
            buf.extend_from_slice(&fallback_bytes(self.0 as u32));
        }
        &buf[..]
    }
    /// For packed keys: a u64 whose numeric order equals the byte-lexicographic order of the ID.
    #[inline(always)]
    pub fn lex_key(&self) -> u64 {
        debug_assert!(self.is_packed());
        let v = self.0;
        let mut n = 0u32;
        let mut p: u64 = 1;
        for _ in 0..12 {
            n += (v >= p) as u32;
            p = p.saturating_mul(37);
        }
        v * POW37[(12 - n) as usize]
    }
    /// Inverse of `lex_key`.
    pub fn from_lex_key(mut l: u64) -> PKey {
        while l != 0 && l % 37 == 0 {
            l /= 37;
        }
        PKey(l)
    }
    #[inline]
    pub fn cmp_bytes(&self, other: &PKey) -> std::cmp::Ordering {
        if self.is_packed() && other.is_packed() {
            return self.lex_key().cmp(&other.lex_key());
        }
        let (mut a, mut b) = (Vec::new(), Vec::new());
        self.bytes(&mut a).cmp(other.bytes(&mut b)).then(std::cmp::Ordering::Equal)
    }
}

#[cfg(test)]
mod pkey_tests {
    use super::*;
    #[test]
    fn pack_roundtrip() {
        let ids: Vec<&[u8]> = vec![
            b"u1", b"u01", b"u10", b"e42huhoug", b"zzzzzzzzzzzz", b"0", b"000000000000", b"a", b"", b" u1", b"u1 ", b"U1",
            b"user_id_b", b"A\tB", b"uabcdefghijkl", b"uabcdefghijklm", b"this-is-a-long-user-id-0001", b"c_1",
        ];
        let keys: Vec<PKey> = ids.iter().map(|s| PKey::new(s)).collect();
        let mut buf = Vec::new();
        for (i, a) in keys.iter().enumerate() {
            assert_eq!(a.bytes(&mut buf), ids[i], "{:?}", std::str::from_utf8(ids[i]));
            for (j, b) in keys.iter().enumerate() {
                assert_eq!(a == b, ids[i] == ids[j], "{:?} vs {:?}", std::str::from_utf8(ids[i]), std::str::from_utf8(ids[j]));
                assert_eq!(a.cmp_bytes(b), ids[i].cmp(ids[j]));
            }
            if a.is_packed() {
                assert_eq!(PKey::from_lex_key(a.lex_key()), *a);
            }
        }
        // SWAR packer vs table reference: every byte value in every position, plus pairs of odd bytes
        let base = *b"a1b2c3d4";
        for i in 0..8 {
            for v in 0..=255u8 {
                let mut b = base;
                b[i] = v;
                assert_eq!(pack8(&b), pack8_ref(&b), "pos {} byte {:#x}", i, v);
            }
        }
        for i in 0..8 {
            for j in 0..8 {
                for &v in &[0u8, 0x2F, 0x3A, 0x40, 0x41, 0x5A, 0x60, 0x7B, 0x7F, 0x80, 0xFF, b'0', b'9', b'a', b'z'] {
                    for &u in &[0u8, 0x2F, 0x3A, 0x60, 0x7B, 0xFF, b'5', b'm'] {
                        let mut b = base;
                        b[i] = v;
                        b[j] = u;
                        assert_eq!(pack8(&b), pack8_ref(&b));
                    }
                }
            }
        }
        // same fallback string from "another thread" gets the same key
        let k1 = PKey::new(b" u1");
        let k2 = std::thread::spawn(|| PKey::new(b" u1")).join().unwrap();
        assert_eq!(k1, k2);
        fn generic(s: &[u8]) -> u64 {
            let mut v = 0u64;
            for &c in s {
                v = v * 37 + CHAR_VAL[c as usize] as u64;
            }
            v
        }
        let mut x: u64 = 0x1234_5678_9abc_def1;
        for _ in 0..100000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = 8 + (x & 1) as usize;
            let mut b = [0u8; 9];
            for i in 0..n {
                let r = (x >> (5 * i)) % 40;
                b[i] = if r < 36 { DIGITS[r as usize] } else { b"A_ -"[(r - 36) as usize] };
            }
            let sl = &b[..n];
            let valid = sl.iter().all(|&c| CHAR_VAL[c as usize] != 0);
            let k = PKey::new(sl);
            assert_eq!(k.is_packed(), valid);
            if valid {
                assert_eq!(k.0, generic(sl));
            }
        }
    }
}

/// A growable array backed by anonymous mmap with transparent huge pages requested (Linux).
/// Fewer first-touch page faults than malloc for multi-hundred-MB vectors, and cheap teardown.
pub struct HugeVec<T> {
    ptr: *mut T,
    len: usize,
    cap: usize,
    bytes: usize,
}
unsafe impl<T: Send> Send for HugeVec<T> {}
unsafe impl<T: Sync> Sync for HugeVec<T> {}

const HUGE: usize = 2 << 20;
/// Below this size an allocation uses normal pages and page-size rounding (P*P small vectors must not each take 2 MB).
const HUGE_MIN: usize = 2 << 20;

#[cfg(target_os = "linux")]
fn populate() -> libc::c_int {
    if std::env::var_os("POPULATE").is_some() { libc::MAP_POPULATE } else { 0 }
}
#[cfg(not(target_os = "linux"))]
fn populate() -> libc::c_int {
    0
}

/// Pool of released mappings: later phases reuse already-faulted memory instead of unmapping and re-faulting.
static POOL: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

fn pool_take(bytes: usize) -> Option<*mut u8> {
    if std::env::var_os("NO_POOL").is_some() {
        return None;
    }
    let mut pool = POOL.lock().unwrap();
    // best fit: smallest mapping that is large enough
    let mut best: Option<usize> = None;
    for (i, &(_, b)) in pool.iter().enumerate() {
        if b >= bytes && best.map_or(true, |j| pool[j].1 > b) {
            best = Some(i);
        }
    }
    best.map(|i| {
        let (p, _) = pool.swap_remove(i);
        p as *mut u8
    })
}

fn pool_put(p: *mut u8, bytes: usize) {
    POOL.lock().unwrap().push((p as usize, bytes));
}

/// Unmap everything in the pool using `nthreads` threads. Called right before exit, when most threads are
/// idle, so the page-table teardown is parallel instead of serialized in the kernel's exit path.
pub fn pool_drain_parallel(nthreads: usize) {
    // measured: the parallel unmap costs 0.03-0.05 s and exit teardown was not on the critical path; opt-in only
    if std::env::var_os("DRAIN").is_none() {
        return;
    }
    let items: Vec<(usize, usize)> = std::mem::take(&mut *POOL.lock().unwrap());
    if items.is_empty() {
        return;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..nthreads.max(1) {
            let (next, items) = (&next, &items);
            s.spawn(move || loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= items.len() {
                    break;
                }
                unsafe { libc::munmap(items[i].0 as *mut libc::c_void, items[i].1) };
            });
        }
    });
}

fn map_bytes(bytes: usize) -> *mut u8 {
    if let Some(p) = pool_take(bytes) {
        return p;
    }
    unsafe {
        // Large mappings are 2 MB-aligned so transparent huge pages can cover the whole range.
        let big = bytes >= HUGE_MIN;
        let ask = if big { bytes + HUGE } else { bytes };
        let raw = libc::mmap(
            std::ptr::null_mut(),
            ask,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | populate(),
            -1,
            0,
        );
        assert!(raw != libc::MAP_FAILED, "mmap failed for {} bytes", bytes);
        let mut p = raw;
        if big {
            let addr = raw as usize;
            let aligned = (addr + HUGE - 1) & !(HUGE - 1);
            if aligned > addr {
                libc::munmap(raw, aligned - addr);
            }
            let tail = addr + ask - (aligned + bytes);
            if tail > 0 {
                libc::munmap((aligned + bytes) as *mut libc::c_void, tail);
            }
            p = aligned as *mut libc::c_void;
        }
        #[cfg(target_os = "linux")]
        if big {
            libc::madvise(p, bytes, libc::MADV_HUGEPAGE);
            // MADV_POPULATE_WRITE (Linux 5.14+): fault the whole range in one call, as huge pages where
            // possible, instead of one trap per 4 KB page. Ignored on older kernels. Opt out with NO_POPULATE.
            if std::env::var_os("NO_POPULATE").is_none() {
                libc::madvise(p, bytes, 23);
            }
        }
        p as *mut u8
    }
}

impl<T: Copy> HugeVec<T> {
    pub fn with_capacity(cap: usize) -> Self {
        let size = std::mem::size_of::<T>().max(1);
        let raw = cap.max(64) * size;
        let unit = if raw >= HUGE_MIN { HUGE } else { 4096 };
        let bytes = (raw + unit - 1) / unit * unit;
        let ptr = map_bytes(bytes) as *mut T;
        HugeVec { ptr, len: 0, cap: bytes / size, bytes }
    }
    /// Allocates and marks `n` elements as present without initializing them; the caller writes every one.
    pub fn with_len_uninit(n: usize) -> Self {
        let mut v = Self::with_capacity(n);
        v.len = n;
        v
    }
    #[inline(always)]
    pub fn push(&mut self, v: T) {
        if self.len == self.cap {
            self.grow();
        }
        unsafe { self.ptr.add(self.len).write(v) };
        self.len += 1;
    }
    #[cold]
    fn grow(&mut self) {
        let size = std::mem::size_of::<T>().max(1);
        let raw = self.cap * 2 * size;
        let unit = if raw >= HUGE_MIN { HUGE } else { 4096 };
        let new_bytes = (raw + unit - 1) / unit * unit;
        let np = map_bytes(new_bytes) as *mut T;
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr, np, self.len);
        }
        pool_put(self.ptr as *mut u8, self.bytes);
        self.ptr = np;
        self.cap = new_bytes / size;
        self.bytes = new_bytes;
    }
    #[inline(always)]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.ptr
    }
    #[inline(always)]
    pub fn extend_from_slice(&mut self, src: &[T]) {
        while self.len + src.len() > self.cap {
            self.grow();
        }
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(self.len), src.len()) };
        self.len += src.len();
    }
}

/// One 64-byte staging line.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub struct Line(pub [u8; 64]);

/// Partitioned output of one producer: a fixed-capacity region per partition inside one mapping, plus a
/// rarely used overflow vector per partition. `get(p)` returns the two slices of partition p.
pub struct Parts<T: Copy> {
    arena: HugeVec<T>,
    cap: usize,
    lens: Vec<u32>,
    over: Vec<Vec<T>>,
}
unsafe impl<T: Copy + Send> Send for Parts<T> {}
unsafe impl<T: Copy + Sync> Sync for Parts<T> {}
impl<T: Copy> Parts<T> {
    #[inline(always)]
    pub fn get(&self, p: usize) -> [&[T]; 2] {
        let a = unsafe { std::slice::from_raw_parts(self.arena.ptr.add(p * self.cap), self.lens[p] as usize) };
        [a, &self.over[p][..]]
    }
    #[inline(always)]
    pub fn len(&self, p: usize) -> usize {
        self.lens[p] as usize + self.over[p].len()
    }
    pub fn nparts(&self) -> usize {
        self.lens.len()
    }
}

/// Software write-combining scatter into `np` partitions: records are staged four per partition (one or
/// two 64-byte lines) and streamed out with non-temporal stores, so the parser's output never pollutes the
/// caches. Four records per flush keeps the flush branch mostly not taken.
pub struct Scatter<T: Copy> {
    arena: HugeVec<T>,
    cap: usize,
    lens: Vec<u32>,
    over: Vec<Vec<T>>,
    stage: Vec<Line>,
    cnt: Vec<u8>,
    per: usize,
    lines: usize, // staging lines per partition
}

impl<T: Copy + Default> Scatter<T> {
    /// `est` = expected records per partition; the region holds 5/4 of that (rounded up to whole flushes).
    pub fn new(np: usize, est: usize) -> Self {
        let size = std::mem::size_of::<T>();
        assert!(size > 0 && 64 % size == 0 && size <= 64, "record size must divide 64");
        let per = 4.max(64 / size);
        let lines = (per * size + 63) / 64;
        let cap = ((est + est / 4 + 64) + per - 1) / per * per;
        Scatter {
            arena: HugeVec::with_capacity(np * cap),
            cap,
            lens: vec![0; np],
            over: (0..np).map(|_| Vec::new()).collect(),
            stage: vec![Line([0u8; 64]); np * lines],
            cnt: vec![0; np],
            per,
            lines,
        }
    }
    #[inline(always)]
    fn stage_ptr(&mut self, p: usize) -> *mut u8 {
        unsafe { self.stage.as_mut_ptr().add(p * self.lines) as *mut u8 }
    }
    #[inline(always)]
    pub fn push(&mut self, p: usize, v: T) {
        let c = self.cnt[p] as usize;
        unsafe {
            let dst = self.stage_ptr(p).add(c * std::mem::size_of::<T>()) as *mut T;
            dst.write(v);
        }
        if c + 1 == self.per {
            self.flush(p);
            self.cnt[p] = 0;
        } else {
            self.cnt[p] = (c + 1) as u8;
        }
    }
    #[inline(always)]
    fn flush(&mut self, p: usize) {
        let l = self.lens[p] as usize;
        if l + self.per <= self.cap {
            unsafe {
                let dst = (self.arena.ptr.add(p * self.cap + l)) as *mut u8;
                let src = self.stage_ptr(p) as *const u8;
                for i in 0..self.lines {
                    stream64(dst.add(i * 64), src.add(i * 64));
                }
            }
            self.lens[p] = (l + self.per) as u32;
        } else {
            let src = unsafe { std::slice::from_raw_parts(self.stage_ptr(p) as *const T, self.per) };
            self.over[p].extend_from_slice(src);
        }
    }
    pub fn finish(mut self) -> Parts<T> {
        for p in 0..self.lens.len() {
            let c = self.cnt[p] as usize;
            if c > 0 {
                let src = unsafe { std::slice::from_raw_parts(self.stage_ptr(p) as *const T, c) };
                let l = self.lens[p] as usize;
                if l + c <= self.cap {
                    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.arena.ptr.add(p * self.cap + l), c) };
                    self.lens[p] = (l + c) as u32;
                } else {
                    self.over[p].extend_from_slice(src);
                }
            }
        }
        sfence();
        Parts { arena: self.arena, cap: self.cap, lens: self.lens, over: self.over }
    }
}

/// Copies one 64-byte line with non-temporal stores (dst must be 64-byte aligned).
#[inline(always)]
unsafe fn stream64(dst: *mut u8, src: *const u8) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    {
        use std::arch::x86_64::*;
        let a = _mm256_load_si256(src as *const __m256i);
        let b = _mm256_load_si256(src.add(32) as *const __m256i);
        _mm256_stream_si256(dst as *mut __m256i, a);
        _mm256_stream_si256(dst.add(32) as *mut __m256i, b);
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    {
        std::ptr::copy_nonoverlapping(src, dst, 64);
    }
}

#[inline(always)]
fn sfence() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_sfence();
    }
}

/// Pin the calling thread to the t-th CPU of its allowed set (Linux). Best effort; PIN=0 disables.
pub fn pin_thread(t: usize) {
    #[cfg(target_os = "linux")]
    unsafe {
        if std::env::var_os("NO_PIN").is_some() {
            return;
        }
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return;
        }
        let mut allowed = Vec::new();
        for c in 0..libc::CPU_SETSIZE as usize {
            if libc::CPU_ISSET(c, &set) {
                allowed.push(c);
            }
        }
        if allowed.len() < 2 {
            return;
        }
        let cpu = allowed[t % allowed.len()];
        let mut one: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut one);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &one);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = t;
    }
}

#[inline(always)]
pub fn prefetch<T>(p: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, readonly, preserves_flags));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = p;
    }
}

/// Lookup of small reference tables (products, campaigns): IDs of the shape `<prefix><decimal>` with no
/// leading zeros index an array directly; everything else goes through the hash table.
pub struct Lookup {
    prefix: Vec<u8>,
    arr: Vec<u32>,
    table: SmallTable,
}

impl Lookup {
    pub fn new(prefix: &[u8], cap: usize) -> Self {
        Lookup { prefix: prefix.to_vec(), arr: Vec::new(), table: SmallTable::with_capacity(cap) }
    }
    /// Canonical decimal number after the prefix (no leading zeros, at most 7 digits), or None.
    #[inline(always)]
    fn number(&self, id: &[u8]) -> Option<usize> {
        let pl = self.prefix.len();
        if id.len() <= pl || id.len() > pl + 7 || &id[..pl] != &self.prefix[..] {
            return None;
        }
        let d = &id[pl..];
        if d[0] == b'0' {
            return None;
        }
        let mut v = 0usize;
        for &c in d {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as usize;
        }
        Some(v)
    }
    pub fn insert(&mut self, id: &[u8], val: u32) {
        match self.number(id) {
            Some(n) if n < (1 << 23) => {
                if n >= self.arr.len() {
                    self.arr.resize(n + 1, u32::MAX);
                }
                self.arr[n] = val;
            }
            _ => self.table.insert(id, val),
        }
    }
    #[inline(always)]
    pub fn get(&self, id: &[u8]) -> Option<u32> {
        match self.number(id) {
            Some(n) => {
                if n < self.arr.len() {
                    let v = self.arr[n];
                    if v != u32::MAX {
                        return Some(v);
                    }
                }
                self.table.get(id)
            }
            None => self.table.get(id),
        }
    }
}

// ---------- global record pool with chunked per-partition scatter ----------

/// Records per pool chunk. A chunk is the unit a producer takes from the pool for one partition, so a
/// partition's records are runs of up to CHUNK contiguous records; the unused tail of a run is wasted.
pub const CHUNK: usize = 512;
/// Chunks a producer takes from the pool at once (one atomic add, one populate call).
const RUN: usize = 32;

fn map_lazy(bytes: usize) -> *mut u8 {
    unsafe {
        let ask = bytes + HUGE;
        let raw = libc::mmap(
            std::ptr::null_mut(),
            ask,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        );
        assert!(raw != libc::MAP_FAILED, "mmap failed for {} bytes", bytes);
        let addr = raw as usize;
        let aligned = (addr + HUGE - 1) & !(HUGE - 1);
        if aligned > addr {
            libc::munmap(raw, aligned - addr);
        }
        let tail = addr + ask - (aligned + bytes);
        if tail > 0 {
            libc::munmap((aligned + bytes) as *mut libc::c_void, tail);
        }
        #[cfg(target_os = "linux")]
        libc::madvise(aligned as *mut libc::c_void, bytes, libc::MADV_HUGEPAGE);
        aligned as *mut u8
    }
}

/// One reservation of records of T, handed out in runs of chunks by an atomic bump. A record's global
/// index (u32) is stable for the pool's life, so a later phase can address one record directly.
/// The reservation is virtual (2x the estimate); pages are populated run by run as they are taken.
pub struct Pool<T: Copy> {
    base: *mut T,
    cap: usize, // records
    next: std::sync::atomic::AtomicUsize,
    bytes: usize,
}
unsafe impl<T: Copy + Send> Send for Pool<T> {}
unsafe impl<T: Copy + Sync> Sync for Pool<T> {}

impl<T: Copy> Pool<T> {
    pub fn new(est: usize) -> Self {
        let size = std::mem::size_of::<T>();
        let chunks = (est * 2 + (1 << 20)) / CHUNK + RUN;
        let cap = chunks * CHUNK;
        assert!(cap < u32::MAX as usize, "pool too large for u32 indices");
        let bytes = (cap * size + HUGE - 1) / HUGE * HUGE;
        Pool { base: map_lazy(bytes) as *mut T, cap, next: std::sync::atomic::AtomicUsize::new(0), bytes }
    }
    #[inline(always)]
    pub fn at(&self, i: u32) -> *mut T {
        unsafe { self.base.add(i as usize) }
    }
    /// Takes RUN chunks; returns the first record index.
    fn grab(&self) -> u32 {
        let c = self.next.fetch_add(RUN, std::sync::atomic::Ordering::Relaxed);
        assert!((c + RUN) * CHUNK <= self.cap, "record pool exhausted: estimate too low");
        let first = c * CHUNK;
        #[cfg(target_os = "linux")]
        unsafe {
            // MADV_POPULATE_WRITE: fault the run in with one call (ignored on kernels before 5.14)
            let p = self.base.add(first) as *mut libc::c_void;
            libc::madvise(p, RUN * CHUNK * std::mem::size_of::<T>(), 23);
        }
        first as u32
    }
    pub fn used_records(&self) -> usize {
        self.next.load(std::sync::atomic::Ordering::Relaxed) * CHUNK
    }
}
impl<T: Copy> Drop for Pool<T> {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.bytes) };
    }
}

/// Runs of records of one partition: (first record index, length), in production order.
pub type Runs = Vec<(u32, u32)>;

/// One producer's partitioned output into a shared Pool: records are staged four per partition in
/// 64-byte lines and streamed to the partition's current chunk with non-temporal stores. `push` returns
/// the record's global index.
pub struct ChunkScatter<'a, T: Copy> {
    pool: &'a Pool<T>,
    cur: Vec<u32>,
    end: Vec<u32>,
    lists: Vec<Runs>,
    stage: Vec<Line>,
    cnt: Vec<u8>,
    per: usize,
    lines: usize,
    run: (u32, u32),
}

impl<'a, T: Copy> ChunkScatter<'a, T> {
    pub fn new(pool: &'a Pool<T>, np: usize) -> Self {
        let size = std::mem::size_of::<T>();
        assert!(size > 0 && 64 % size == 0 && size <= 64, "record size must divide 64");
        let per = 4.max(64 / size);
        let lines = (per * size + 63) / 64;
        assert!(CHUNK % per == 0);
        ChunkScatter {
            pool,
            cur: vec![0; np],
            end: vec![0; np],
            lists: (0..np).map(|_| Vec::new()).collect(),
            stage: vec![Line([0u8; 64]); np * lines],
            cnt: vec![0; np],
            per,
            lines,
            run: (0, 0),
        }
    }
    #[inline(always)]
    fn stage_ptr(&mut self, p: usize) -> *mut u8 {
        unsafe { self.stage.as_mut_ptr().add(p * self.lines) as *mut u8 }
    }
    #[cold]
    fn new_chunk(&mut self, p: usize) {
        if self.run.0 == self.run.1 {
            let first = self.pool.grab();
            self.run = (first, first + (RUN * CHUNK) as u32);
        }
        self.cur[p] = self.run.0;
        self.end[p] = self.run.0 + CHUNK as u32;
        self.run.0 += CHUNK as u32;
        self.lists[p].push((self.cur[p], 0));
    }
    #[inline(always)]
    pub fn push(&mut self, p: usize, v: T) -> u32 {
        let c = self.cnt[p] as usize;
        if c == 0 && self.cur[p] == self.end[p] {
            self.new_chunk(p);
        }
        unsafe {
            let dst = self.stage_ptr(p).add(c * std::mem::size_of::<T>()) as *mut T;
            dst.write(v);
        }
        let idx = self.cur[p] + c as u32;
        if c + 1 == self.per {
            self.flush(p);
            self.cnt[p] = 0;
        } else {
            self.cnt[p] = (c + 1) as u8;
        }
        idx
    }
    #[inline(always)]
    fn flush(&mut self, p: usize) {
        unsafe {
            let dst = self.pool.at(self.cur[p]) as *mut u8;
            let src = self.stage_ptr(p) as *const u8;
            for i in 0..self.lines {
                stream64(dst.add(i * 64), src.add(i * 64));
            }
        }
        self.cur[p] += self.per as u32;
        self.lists[p].last_mut().unwrap().1 += self.per as u32;
    }
    pub fn finish(mut self) -> Vec<Runs> {
        for p in 0..self.cnt.len() {
            let c = self.cnt[p] as usize;
            if c > 0 {
                unsafe {
                    let src = self.stage_ptr(p) as *const T;
                    std::ptr::copy_nonoverlapping(src, self.pool.at(self.cur[p]), c);
                }
                self.lists[p].last_mut().unwrap().1 += c as u32;
                self.cur[p] += c as u32;
                self.cnt[p] = 0;
            }
        }
        sfence();
        self.lists
    }
}

/// Read side of a pool: the runs of each producer for partition p, as slices.
pub struct PoolParts<'a, T: Copy> {
    pub pool: &'a Pool<T>,
    pub lists: Vec<Vec<Runs>>, // [producer][partition]
}
impl<'a, T: Copy> PoolParts<'a, T> {
    #[inline(always)]
    pub fn slices(&self, p: usize) -> impl Iterator<Item = &[T]> + '_ {
        let pool = self.pool;
        self.lists.iter().flat_map(move |l| l[p].iter().map(move |&(s, n)| unsafe { std::slice::from_raw_parts(pool.at(s) as *const T, n as usize) }))
    }
    /// Slices with their first record's global index.
    #[inline(always)]
    pub fn runs(&self, p: usize) -> impl Iterator<Item = (u32, &[T])> + '_ {
        let pool = self.pool;
        self.lists.iter().flat_map(move |l| l[p].iter().map(move |&(s, n)| (s, unsafe { std::slice::from_raw_parts(pool.at(s) as *const T, n as usize) })))
    }
    pub fn len(&self, p: usize) -> usize {
        self.lists.iter().map(|l| l[p].iter().map(|r| r.1 as usize).sum::<usize>()).sum()
    }
    pub fn total(&self) -> usize {
        self.lists.iter().map(|l| l.iter().map(|runs| runs.iter().map(|r| r.1 as usize).sum::<usize>()).sum::<usize>()).sum()
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    #[derive(Clone, Copy, Default, PartialEq, Debug)]
    #[repr(C)]
    struct R {
        a: u64,
        b: u32,
        c: u32,
    }

    #[test]
    fn chunk_scatter_keeps_order_and_indices() {
        let pool: Pool<R> = Pool::new(100_000);
        let np = 8;
        let mut idx: Vec<Vec<(u32, R)>> = vec![Vec::new(); np];
        let lists: Vec<Runs> = {
            let mut sc = ChunkScatter::new(&pool, np);
            let mut x = 12345u64;
            for i in 0..70_000u64 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let p = ((x >> 40) as usize) % np;
                let r = R { a: i, b: (x >> 8) as u32, c: p as u32 };
                let at = sc.push(p, r);
                idx[p].push((at, r));
            }
            sc.finish()
        };
        let parts = PoolParts { pool: &pool, lists: vec![lists] };
        for p in 0..np {
            let got: Vec<R> = parts.slices(p).flat_map(|s| s.iter().copied()).collect();
            let want: Vec<R> = idx[p].iter().map(|&(_, r)| r).collect();
            assert_eq!(got, want, "partition {p} order");
            for &(at, r) in &idx[p] {
                assert_eq!(unsafe { *pool.at(at) }, r);
            }
            assert_eq!(parts.len(p), want.len());
        }
    }
}
