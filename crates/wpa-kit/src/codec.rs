//! 零依赖编解码：base64 / hex / URL。
//!
//! 不引第三方 crate 是为了让 75 个插件各自链接时体积最小（实测每个多 200KB 没意义）。

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准 base64 编码（带 `=` 填充）。
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 解码；遇到非法字符直接丢弃（容错优先，调用方多为网络数据）。
pub fn b64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => continue,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

/// base64url（无填充）——JWT 用。
pub fn b64url_decode(s: &str) -> Vec<u8> {
    b64_decode(s)
}

pub fn hex_encode(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

pub fn hex_decode(s: &str) -> Vec<u8> {
    let clean: Vec<u8> = s.bytes().filter(|c| c.is_ascii_hexdigit()).collect();
    clean
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| {
            let hi = (c[0] as char).to_digit(16).unwrap_or(0) as u8;
            let lo = (c[1] as char).to_digit(16).unwrap_or(0) as u8;
            (hi << 4) | lo
        })
        .collect()
}

/// URL 百分号编码（`safe` 之外的字节转 `%XX`；空格转 `+` 由调用方决定）。
pub fn url_encode(s: &str, plus_space: bool) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(b as char);
        } else if b == b' ' && plus_space {
            out.push('+');
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// 解码 `%XX` 与 `+`。
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 解析查询串为键值对（保留重复键）。
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let mut it = kv.splitn(2, '=');
            let k = url_decode(it.next().unwrap_or(""));
            let v = url_decode(it.next().unwrap_or(""));
            (k, v)
        })
        .collect()
}

/// 拼查询串。
pub fn build_query(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k, true), url_encode(v, true)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "中文"] {
            let e = b64_encode(s.as_bytes());
            assert_eq!(b64_decode(&e), s.as_bytes(), "{s} -> {e}");
        }
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64_encode(b"f"), "Zg==");
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x1a]), "00ff1a");
        assert_eq!(hex_decode("00:ff:1a"), vec![0x00, 0xff, 0x1a]);
    }

    #[test]
    fn url_roundtrip() {
        let s = "a b&c=d/e?f";
        let e = url_encode(s, true);
        assert_eq!(url_decode(&e), s);
        assert_eq!(url_encode("a b", true), "a+b");
        assert_eq!(url_encode("a b", false), "a%20b");
    }

    #[test]
    fn query_pairs() {
        let p = parse_query("a=1&b=2&c=&a=3");
        assert_eq!(p.len(), 4);
        assert_eq!(p[0], ("a".into(), "1".into()));
        assert_eq!(build_query(&[("a b".into(), "c&d".into())]), "a+b=c%26d");
    }
}
