//! The CSV dialect of rules.md: fields split on every comma, optional whole-field double quotes,
//! any other quote is malformed, wrong field count is malformed, empty line is malformed.

/// Unquote one raw field. None = malformed.
#[inline(always)]
pub fn unquote(f: &[u8]) -> Option<&[u8]> {
    let n = f.len();
    // Fast path: no quote anywhere.
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

/// Split `line` (without the trailing '\n') into exactly N fields. Returns false if malformed.
#[inline(always)]
pub fn split_fields<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
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

/// Iterate lines of `buf[start..end]` where each line ends at '\n' (or at `end` for the last one).
/// `end` must be a line boundary (a position right after '\n' or buf.len()).
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
    #[test]
    fn fields() {
        let mut f = [&b""[..]; 3];
        assert!(split_fields(b"a,\"b\",", &mut f));
        assert_eq!(f, [&b"a"[..], b"b", b""]);
        assert!(split_fields(b"\"\",x,y", &mut f));
        assert_eq!(f[0], b"");
        assert!(!split_fields(b"a,b", &mut f));
        assert!(!split_fields(b"a,b,c,d", &mut f));
        assert!(!split_fields(b"a,b\",c", &mut f));
        assert!(!split_fields(b"\"a,b,c", &mut f));
        assert!(!split_fields(b"a,\"b\"x,c", &mut f));
        let mut g = [&b""[..]; 2];
        assert!(!split_fields(b"", &mut g));
        assert!(split_fields(b" u1,u1 ", &mut g));
        assert_eq!(g, [&b" u1"[..], b"u1 "]);
    }
}
