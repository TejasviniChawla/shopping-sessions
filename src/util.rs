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

/// Hash of a key: inline keys mix the u128 directly; arena keys hash their bytes.
#[inline(always)]
pub fn key_hash(k: &Key, arenas: &Arenas) -> u64 {
    if k.is_inline() {
        const K1: u64 = 0x9E37_79B9_7F4A_7C15;
        const K2: u64 = 0xD6E8_FEB8_6659_FD93;
        let lo = k.0 as u64;
        let hi = (k.0 >> 64) as u64;
        let mut h = (lo ^ K1).wrapping_mul(K2);
        h ^= h >> 32;
        h = (h ^ hi).wrapping_mul(K2);
        h ^= h >> 29;
        h = h.wrapping_mul(K1);
        h ^= h >> 32;
        h
    } else {
        hash_bytes(k.bytes(arenas))
    }
}

/// Per-thread byte arenas hold the rare IDs longer than 15 bytes.
pub type Arenas = [Vec<u8>];

/// An ID key: up to 15 bytes stored inline (top byte = length), longer ones copied into an arena (top byte 0xFF).
/// Inline keys compare with one u128 equality; nothing after parsing needs the input files again.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Key(u128);

impl Key {
    #[inline(always)]
    pub fn new(s: &[u8], arena_id: usize, arena: &mut Vec<u8>) -> Key {
        let n = s.len();
        if n <= 15 {
            let mut buf = [0u8; 16];
            buf[..n].copy_from_slice(s);
            buf[15] = n as u8;
            Key(u128::from_le_bytes(buf))
        } else {
            let off = arena.len();
            arena.extend_from_slice(s);
            let packed = (off as u128) | ((arena_id as u128) << 32) | ((n as u128) << 48);
            Key(packed | (0xFFu128 << 120))
        }
    }
    #[inline(always)]
    pub fn is_inline(&self) -> bool {
        (self.0 >> 120) as u8 != 0xFF
    }
    #[inline(always)]
    pub fn bytes<'a>(&'a self, arenas: &'a Arenas) -> &'a [u8] {
        let tag = (self.0 >> 120) as u8;
        if tag != 0xFF {
            // little-endian: byte 0 of the u128 is at the lowest address
            unsafe { std::slice::from_raw_parts(self as *const Key as *const u8, tag as usize) }
        } else {
            let off = self.0 as u32 as usize;
            let arena = (self.0 >> 32) as u16 as usize;
            let len = (self.0 >> 48) as u32 as usize & 0xFFFF_FFFF;
            let len = len & 0xFFFF_FFFF_FF;
            &arenas[arena][off..off + len]
        }
    }
    #[inline(always)]
    pub fn same(&self, other: &Key, arenas: &Arenas) -> bool {
        if self.is_inline() && other.is_inline() {
            self.0 == other.0
        } else {
            self.bytes(arenas) == other.bytes(arenas)
        }
    }
    #[inline(always)]
    pub fn cmp_bytes(&self, other: &Key, arenas: &Arenas) -> std::cmp::Ordering {
        self.bytes(arenas).cmp(other.bytes(arenas))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_inline_and_arena() {
        let mut arena = Vec::new();
        let a = Key::new(b"u1", 0, &mut arena);
        let b = Key::new(b"u1", 0, &mut arena);
        let c = Key::new(b"U1", 0, &mut arena);
        let d = Key::new(b"u1 ", 0, &mut arena);
        let long1 = Key::new(b"this-is-a-long-user-id-0001", 0, &mut arena);
        let long2 = Key::new(b"this-is-a-long-user-id-0001", 0, &mut arena);
        let long3 = Key::new(b"this-is-a-long-user-id-0002", 0, &mut arena);
        let arenas = vec![arena];
        assert!(a.same(&b, &arenas));
        assert!(!a.same(&c, &arenas));
        assert!(!a.same(&d, &arenas));
        assert!(long1.same(&long2, &arenas));
        assert!(!long1.same(&long3, &arenas));
        assert!(!long1.same(&a, &arenas));
        assert_eq!(long1.bytes(&arenas), b"this-is-a-long-user-id-0001");
        assert_eq!(a.bytes(&arenas), b"u1");
        assert_eq!(Key::new(b"", 0, &mut Vec::new()).bytes(&arenas), b"");
        assert_eq!(Key::new(b"123456789012345", 0, &mut Vec::new()).bytes(&arenas), b"123456789012345");
        assert_eq!(c.cmp_bytes(&a, &arenas), std::cmp::Ordering::Less);
        assert_eq!(long1.cmp_bytes(&long3, &arenas), std::cmp::Ordering::Less);
    }
}

/// A growable array backed by anonymous mmap with transparent huge pages requested (Linux).
/// Fewer first-touch page faults than malloc for multi-hundred-MB vectors, and cheap teardown.
pub struct HugeVec<T> {
    ptr: *mut T,
    len: usize,
    cap: usize,
}
unsafe impl<T: Send> Send for HugeVec<T> {}
unsafe impl<T: Sync> Sync for HugeVec<T> {}

const HUGE: usize = 2 << 20;
/// Below this size an allocation uses normal pages and page-size rounding (P*P small vectors must not each take 2 MB).
const HUGE_MIN: usize = 2 << 20;

#[cfg(target_os = "linux")]
fn populate() -> libc::c_int {
    if std::env::var_os("NO_POPULATE").is_some() { 0 } else { libc::MAP_POPULATE }
}
#[cfg(not(target_os = "linux"))]
fn populate() -> libc::c_int {
    0
}

fn map_bytes(bytes: usize) -> *mut u8 {
    unsafe {
        // MAP_POPULATE: the kernel maps and zeroes all pages in one go, instead of one trap per page.
        let p = libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | populate(),
            -1,
            0,
        );
        assert!(p != libc::MAP_FAILED, "mmap failed for {} bytes", bytes);
        #[cfg(target_os = "linux")]
        if bytes >= HUGE_MIN {
            libc::madvise(p, bytes, libc::MADV_HUGEPAGE);
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
        HugeVec { ptr, len: 0, cap: bytes / size }
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
            libc::munmap(self.ptr as *mut libc::c_void, self.cap * size);
        }
        self.ptr = np;
        self.cap = new_bytes / size;
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
        let size = std::mem::size_of::<T>().max(1);
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.cap * size);
        }
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

/// A packed ID: lowercase base-36 strings of 1..=12 chars are encoded as a base-37 number (digits 1..=36),
/// which is injective and fits in 63 bits. Anything else is stored as a `Key` in a per-thread side table and
/// referenced as (1<<63 | aid<<32 | index). Two packed keys are equal iff their u64s are equal.
#[derive(Clone, Copy, PartialEq, Eq, Default, Hash)]
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

/// Per-thread storage for IDs that do not pack: side table of Keys plus the arena for Keys over 15 bytes.
pub struct Side {
    pub keys: Vec<Key>,
    pub arena: Vec<u8>,
    pub dates: crate::ts::DateCache,
}
impl Side {
    pub fn new() -> Self {
        Side { keys: Vec::new(), arena: Vec::new(), dates: [Default::default(); 32] }
    }
}
pub type Sides = [Side];

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
    // any zero byte => invalid char
    if (t.wrapping_sub(0x0101_0101_0101_0101) & !t & 0x8080_8080_8080_8080) != 0 {
        return None;
    }
    let e = t & 0x00FF_00FF_00FF_00FF; // t0 t2 t4 t6 (more significant of each pair)
    let o = (t >> 8) & 0x00FF_00FF_00FF_00FF; // t1 t3 t5 t7
    let pair = e * 37 + o; // 16-bit lanes, max 1368
    let pe = pair & 0x0000_FFFF_0000_FFFF;
    let po = (pair >> 16) & 0x0000_FFFF_0000_FFFF;
    let quad = pe * 1369 + po; // 32-bit lanes, max 1_874_160
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
    pub fn new(s: &[u8], aid: usize, side: &mut Side) -> PKey {
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
        let idx = side.keys.len();
        side.keys.push(Key::new(s, aid, &mut side.arena));
        PKey((1u64 << 63) | ((aid as u64) << 32) | idx as u64)
    }
    #[inline(always)]
    pub fn is_packed(&self) -> bool {
        (self.0 >> 63) == 0
    }
    #[inline(always)]
    fn mix(v: u64) -> u64 {
        let mut h = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 32;
        h = h.wrapping_mul(0xD6E8_FEB8_6659_FD93);
        h ^= h >> 29;
        h
    }
    /// Hash: packed keys mix the u64; fallback keys hash their bytes so equal strings from different
    /// threads hash alike. `sides` must hold every thread's side table.
    #[inline(always)]
    pub fn hash(&self, sides: &Sides) -> u64 {
        if self.is_packed() {
            Self::mix(self.0)
        } else {
            let aid = ((self.0 >> 32) & 0x7FFF_FFFF) as usize;
            let k = &sides[aid].keys[(self.0 & 0xFFFF_FFFF) as usize];
            hash_bytes(k.bytes_from(&sides[aid].arena))
        }
    }
    /// Hash while parsing, when only this thread's own side table exists.
    #[inline(always)]
    pub fn hash_in(&self, side: &Side) -> u64 {
        if self.is_packed() {
            Self::mix(self.0)
        } else {
            let k = &side.keys[(self.0 & 0xFFFF_FFFF) as usize];
            hash_bytes(k.bytes_from(&side.arena))
        }
    }
    #[inline(always)]
    pub fn same(&self, other: &PKey, sides: &Sides) -> bool {
        if self.0 == other.0 {
            return true;
        }
        if self.is_packed() || other.is_packed() {
            return false;
        }
        let a = self.side_key(sides);
        let b = other.side_key(sides);
        let arenas_a = &sides[((self.0 >> 32) & 0x7FFF_FFFF) as usize].arena;
        let arenas_b = &sides[((other.0 >> 32) & 0x7FFF_FFFF) as usize].arena;
        a.bytes(std::slice::from_ref(arenas_a)) == b.bytes(std::slice::from_ref(arenas_b))
    }
    #[inline(always)]
    fn side_key<'a>(&self, sides: &'a Sides) -> &'a Key {
        let aid = ((self.0 >> 32) & 0x7FFF_FFFF) as usize;
        &sides[aid].keys[(self.0 & 0xFFFF_FFFF) as usize]
    }
    /// The ID's bytes. `buf` receives the decoded packed form.
    #[inline]
    pub fn bytes<'a>(&self, sides: &'a Sides, buf: &'a mut [u8; 12]) -> &'a [u8] {
        if self.is_packed() {
            let mut v = self.0;
            let mut n = 0;
            while v != 0 {
                n += 1;
                buf[12 - n] = DIGITS[(v % 37) as usize - 1];
                v /= 37;
            }
            &buf[12 - n..]
        } else {
            let aid = ((self.0 >> 32) & 0x7FFF_FFFF) as usize;
            let k = &sides[aid].keys[(self.0 & 0xFFFF_FFFF) as usize];
            // Key::bytes needs the arena slice indexed by arena id == aid; sides[aid].arena is that arena
            k.bytes_from(&sides[aid].arena)
        }
    }
    /// For packed keys: a u64 whose numeric order equals the byte-lexicographic order of the ID.
    /// Left-align the base-37 digits to 12 places (pad with digit 0, which sorts before every real digit).
    #[inline(always)]
    pub fn lex_key(&self) -> u64 {
        debug_assert!(self.is_packed());
        let v = self.0;
        // number of digits = count of powers <= v
        let mut n = 0u32;
        let mut p: u64 = 1;
        for _ in 0..12 {
            n += (v >= p) as u32;
            p = p.saturating_mul(37);
        }
        v * POW37[(12 - n) as usize]
    }
    #[inline]
    pub fn cmp_bytes(&self, other: &PKey, sides: &Sides) -> std::cmp::Ordering {
        if self.is_packed() && other.is_packed() {
            return self.lex_key().cmp(&other.lex_key());
        }
        let (mut ba, mut bb) = ([0u8; 12], [0u8; 12]);
        self.bytes(sides, &mut ba).cmp(other.bytes(sides, &mut bb))
    }
}

impl Key {
    /// Bytes when the arena for this key is known directly.
    #[inline(always)]
    pub fn bytes_from<'a>(&'a self, arena: &'a Vec<u8>) -> &'a [u8] {
        let tag = (self.0 >> 120) as u8;
        if tag != 0xFF {
            unsafe { std::slice::from_raw_parts(self as *const Key as *const u8, tag as usize) }
        } else {
            let off = self.0 as u32 as usize;
            let len = ((self.0 >> 48) & 0xFFFF_FFFF_FFFF) as usize;
            &arena[off..off + len]
        }
    }
}

#[cfg(test)]
mod pkey_tests {
    use super::*;
    #[test]
    fn pack_roundtrip() {
        let mut side = Side::new();
        let ids: Vec<&[u8]> = vec![
            b"u1", b"u01", b"u10", b"e42huhoug", b"zzzzzzzzzzzz", b"0", b"000000000000", b"a", b"", b" u1", b"u1 ", b"U1",
            b"user_id_b", b"A\tB", b"uabcdefghijkl", b"uabcdefghijklm", b"this-is-a-long-user-id-0001", b"c_1",
        ];
        let keys: Vec<PKey> = ids.iter().map(|s| PKey::new(s, 0, &mut side)).collect();
        let sides = vec![side];
        for (i, a) in keys.iter().enumerate() {
            let mut buf = [0u8; 12];
            assert_eq!(a.bytes(&sides, &mut buf), ids[i], "{:?}", std::str::from_utf8(ids[i]));
            for (j, b) in keys.iter().enumerate() {
                assert_eq!(a.same(b, &sides), ids[i] == ids[j], "{:?} vs {:?}", std::str::from_utf8(ids[i]), std::str::from_utf8(ids[j]));
                assert_eq!(a.cmp_bytes(b, &sides), ids[i].cmp(ids[j]));
            }
        }
        // equal strings in different side tables hash alike
        let mut s0 = Side::new();
        let mut s1 = Side::new();
        let k0 = PKey::new(b" u1", 0, &mut s0);
        let _ = PKey::new(b"zzz_pad", 1, &mut s1);
        let k1 = PKey::new(b" u1", 1, &mut s1);
        let two = vec![s0, s1];
        assert_eq!(k0.hash(&two), k1.hash(&two));
        assert!(k0.same(&k1, &two));
        assert_eq!(k0.hash_in(&two[0]), k0.hash(&two));
        // SWAR fast paths agree with the generic encoding
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
            let mut buf = [0u8; 9];
            for i in 0..n {
                let r = (x >> (5 * i)) % 40;
                buf[i] = if r < 36 { DIGITS[r as usize] } else { b"A_ -"[(r - 36) as usize] };
            }
            let sl = &buf[..n];
            let valid = sl.iter().all(|&c| CHAR_VAL[c as usize] != 0);
            let mut sd = Side::new();
            let k = PKey::new(sl, 0, &mut sd);
            assert_eq!(k.is_packed(), valid, "{:?}", std::str::from_utf8(sl));
            if valid {
                assert_eq!(k.0, generic(sl), "{:?}", std::str::from_utf8(sl));
            }
        }
        assert!(PKey::new(b"e42huhoug", 0, &mut Side::new()).is_packed());
        assert!(!PKey::new(b"user_id_b", 0, &mut Side::new()).is_packed());
        assert!(!PKey::new(b"", 0, &mut Side::new()).is_packed());
        assert!(!PKey::new(b"abcdefghijklm", 0, &mut Side::new()).is_packed());
        assert!(PKey::new(b"abcdefghijkl", 0, &mut Side::new()).is_packed());
    }
}
