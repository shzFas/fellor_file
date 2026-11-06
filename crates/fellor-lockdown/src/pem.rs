//! Minimal PEM / base64 decoding (no external crates).

/// One `-----BEGIN <label>-----` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemBlock {
    pub label: String,
    pub der: Vec<u8>,
}

/// Decode standard base64 (RFC 4648), ignoring ASCII whitespace.
pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut padding = 0usize;
    for &c in input {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            padding += 1;
            continue;
        }
        if padding > 0 {
            return None; // data after padding
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if padding > 2 || bits >= 6 {
        return None;
    }
    Some(out)
}

/// Parse all PEM blocks in `input`. Malformed blocks are skipped.
pub fn parse_pem(input: &[u8]) -> Vec<PemBlock> {
    let text = String::from_utf8_lossy(input);
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        let Some(label) = line.strip_prefix("-----BEGIN ").and_then(|l| l.strip_suffix("-----")) else {
            continue;
        };
        let end = format!("-----END {label}-----");
        let mut body = String::new();
        let mut closed = false;
        for l in lines.by_ref() {
            let l = l.trim();
            if l == end {
                closed = true;
                break;
            }
            // Skip RFC 1421 headers like "Proc-Type:" (encrypted keys aren't supported anyway).
            if l.contains(':') {
                continue;
            }
            body.push_str(l);
        }
        if !closed {
            break;
        }
        if let Some(der) = base64_decode(body.as_bytes()) {
            out.push(PemBlock { label: label.to_owned(), der });
        }
    }
    out
}

/// First block with the given label, or — if the input isn't PEM at all and
/// looks like DER (starts with SEQUENCE) — the raw input.
pub fn first_block(input: &[u8], labels: &[&str]) -> Option<PemBlock> {
    if let Some(b) = parse_pem(input).into_iter().find(|b| labels.contains(&b.label.as_str())) {
        return Some(b);
    }
    if input.first() == Some(&0x30) {
        return Some(PemBlock { label: String::new(), der: input.to_vec() });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64_decode(b"").unwrap(), b"");
        assert_eq!(base64_decode(b"Zg==").unwrap(), b"f");
        assert_eq!(base64_decode(b"Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode(b"Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode(b"Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode(b"Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode(b"Zm9v\nYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode(b"/+8=").unwrap(), [0xff, 0xef]);
        assert!(base64_decode(b"Zm9v!").is_none());
        assert!(base64_decode(b"Zg==Zg").is_none());
        assert!(base64_decode(b"Z").is_none());
    }

    #[test]
    fn pem_blocks() {
        let pem = b"junk\n-----BEGIN CERTIFICATE-----\nZm9v\nYmFy\n-----END CERTIFICATE-----\n\
-----BEGIN RSA PRIVATE KEY-----\r\nAAEC\r\n-----END RSA PRIVATE KEY-----\r\n-----BEGIN X-----\nnot closed";
        let blocks = parse_pem(pem);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], PemBlock { label: "CERTIFICATE".into(), der: b"foobar".to_vec() });
        assert_eq!(blocks[1].label, "RSA PRIVATE KEY");
        assert_eq!(blocks[1].der, vec![0, 1, 2]);
        assert_eq!(first_block(pem, &["RSA PRIVATE KEY"]).unwrap().der, vec![0, 1, 2]);
        assert!(first_block(pem, &["EC PRIVATE KEY"]).is_none());
        assert_eq!(first_block(&[0x30, 0x03, 1, 2, 3], &["CERTIFICATE"]).unwrap().der.len(), 5);
        assert!(parse_pem(&[0xff, 0xfe]).is_empty());
    }
}
