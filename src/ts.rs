//! Timestamp parsing per rules.md: F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 with `+HH:MM`/`-HH:MM`, F3 epoch millis digits.

pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;

#[inline(always)]
fn d(b: u8) -> Option<i64> {
    if b.is_ascii_digit() { Some((b - b'0') as i64) } else { None }
}

#[inline(always)]
fn two(s: &[u8], i: usize) -> Option<i64> {
    Some(d(s[i])? * 10 + d(s[i + 1])?)
}

#[inline(always)]
fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

#[inline(always)]
pub fn days_from_civil(y: i64, m: i64, dd: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + dd - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Parses the 19-byte civil part `YYYY-MM-DDTHH:MM:SS` at the start of `s` into epoch ms (UTC, no offset).
#[inline(always)]
fn civil19(s: &[u8]) -> Option<i64> {
    debug_assert!(s.len() >= 19);
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = two(s, 0)? * 100 + two(s, 2)?;
    let m = two(s, 5)?;
    let dd = two(s, 8)?;
    let hh = two(s, 11)?;
    let mi = two(s, 14)?;
    let ss = two(s, 17)?;
    if m < 1 || m > 12 || dd < 1 || hh > 23 || mi > 59 || ss > 59 {
        return None;
    }
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => if is_leap(y) { 29 } else { 28 },
    };
    if dd > dim {
        return None;
    }
    Some(days_from_civil(y, m, dd) * DAY + hh * HOUR + mi * MINUTE + ss * SECOND)
}

/// Strict F1 only (used for ingest_ts and the manifest).
#[inline]
pub fn parse_f1(s: &[u8]) -> Option<i64> {
    if s.len() != 20 || s[19] != b'Z' {
        return None;
    }
    civil19(s)
}

/// Any of F1/F2/F3. Returns epoch ms. None = bad timestamp. Range is not checked here.
#[inline]
pub fn parse_ts(s: &[u8]) -> Option<i64> {
    let n = s.len();
    if n == 0 {
        return None;
    }
    let first = s[0];
    if first.is_ascii_digit() && (n < 5 || s[4] != b'-') {
        // F3: digits only. Leading zeros allowed. Overflow => not representable => treat as bad (out of window).
        let mut i = 0;
        while i < n && s[i] == b'0' {
            i += 1;
        }
        let rest = &s[i..];
        if rest.len() > 19 {
            // still must be all digits to be "well-formed", but either way it is bad
            return None;
        }
        let mut v: u64 = 0;
        for &c in rest {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as u64;
        }
        // check the skipped zeros region was digits (it was: all '0')
        if v > i64::MAX as u64 {
            return None;
        }
        // verify every byte in s is a digit (leading part all '0', rest checked)
        return Some(v as i64);
    }
    match n {
        20 => {
            if s[19] != b'Z' {
                return None;
            }
            civil19(s)
        }
        25 => {
            let sign = match s[19] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if s[22] != b':' {
                return None;
            }
            let oh = two(s, 20)?;
            let om = two(s, 23)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let base = civil19(s)?;
            Some(base - sign * (oh * HOUR + om * MINUTE))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forms() {
        let a = parse_ts(b"2026-10-03T23:58:10Z").unwrap();
        let b = parse_ts(b"2026-10-03T19:58:10-04:00").unwrap();
        let c = parse_ts(b"1791071890000").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(parse_ts(b"0001791071890000").unwrap(), c);
        assert!(parse_ts(b"2026-10-03t23:58:10Z").is_none());
        assert!(parse_ts(b"2026-10-03T23:58:10z").is_none());
        assert!(parse_ts(b"2026-10-03 23:58:10Z").is_none());
        assert!(parse_ts(b"2026-10-03T23:58:10").is_none());
        assert!(parse_ts(b"2026-10-03T23:58:10.000Z").is_none());
        assert!(parse_ts(b"1791071890000.0").is_none());
        assert!(parse_ts(b"-5").is_none());
        assert!(parse_ts(b"2026-13-03T23:58:10Z").is_none());
        assert!(parse_ts(b"2027-02-29T00:00:00Z").is_none());
        assert!(parse_ts(b"2028-02-29T00:00:00Z").is_some());
        assert!(parse_ts(b"2026-10-03T24:00:00Z").is_none());
        assert!(parse_ts(b"2026-10-03T23:58:60Z").is_none());
        assert!(parse_ts(b"2026-10-03T23:58:10+24:00").is_none());
        assert!(parse_ts(b"2026-10-03T23:58:10+05:60").is_none());
        assert!(parse_ts(b"2026-10-03T23:58:10+0200").is_none());
        assert!(parse_ts(b"2026-10-03T23:58:10-00:00").is_some());
        assert!(parse_ts(b"2026-10-03T23:58:10+23:59").is_some());
        assert_eq!(parse_ts(b"86400000"), Some(86_400_000));
        assert!(parse_ts(b"123456789012345678901").is_none());
        assert!(parse_ts(b"").is_none());
        assert!(parse_ts(b"2026-10-03T23:58:10Z\r").is_none());
    }
}
