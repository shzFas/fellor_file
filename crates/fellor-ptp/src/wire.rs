//! Little-endian PTP primitive encoding: integers, PTP strings, arrays and
//! the PTP DateTime string format.

use crate::error::{PtpError, PtpResult};
use std::time::SystemTime;

/// Cursor over a received dataset.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn take(&mut self, n: usize) -> PtpResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(PtpError::protocol(format!(
                "dataset truncated: need {n} bytes at offset {}, have {}",
                self.pos,
                self.remaining()
            )));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn arr<const N: usize>(&mut self) -> PtpResult<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    pub fn u8(&mut self) -> PtpResult<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> PtpResult<u16> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    pub fn u32(&mut self) -> PtpResult<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    pub fn u64(&mut self) -> PtpResult<u64> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    pub fn u128(&mut self) -> PtpResult<u128> {
        Ok(u128::from_le_bytes(self.arr()?))
    }

    /// PTP string: u8 number of UCS-2 code units including the terminating
    /// NUL (0 = empty string), followed by that many UTF-16LE units.
    pub fn string(&mut self) -> PtpResult<String> {
        let n = self.u8()? as usize;
        if n == 0 {
            return Ok(String::new());
        }
        let raw = self.take(n * 2)?;
        let mut units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        // Strip terminator (and anything after a stray embedded NUL).
        if let Some(i) = units.iter().position(|&u| u == 0) {
            units.truncate(i);
        }
        Ok(String::from_utf16_lossy(&units))
    }

    /// Array of u16: u32 element count then elements.
    pub fn u16_array(&mut self) -> PtpResult<Vec<u16>> {
        let n = self.array_len(2)?;
        (0..n).map(|_| self.u16()).collect()
    }

    pub fn u32_array(&mut self) -> PtpResult<Vec<u32>> {
        let n = self.array_len(4)?;
        (0..n).map(|_| self.u32()).collect()
    }

    /// Reads an array count and sanity-checks it against the remaining bytes.
    pub fn array_len(&mut self, elem_size: usize) -> PtpResult<usize> {
        let n = self.u32()? as usize;
        if n.saturating_mul(elem_size) > self.remaining() {
            return Err(PtpError::protocol(format!("array of {n} elements exceeds dataset")));
        }
        Ok(n)
    }
}

/// Growable little-endian writer.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u128(&mut self, v: u128) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// Encode a PTP string. Strings longer than 254 UTF-16 units are
    /// truncated (the count byte includes the NUL and must fit in u8).
    pub fn string(&mut self, s: &str) -> &mut Self {
        if s.is_empty() {
            return self.u8(0);
        }
        let mut units: Vec<u16> = s.encode_utf16().collect();
        if units.len() > 254 {
            units.truncate(254);
            // Don't leave a dangling high surrogate.
            if matches!(units.last(), Some(0xD800..=0xDBFF)) {
                units.pop();
            }
        }
        self.u8((units.len() + 1) as u8);
        for u in units {
            self.u16(u);
        }
        self.u16(0)
    }

    pub fn u16_array(&mut self, v: &[u16]) -> &mut Self {
        self.u32(v.len() as u32);
        for &x in v {
            self.u16(x);
        }
        self
    }

    pub fn u32_array(&mut self, v: &[u32]) -> &mut Self {
        self.u32(v.len() as u32);
        for &x in v {
            self.u32(x);
        }
        self
    }
}

/// Parse a PTP DateTime string `YYYYMMDDThhmmss[.s][Z|±hhmm]`.
/// Without a zone designator the time is device-local; we interpret it in
/// the host's local zone (phones and the Mac normally agree).
pub fn parse_datetime(s: &str) -> Option<SystemTime> {
    use chrono::{FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
    let s = s.trim();
    if s.len() < 15 || s.as_bytes().get(8) != Some(&b'T') {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<u32> { s.get(a..b)?.parse().ok() };
    let date = NaiveDate::from_ymd_opt(num(0, 4)? as i32, num(4, 6)?, num(6, 8)?)?;
    let naive: NaiveDateTime = date.and_hms_opt(num(9, 11)?, num(11, 13)?, num(13, 15)?)?;
    let mut rest = &s[15..];
    // Optional tenths of a second.
    let mut tenths = 0u32;
    if let Some(r) = rest.strip_prefix('.') {
        let digits: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
        tenths = digits.chars().next().and_then(|c| c.to_digit(10)).unwrap_or(0);
        rest = &r[digits.len()..];
    }
    let naive = naive + chrono::Duration::milliseconds(tenths as i64 * 100);
    let dt: chrono::DateTime<Utc> = if rest.is_empty() {
        Local.from_local_datetime(&naive).earliest()?.with_timezone(&Utc)
    } else if rest == "Z" {
        Utc.from_utc_datetime(&naive)
    } else {
        let sign = match rest.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let hh: i32 = rest.get(1..3)?.parse().ok()?;
        let mm: i32 = rest.get(3..5).unwrap_or("00").parse().ok()?;
        let off = FixedOffset::east_opt(sign * (hh * 3600 + mm * 60))?;
        off.from_local_datetime(&naive).single()?.with_timezone(&Utc)
    };
    Some(dt.into())
}

/// Format a time as a PTP DateTime string in the host local zone, without a
/// zone suffix (most widely accepted form; Android treats it as local).
pub fn format_datetime(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y%m%dT%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn string_bytes() {
        let mut w = Writer::new();
        w.string("DCIM");
        assert_eq!(
            w.buf,
            [0x05, b'D', 0, b'C', 0, b'I', 0, b'M', 0, 0, 0],
            "count includes NUL, UCS-2LE"
        );
        let mut r = Reader::new(&w.buf);
        assert_eq!(r.string().unwrap(), "DCIM");
        assert!(r.is_empty());
    }

    #[test]
    fn empty_string_is_single_zero_byte() {
        let mut w = Writer::new();
        w.string("");
        assert_eq!(w.buf, [0]);
        assert_eq!(Reader::new(&[0]).string().unwrap(), "");
    }

    #[test]
    fn cyrillic_and_emoji_string() {
        let s = "Фото 📷";
        let mut w = Writer::new();
        w.string(s);
        // 4 cyrillic + space + surrogate pair (2 units) + NUL = 8 units
        assert_eq!(w.buf[0], 8);
        assert_eq!(&w.buf[1..3], &[0x24, 0x04]); // 'Ф' = U+0424
        assert_eq!(Reader::new(&w.buf).string().unwrap(), s);
    }

    #[test]
    fn long_string_truncated_to_255_units() {
        let s = "a".repeat(400);
        let mut w = Writer::new();
        w.string(&s);
        assert_eq!(w.buf[0], 255);
        assert_eq!(w.buf.len(), 1 + 255 * 2);
        assert_eq!(Reader::new(&w.buf).string().unwrap().len(), 254);
    }

    #[test]
    fn string_without_terminator_tolerated() {
        // Some cameras omit the NUL even though the count says 2.
        let raw = [0x02, b'A', 0, b'B', 0];
        assert_eq!(Reader::new(&raw).string().unwrap(), "AB");
    }

    #[test]
    fn arrays() {
        let raw = [0x03, 0, 0, 0, 0x01, 0x10, 0x02, 0x10, 0x05, 0x98];
        let mut r = Reader::new(&raw);
        assert_eq!(r.u16_array().unwrap(), vec![0x1001, 0x1002, 0x9805]);
        let mut w = Writer::new();
        w.u16_array(&[0x1001, 0x1002, 0x9805]);
        assert_eq!(w.buf, raw);
    }

    #[test]
    fn array_count_overflow_rejected() {
        let raw = [0xFF, 0xFF, 0xFF, 0x7F, 0x01, 0x00];
        assert!(Reader::new(&raw).u32_array().is_err());
    }

    #[test]
    fn truncated_integer() {
        assert!(Reader::new(&[1, 2, 3]).u32().is_err());
    }

    #[test]
    fn datetime_utc_and_offset() {
        let t = parse_datetime("20240131T235959Z").unwrap();
        assert_eq!(t.duration_since(UNIX_EPOCH).unwrap(), Duration::from_secs(1706745599));
        let t2 = parse_datetime("20240201T025959+0300").unwrap();
        assert_eq!(t, t2);
        let t3 = parse_datetime("20240131T235959.5Z").unwrap();
        assert_eq!(t3.duration_since(t).unwrap(), Duration::from_millis(500));
        assert!(parse_datetime("").is_none());
        assert!(parse_datetime("garbage").is_none());
    }

    #[test]
    fn datetime_local_roundtrip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let s = format_datetime(t);
        assert_eq!(s.len(), 15);
        assert_eq!(parse_datetime(&s).unwrap(), t);
    }
}
