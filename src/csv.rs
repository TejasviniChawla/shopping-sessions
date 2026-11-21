//! The CSV dialect of rules.md: fields split on every comma, optional whole-field double quotes,
//! any other quote is malformed, wrong field count is malformed, empty line is malformed.

/// Unquote one raw field. None = malformed.
#[inline(always)]
pub fn unquote(f: &[u8]) -> Option<&[u8]> {
    let n = f.len();
    let mut has_quote = false;
    for &c in f {
        has_quote |= c == b'"';
    }
    if !has_quote {
        return Some(f);
    }
    if n >= 2 && f[0] == b'"' && f[n - 1] == b'"' {
        let inner = &f[1..n - 1];
        for &c in inner {
            if c == b'"' {
                return None;
            }
        }
        return Some(inner);
    }
    None
}

/// Scalar reference splitter. Returns false if malformed.
#[inline]
pub fn split_fields_scalar<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
    let mut nf = 0usize;
    let mut start = 0usize;
    let n = line.len();
    let mut i = 0usize;
    loop {
        if i == n || line[i] == b',' {
            if nf >= N {
                return false;
            }
            match unquote(&line[start..i]) {
                Some(f) => out[nf] = f,
                None => return false,
            }
            nf += 1;
            start = i + 1;
            if i == n {
                break;
            }
        }
        i += 1;
    }
    nf == N
}

/// Comma and quote bitmasks over the first 128 bytes at `p` (over-read allowed: caller guarantees padding).
#[inline(always)]
#[allow(unused_unsafe)]
unsafe fn masks128(p: *const u8) -> (u128, u128) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    {
        use std::arch::x86_64::*;
        let comma = _mm256_set1_epi8(b',' as i8);
        let quote = _mm256_set1_epi8(b'"' as i8);
        let mut cm: u128 = 0;
        let mut qm: u128 = 0;
        let mut i = 0;
        while i < 4 {
            let v = _mm256_loadu_si256(p.add(32 * i) as *const __m256i);
            let c = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, comma)) as u32 as u128;
            let q = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, quote)) as u32 as u128;
            cm |= c << (32 * i);
            qm |= q << (32 * i);
            i += 1;
        }
        return (cm, qm);
    }
    #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
    {
        use std::arch::x86_64::*;
        let comma = _mm_set1_epi8(b',' as i8);
        let quote = _mm_set1_epi8(b'"' as i8);
        let mut cm: u128 = 0;
        let mut qm: u128 = 0;
        let mut i = 0;
        while i < 8 {
            let v = _mm_loadu_si128(p.add(16 * i) as *const __m128i);
            let c = _mm_movemask_epi8(_mm_cmpeq_epi8(v, comma)) as u32 as u128;
            let q = _mm_movemask_epi8(_mm_cmpeq_epi8(v, quote)) as u32 as u128;
            cm |= c << (16 * i);
            qm |= q << (16 * i);
            i += 1;
        }
        return (cm, qm);
    }
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        let comma = vdupq_n_u8(b',');
        let quote = vdupq_n_u8(b'"');
        let mut cm: u128 = 0;
        let mut qm: u128 = 0;
        let mut i = 0;
        while i < 8 {
            let v = vld1q_u8(p.add(16 * i));
            cm |= (movemask16(vceqq_u8(v, comma)) as u128) << (16 * i);
            qm |= (movemask16(vceqq_u8(v, quote)) as u128) << (16 * i);
            i += 1;
        }
        return (cm, qm);
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let s = std::slice::from_raw_parts(p, 128);
        let mut cm: u128 = 0;
        let mut qm: u128 = 0;
        for (i, &b) in s.iter().enumerate() {
            cm |= ((b == b',') as u128) << i;
            qm |= ((b == b'"') as u128) << i;
        }
        (cm, qm)
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn movemask16(v: std::arch::aarch64::uint8x16_t) -> u16 {
    use std::arch::aarch64::*;
    // Each lane is 0x00 or 0xFF. Narrow pairs of bytes to nibbles, then gather the 16 nibble-bits.
    let n = vshrn_n_u16::<4>(vreinterpretq_u16_u8(v)); // 8 bytes, each byte = two 4-bit lanes
    let x = vget_lane_u64::<0>(vreinterpret_u64_u8(n));
    // x has 16 nibbles each 0x0 or 0xF; keep bit 0 of each nibble then pack.
    let x = x & 0x1111_1111_1111_1111;
    let x = (x | (x >> 3)) & 0x0303_0303_0303_0303; // pairs -> 2 bits per byte
    let x = (x | (x >> 6)) & 0x000F_000F_000F_000F; // -> 4 bits per 16
    let x = (x | (x >> 12)) & 0x0000_00FF_0000_00FF; // -> 8 bits per 32
    ((x | (x >> 24)) & 0xFFFF) as u16
}

/// SIMD splitter. Requires that reading 128 bytes from `line.as_ptr()` is in bounds (buffer padding).
/// Falls back to the scalar splitter for lines of 128 bytes or longer.
#[inline(always)]
pub fn split_fields<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
    let n = line.len();
    if n == 0 || n >= 128 {
        return split_fields_scalar(line, out);
    }
    let (cm, qm) = unsafe { masks128(line.as_ptr()) };
    let valid: u128 = if n == 128 { u128::MAX } else { (1u128 << n) - 1 };
    let cm = cm & valid;
    let qm = qm & valid;
    if cm.count_ones() as usize != N - 1 {
        return false;
    }
    if qm == 0 {
        // fast path: no quotes
        let mut s = 0usize;
        let mut c = cm;
        for k in 0..N - 1 {
            let e = c.trailing_zeros() as usize;
            c &= c - 1;
            out[k] = &line[s..e];
            s = e + 1;
        }
        out[N - 1] = &line[s..n];
        return true;
    }
    // quotes: a quote is valid only as the first and last byte of a field of length >= 2
    let starts = (cm << 1) | 1;
    let ends = (cm >> 1) | (1u128 << (n - 1));
    let open = qm & starts;
    let close = qm & ends;
    if (open | close) != qm || (open & close) != 0 {
        return false;
    }
    let mut s = 0usize;
    let mut c = cm | (1u128 << n);
    for k in 0..N {
        let e = c.trailing_zeros() as usize;
        c &= c - 1;
        let o = (open >> s) & 1 == 1;
        let cl = e > s && (close >> (e - 1)) & 1 == 1;
        if o != cl {
            return false;
        }
        out[k] = if o { &line[s + 1..e - 1] } else { &line[s..e] };
        s = e + 1;
    }
    true
}

/// Iterate lines of `buf[start..end]`; each line ends at '\n' or at `end`.
pub struct Lines<'a> {
    buf: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Lines<'a> {
    pub fn new(buf: &'a [u8], start: usize, end: usize) -> Self {
        Lines { buf, pos: start, end }
    }
}

impl<'a> Iterator for Lines<'a> {
    type Item = &'a [u8];
    #[inline]
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.end {
            return None;
        }
        let rest = &self.buf[self.pos..self.end];
        match memchr::memchr(b'\n', rest) {
            Some(k) => {
                let line = &rest[..k];
                self.pos += k + 1;
                Some(line)
            }
            None => {
                self.pos = self.end;
                Some(rest)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded(s: &[u8]) -> Vec<u8> {
        let mut v = s.to_vec();
        v.resize(s.len() + 256, 0);
        v
    }

    fn both<const N: usize>(s: &[u8]) -> (bool, Vec<Vec<u8>>, bool, Vec<Vec<u8>>) {
        let p = padded(s);
        let line = &p[..s.len()];
        let mut a = [&b""[..]; N];
        let mut b = [&b""[..]; N];
        let ra = split_fields_scalar(line, &mut a);
        let rb = split_fields(line, &mut b);
        (ra, a.iter().map(|x| x.to_vec()).collect(), rb, b.iter().map(|x| x.to_vec()).collect())
    }

    #[test]
    fn fields() {
        let cases: Vec<&[u8]> = vec![
            b"a,\"b\",", b"\"\",x,y", b"a,b", b"a,b,c,d", b"a,b\",c", b"\"a,b,c", b"a,\"b\"x,c", b"\",a,b", b"\"\"\",a,b",
            b"a,b,\"", b"a,b,\"\"", b",,", b"\",\",a", b"\"a\",\"b\",\"c\"", b"a,b,c\r", b"\"a\"\"b\",c,d",
        ];
        for c in cases {
            let (ra, a, rb, b) = both::<3>(c);
            assert_eq!(ra, rb, "{:?}", std::str::from_utf8(c));
            if ra {
                assert_eq!(a, b, "{:?}", std::str::from_utf8(c));
            }
        }
        let (ra, _, rb, _) = both::<2>(b"");
        assert!(!ra && !rb);
        let (ra, a, rb, b) = both::<2>(b" u1,u1 ");
        assert!(ra && rb);
        assert_eq!(a, b);
        assert_eq!(a[0], b" u1");
        // long line falls back to scalar
        let long = vec![b'x'; 200];
        let mut l = long.clone();
        l[50] = b',';
        l[100] = b',';
        let (ra, a, rb, b) = both::<3>(&l);
        assert!(ra && rb);
        assert_eq!(a, b);
    }

    #[test]
    fn random_lines() {
        // pseudo-random lines of commas, quotes, letters: scalar and simd must agree
        let mut x: u64 = 0x1234_5678_9abc_def0;
        for _ in 0..20000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 40) as usize;
            let mut s = Vec::with_capacity(len);
            let mut y = x;
            for _ in 0..len {
                y = y.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                s.push(match (y >> 60) % 5 {
                    0 => b',',
                    1 => b'"',
                    _ => b'a' + ((y >> 50) % 3) as u8,
                });
            }
            let (ra, a, rb, b) = both::<4>(&s);
            assert_eq!(ra, rb, "{:?}", String::from_utf8_lossy(&s));
            if ra {
                assert_eq!(a, b, "{:?}", String::from_utf8_lossy(&s));
            }
        }
    }
}
