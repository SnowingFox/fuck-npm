//! Publish-time parsing for the packument's `time` object (ISO 8601 / RFC 3339, the format the
//! npm registry writes), without a date library.

/// Unix milliseconds of `2015-01-01T00:00:00.000Z`-style timestamps. Accepts a date alone, a
/// time with or without seconds and fraction, `Z` or a `±hh:mm` / `±hhmm` offset, a space instead
/// of `T`, and treats a missing zone as UTC. `None` for anything else, and for dates before 1970.
pub fn parse_iso8601_ms(input: &str) -> Option<u64> {
    let s = input.trim().as_bytes();
    let mut p = Cursor { s, i: 0 };
    let year = p.digits(4)? as i64;
    p.expect(b'-')?;
    let month = p.digits(2)? as u32;
    p.expect(b'-')?;
    let day = p.digits(2)? as u32;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let (mut hour, mut minute, mut second, mut millis) = (0u32, 0u32, 0u32, 0u32);
    let mut offset_minutes: i64 = 0;
    if p.peek()
        .is_some_and(|c| c == b'T' || c == b't' || c == b' ')
    {
        p.i += 1;
        hour = p.digits(2)? as u32;
        p.expect(b':')?;
        minute = p.digits(2)? as u32;
        if p.peek() == Some(b':') {
            p.i += 1;
            second = p.digits(2)? as u32;
            if p.peek() == Some(b'.') || p.peek() == Some(b',') {
                p.i += 1;
                let start = p.i;
                let mut value = 0u32;
                while let Some(c) = p.peek().filter(u8::is_ascii_digit) {
                    if p.i - start < 3 {
                        value = value * 10 + u32::from(c - b'0');
                    }
                    p.i += 1;
                }
                let n = p.i - start;
                if n == 0 {
                    return None;
                }
                for _ in n..3 {
                    value *= 10;
                }
                millis = value;
            }
        }
        if hour > 24
            || minute > 59
            || second > 60
            || (hour == 24 && (minute, second, millis) != (0, 0, 0))
        {
            return None;
        }
        match p.peek() {
            None => {}
            Some(b'Z' | b'z') => p.i += 1,
            Some(sign @ (b'+' | b'-')) => {
                p.i += 1;
                let oh = p.digits(2)? as i64;
                if p.peek() == Some(b':') {
                    p.i += 1;
                }
                let om = p.digits(2)? as i64;
                if oh > 23 || om > 59 {
                    return None;
                }
                offset_minutes = (oh * 60 + om) * if sign == b'+' { 1 } else { -1 };
            }
            Some(_) => return None,
        }
    }
    if p.i != s.len() {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds =
        days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second.min(59))
            - offset_minutes * 60;
    let ms = seconds.checked_mul(1000)?.checked_add(i64::from(millis))?;
    u64::try_from(ms).ok()
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Option<()> {
        (self.peek()? == c).then(|| self.i += 1)
    }

    fn digits(&mut self, n: usize) -> Option<u64> {
        let bytes = self.s.get(self.i..self.i + n)?;
        if !bytes.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.i += n;
        Some(
            bytes
                .iter()
                .fold(0u64, |acc, b| acc * 10 + u64::from(b - b'0')),
        )
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_timestamps() {
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_iso8601_ms("2015-01-01T00:00:00.000Z"),
            Some(1_420_070_400_000)
        );
        assert_eq!(
            parse_iso8601_ms("2021-03-04T05:06:07.890Z"),
            Some(1_614_834_367_890)
        );
        assert_eq!(
            parse_iso8601_ms("2021-03-04T05:06:07.8Z"),
            Some(1_614_834_367_800)
        );
        assert_eq!(
            parse_iso8601_ms("2021-03-04T05:06:07.123456Z"),
            Some(1_614_834_367_123)
        );
        assert_eq!(
            parse_iso8601_ms("2021-03-04T05:06:07Z"),
            Some(1_614_834_367_000)
        );
        assert_eq!(
            parse_iso8601_ms("2021-03-04T05:06Z"),
            Some(1_614_834_360_000)
        );
        assert_eq!(parse_iso8601_ms("2021-03-04"), Some(1_614_816_000_000));
        assert_eq!(
            parse_iso8601_ms("2021-03-04T07:06:07+02:00"),
            Some(1_614_834_367_000)
        );
        assert_eq!(
            parse_iso8601_ms("2021-03-04T03:06:07-0200"),
            Some(1_614_834_367_000)
        );
        assert_eq!(
            parse_iso8601_ms("2021-03-04 05:06:07"),
            Some(1_614_834_367_000)
        );
        assert_eq!(
            parse_iso8601_ms("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
    }

    #[test]
    fn rejects_non_dates() {
        for bad in [
            "",
            "soon",
            "2021-13-01T00:00:00Z",
            "2021-02-29T00:00:00Z",
            "2021-02-00",
            "2021-03-04T25:00:00Z",
            "2021-03-04T05:06:07.Z",
            "2021-03-04T05:06:07Zjunk",
            "1969-12-31T23:59:59Z",
            "21-03-04",
        ] {
            assert_eq!(parse_iso8601_ms(bad), None, "{bad:?}");
        }
    }
}
