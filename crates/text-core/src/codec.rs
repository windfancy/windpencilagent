//! 编解码：base64 / base64url / url / hex / html entity。
//!
//! 自研实现（不引 percent-encoding 之类的小依赖），因为规则简单且需要可控的
//! 错误行为：解码失败必须报错，**不能**静默丢字节。

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;

/// 支持的方案。
pub const SCHEMES: &[&str] = &["base64", "base64url", "url", "hex", "html"];

pub fn encode(scheme: &str, data: &str) -> Result<String, String> {
    match scheme {
        "base64" => Ok(STANDARD.encode(data.as_bytes())),
        "base64url" => Ok(URL_SAFE_NO_PAD.encode(data.as_bytes())),
        "hex" => Ok(data.as_bytes().iter().map(|b| format!("{b:02x}")).collect()),
        "url" => Ok(url_encode(data)),
        "html" => Ok(html_encode(data)),
        other => Err(format!("不支持的编码方案：{other}（可选 {}）", SCHEMES.join("/"))),
    }
}

pub fn decode(scheme: &str, data: &str) -> Result<String, String> {
    match scheme {
        "base64" => {
            let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
            let bytes = STANDARD
                .decode(cleaned.as_bytes())
                .map_err(|e| format!("base64 解码失败：{e}"))?;
            String::from_utf8(bytes).map_err(|e| format!("base64 内容不是 UTF-8：{e}"))
        }
        "base64url" => {
            let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
            let trimmed = cleaned.trim_end_matches('=');
            URL_SAFE_NO_PAD
                .decode(trimmed.as_bytes())
                .map_err(|e| format!("base64url 解码失败：{e}"))
                .and_then(|b| String::from_utf8(b).map_err(|e| format!("内容不是 UTF-8：{e}")))
        }
        "hex" => {
            let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
            if cleaned.len() % 2 != 0 {
                return Err("hex 长度必须为偶数".into());
            }
            let mut bytes = Vec::with_capacity(cleaned.len() / 2);
            let cs: Vec<char> = cleaned.chars().collect();
            for pair in cs.chunks(2) {
                let s: String = pair.iter().collect();
                let b = u8::from_str_radix(&s, 16).map_err(|_| format!("非法 hex 字节：{s}"))?;
                bytes.push(b);
            }
            String::from_utf8(bytes).map_err(|e| format!("hex 内容不是 UTF-8：{e}"))
        }
        "url" => Ok(url_decode(data)),
        "html" => Ok(html_decode(data)),
        other => Err(format!("不支持的解码方案：{other}（可选 {}）", SCHEMES.join("/"))),
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
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
                        out.push(b'%');
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

fn html_encode(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn html_decode(s: &str) -> String {
    // 数值实体（&#65; / &#x41;）与常见命名实体
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        let Some(semi) = after.find(';').filter(|i| *i <= 12) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let ent = &after[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            other => {
                let code = if let Some(h) = other.strip_prefix("#x").or_else(|| other.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).ok()
                } else if let Some(d) = other.strip_prefix('#') {
                    d.parse::<u32>().ok()
                } else {
                    None
                };
                code.and_then(char::from_u32)
            }
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_and_utf8() {
        let s = "你好 windpencilagent";
        let e = encode("base64", s).unwrap();
        assert_eq!(decode("base64", &e).unwrap(), s);
        // 带换行的 base64（常见于证书/邮件）应容忍
        assert_eq!(decode("base64", &format!("{e}\n")).unwrap(), s);
    }

    #[test]
    fn base64url_no_padding() {
        let e = encode("base64url", "a?b/c").unwrap();
        assert!(!e.contains('+') && !e.contains('/') && !e.contains('='));
        assert_eq!(decode("base64url", &e).unwrap(), "a?b/c");
    }

    #[test]
    fn hex_roundtrip_and_errors() {
        let e = encode("hex", "AB") .unwrap();
        assert_eq!(e, "4142");
        assert_eq!(decode("hex", &e).unwrap(), "AB");
        assert!(decode("hex", "abc").is_err());
        assert!(decode("hex", "zz").is_err());
    }

    #[test]
    fn url_roundtrip() {
        let s = "a b&c=d/中文";
        let e = encode("url", s).unwrap();
        assert!(e.contains("%20"));
        assert_eq!(decode("url", &e).unwrap(), s);
        assert_eq!(decode("url", "a+b").unwrap(), "a b");
    }

    #[test]
    fn html_entities() {
        let e = encode("html", "<a href=\"x\">&</a>").unwrap();
        assert!(e.starts_with("&lt;a"));
        assert_eq!(decode("html", &e).unwrap(), "<a href=\"x\">&</a>");
        assert_eq!(decode("html", "&#65;&#x42;").unwrap(), "AB");
        assert_eq!(decode("html", "&unknown;").unwrap(), "&unknown;");
    }

    #[test]
    fn unknown_scheme_rejected() {
        assert!(encode("rot13", "x").is_err());
        assert!(decode("rot13", "x").is_err());
    }
}
