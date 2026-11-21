//! Byte references into the mmapped input files, hashing, and small open-addressing tables.

use memmap2::Mmap;

/// A slice of one input file: (file index, offset, length). 8 bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ref {
    pub off: u32,
    pub file: u16,
    pub len: u16,
}

impl Ref {
    #[inline(always)]
    pub fn new(file: usize, base: *const u8, s: &[u8]) -> Ref {
        let off = (s.as_ptr() as usize - base as usize) as u32;
        Ref { off, file: file as u16, len: s.len() as u16 }
    }
    #[inline(always)]
    pub fn get<'a>(&self, files: &'a [Mmap]) -> &'a [u8] {
        let f = &files[self.file as usize];
        &f[self.off as usize..self.off as usize + self.len as usize]
    }
}

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
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// An ID key: up to 15 bytes stored inline (top byte = length), longer ones as a Ref (top byte 0xFF).
/// Inline keys compare with one u128 equality; nothing after parsing needs the input files again.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Key(u128);

impl Key {
    #[inline(always)]
    pub fn new(s: &[u8], file: usize, base: *const u8) -> Key {
        let n = s.len();
        if n <= 15 {
            let mut buf = [0u8; 16];
            buf[..n].copy_from_slice(s);
            buf[15] = n as u8;
            Key(u128::from_le_bytes(buf))
        } else {
            let r = Ref::new(file, base, s);
            let packed = (r.off as u128) | ((r.file as u128) << 32) | ((r.len as u128) << 48);
            Key(packed | (0xFFu128 << 120))
        }
    }
    #[inline(always)]
    pub fn is_inline(&self) -> bool {
        (self.0 >> 120) as u8 != 0xFF
    }
    #[inline(always)]
    pub fn bytes<'a>(&'a self, files: &'a [Mmap]) -> &'a [u8] {
        let tag = (self.0 >> 120) as u8;
        if tag != 0xFF {
            // little-endian: byte 0 of the u128 is at the lowest address
            unsafe { std::slice::from_raw_parts(self as *const Key as *const u8, tag as usize) }
        } else {
            let off = self.0 as u32 as usize;
            let file = (self.0 >> 32) as u16 as usize;
            let len = (self.0 >> 48) as u16 as usize;
            &files[file][off..off + len]
        }
    }
    #[inline(always)]
    pub fn same(&self, other: &Key, files: &[Mmap]) -> bool {
        if self.is_inline() && other.is_inline() {
            self.0 == other.0
        } else {
            self.bytes(files) == other.bytes(files)
        }
    }
    #[inline(always)]
    pub fn cmp_bytes(&self, other: &Key, files: &[Mmap]) -> std::cmp::Ordering {
        self.bytes(files).cmp(other.bytes(files))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_inline() {
        let f: Vec<Mmap> = Vec::new();
        let a = Key::new(b"u1", 0, std::ptr::null());
        let b = Key::new(b"u1", 0, std::ptr::null());
        let c = Key::new(b"U1", 0, std::ptr::null());
        let d = Key::new(b"u1 ", 0, std::ptr::null());
        assert!(a.same(&b, &f));
        assert!(!a.same(&c, &f));
        assert!(!a.same(&d, &f));
        assert_eq!(a.bytes(&f), b"u1");
        assert_eq!(Key::new(b"", 0, std::ptr::null()).bytes(&f), b"");
        assert_eq!(Key::new(b"123456789012345", 0, std::ptr::null()).bytes(&f), b"123456789012345");
        assert_eq!(c.cmp_bytes(&a, &f), std::cmp::Ordering::Less);
    }
}
