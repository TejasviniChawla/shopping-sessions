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

/// Base-37 value of 8 chars (s[0] most significant) with three independent SWAR steps. None if any char is invalid.
#[inline(always)]
fn pack8(s: &[u8]) -> Option<u64> {
    let t = (CHAR_VAL[s[0] as usize] as u64)
        | (CHAR_VAL[s[1] as usize] as u64) << 8
        | (CHAR_VAL[s[2] as usize] as u64) << 16
        | (CHAR_VAL[s[3] as usize] as u64) << 24
        | (CHAR_VAL[s[4] as usize] as u64) << 32
        | (CHAR_VAL[s[5] as usize] as u64) << 40
        | (CHAR_VAL[s[6] as usize] as u64) << 48
        | (CHAR_VAL[s[7] as usize] as u64) << 56;
    if (t.wrapping_sub(0x0101_0101_0101_0101) & !t & 0x8080_8080_8080_8080) != 0 {
        return None;
    }
    let e = t & 0x00FF_00FF_00FF_00FF;
    let o = (t >> 8) & 0x00FF_00FF_00FF_00FF;
    let pair = e * 37 + o;
    let pe = pair & 0x0000_FFFF_0000_FFFF;
    let po = (pair >> 16) & 0x0000_FFFF_0000_FFFF;
    let quad = pe * 1369 + po;
    Some((quad & 0xFFFF_FFFF) * 1_874_161 + (quad >> 32))
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
        let mut h = self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 32;
        h = h.wrapping_mul(0xD6E8_FEB8_6659_FD93);
        h ^= h >> 29;
        h
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

/// Software write-combining scatter into P partition vectors: records are staged in small L1-resident
/// buffers and copied out in 512-byte bursts, so the memory system sees few sequential streams.
pub struct Scatter<T: Copy, const P: usize> {
    pub out: Vec<HugeVec<T>>,
    stage: Vec<[T; 16]>,
    cnt: [u8; P],
}

impl<T: Copy + Default, const P: usize> Scatter<T, P> {
    pub fn new(est: usize) -> Self {
        Scatter {
            out: (0..P).map(|_| HugeVec::with_capacity(est)).collect(),
            stage: vec![[T::default(); 16]; P],
            cnt: [0; P],
        }
    }
    #[inline(always)]
    pub fn push(&mut self, p: usize, v: T) {
        let c = self.cnt[p] as usize;
        self.stage[p][c] = v;
        if c + 1 == 16 {
            self.out[p].extend_from_slice(&self.stage[p]);
            self.cnt[p] = 0;
        } else {
            self.cnt[p] = (c + 1) as u8;
        }
    }
    pub fn finish(mut self) -> Vec<HugeVec<T>> {
        for p in 0..P {
            let c = self.cnt[p] as usize;
            if c > 0 {
                let st = self.stage[p];
                self.out[p].extend_from_slice(&st[..c]);
            }
        }
        self.out
    }
}

impl<T> std::ops::Deref for HugeVec<T> {
    type Target = [T];
    #[inline(always)]
    fn deref(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl<T> std::ops::DerefMut for HugeVec<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}
impl<T> Drop for HugeVec<T> {
    fn drop(&mut self) {
        pool_put(self.ptr as *mut u8, self.bytes);
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
