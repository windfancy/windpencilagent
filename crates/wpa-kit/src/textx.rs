//! 文本小工具：URL 拆解、HTML 实体、相对路径、规范化扫描。
//!
//! **这里刻意不含正则引擎**：`regex` crate 自带约 900KB（DFA/AST/字节码），
//! 而 75 个插件里只有少数真需要正则。所以：
//! - 通用规范化（UUID / 时间戳 / 邮箱）用手写扫描器，零依赖且更快；
//! - 真需要正则的插件（`text_extract`、`regex_match`、`tmpl_*`）自己引 `regex`，
//!   体积代价由它自己承担（实测每多一个 regex 用户约 +800KB）。

/// 解析 URL 的各部分（够用即可，不追求 RFC 完整性）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UrlParts {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: String,
}

impl UrlParts {
    pub fn origin(&self) -> String {
        let port = match self.port {
            Some(p) if !is_default_port(&self.scheme, p) => format!(":{p}"),
            _ => String::new(),
        };
        format!("{}://{}{}", self.scheme, self.host, port)
    }

    pub fn authority(&self) -> String {
        match self.port {
            Some(p) if !is_default_port(&self.scheme, p) => format!("{}:{}", self.host, p),
            _ => self.host.clone(),
        }
    }
}

fn is_default_port(scheme: &str, port: u16) -> bool {
    (scheme == "http" && port == 80) || (scheme == "https" && port == 443)
}

pub fn parse_url(url: &str) -> Option<UrlParts> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let (authority, path_query) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    // 主机名大小写不敏感：统一小写，避免 scope 比较时 `Example.com` 与 `example.com` 认不出同一目标。
    let authority_owned = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .to_ascii_lowercase();
    let authority = authority_owned.as_str();
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, tail)) => (
                format!("[{h}]"),
                tail.strip_prefix(':').and_then(|p| p.parse().ok()),
            ),
            None => (authority.to_string(), None),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_string(), p.parse().ok()),
            _ => (authority.to_string(), None),
        }
    };
    let (path, query) = match path_query.split_once('?') {
        Some((p, q)) => (p.to_string(), q.split('#').next().unwrap_or("").to_string()),
        None => (
            path_query.split('#').next().unwrap_or("").to_string(),
            String::new(),
        ),
    };
    Some(UrlParts {
        scheme,
        host,
        port,
        path: if path.is_empty() { "/".into() } else { path },
        query,
    })
}

/// 相对路径解析（`<base>` 语义的简化版）。
pub fn resolve_url(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.is_empty() {
        return base.to_string();
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if href.starts_with("//") {
        let scheme = parse_url(base).map(|u| u.scheme).unwrap_or("http".into());
        return format!("{scheme}:{href}");
    }
    let Some(b) = parse_url(base) else {
        return href.to_string();
    };
    let origin = b.origin();
    if href.starts_with('/') {
        return format!("{origin}{href}");
    }
    if href.starts_with('#') || href.starts_with("javascript:") || href.starts_with("mailto:") {
        return String::new();
    }
    if href.starts_with('?') {
        return format!("{origin}{}?{}", b.path, href.trim_start_matches('?'));
    }
    let dir = match b.path.rfind('/') {
        Some(i) => &b.path[..=i],
        None => "/",
    };
    format!("{origin}{dir}{href}")
}

/// `&amp;` 之类实体解码（常见几个 + 数字实体）。
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = s[i..].find(';') {
                if semi <= 10 {
                    let ent = &s[i + 1..i + semi];
                    let rep = match ent {
                        "amp" => Some("&"),
                        "lt" => Some("<"),
                        "gt" => Some(">"),
                        "quot" => Some("\""),
                        "apos" => Some("'"),
                        "nbsp" => Some(" "),
                        _ => {
                            if let Some(num) = ent.strip_prefix('#') {
                                let code = if let Some(hex) =
                                    num.strip_prefix('x').or_else(|| num.strip_prefix('X'))
                                {
                                    u32::from_str_radix(hex, 16).ok()
                                } else {
                                    num.parse::<u32>().ok()
                                };
                                if let Some(c) = code.and_then(char::from_u32) {
                                    out.push(c);
                                    i += semi + 1;
                                    continue;
                                }
                            }
                            None
                        }
                    };
                    if let Some(r) = rep {
                        out.push_str(r);
                        i += semi + 1;
                        continue;
                    }
                }
            }
        }
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 取 HTML 的可见文本（去标签、跳过 script/style、折叠空白）。
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !in_tag && lower[i..].starts_with("<script") {
            in_script = true;
        }
        if !in_tag && lower[i..].starts_with("</script") {
            in_script = false;
        }
        let c = bytes[i] as char;
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
            out.push(' ');
        } else if !in_tag && !in_script {
            out.push(c);
        }
        i += 1;
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut last_space = false;
    for ch in decode_entities(&out).chars() {
        if ch.is_whitespace() {
            if !last_space {
                collapsed.push(' ');
            }
            last_space = true;
        } else {
            collapsed.push(ch);
            last_space = false;
        }
    }
    collapsed.trim().to_string()
}

// ------------------------------------------------------- 手写扫描（替代正则）

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// 边界判定：粘连在字母数字或连字符上的十六进制串不算独立 UUID
/// （`x3f25...`、`0x3f25...` 这类是别的标识符的一部分）。
fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_'
}

/// 把 UUID 替换成 `[UUID]`（无正则）。
pub fn replace_uuid(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        // 8-4-4-4-12
        if i + 36 <= b.len()
            && b[i..i + 8].iter().all(|&c| is_hex(c))
            && b[i + 8] == b'-'
            && b[i + 9..i + 13].iter().all(|&c| is_hex(c))
            && b[i + 13] == b'-'
            && b[i + 14..i + 18].iter().all(|&c| is_hex(c))
            && b[i + 18] == b'-'
            && b[i + 19..i + 23].iter().all(|&c| is_hex(c))
            && b[i + 23] == b'-'
            && b[i + 24..i + 36].iter().all(|&c| is_hex(c))
            && (i == 0 || !is_word_char(b[i - 1]))
            && (i + 36 >= b.len() || !is_word_char(b[i + 36]))
        {
            out.push_str("[UUID]");
            i += 36;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn digits(b: &[u8], from: usize, n: usize) -> bool {
    from + n <= b.len() && b[from..from + n].iter().all(u8::is_ascii_digit)
}

/// 把 `2024-01-02T03:04:05[.fff][Z|±hh:mm]` 替换成 `[TIME]`（无正则）。
pub fn replace_iso_time(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let date_ok = digits(b, i, 4)
            && i + 10 <= b.len()
            && b[i + 4] == b'-'
            && digits(b, i + 5, 2)
            && b[i + 7] == b'-'
            && digits(b, i + 8, 2);
        if date_ok {
            let mut j = i + 10;
            let sep_ok = j < b.len() && (b[j] == b'T' || b[j] == b't' || b[j] == b' ');
            if sep_ok && digits(b, j + 1, 2) && b.get(j + 3) == Some(&b':') && digits(b, j + 4, 2) {
                j += 6;
                if b.get(j) == Some(&b':') && digits(b, j + 1, 2) {
                    j += 3;
                }
                // 小数秒
                if b.get(j) == Some(&b'.') {
                    let mut k = j + 1;
                    while k < b.len() && b[k].is_ascii_digit() {
                        k += 1;
                    }
                    if k > j + 1 {
                        j = k;
                    }
                }
                // 时区
                if matches!(b.get(j), Some(b'Z') | Some(b'z')) {
                    j += 1;
                } else if matches!(b.get(j), Some(b'+') | Some(b'-')) {
                    let mut k = j + 1;
                    if digits(b, k, 2) {
                        k += 2;
                        if b.get(k) == Some(&b':') {
                            k += 1;
                        }
                        if digits(b, k, 2) {
                            j = k + 2;
                        }
                    }
                }
                out.push_str("[TIME]");
                i = j;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 抽邮箱（无正则）。会过滤掉明显的误报（无 TLD 的 `a@b`）。
pub fn extract_emails(text: &str, max: usize) -> Vec<String> {
    let b = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    for (at, _) in text.match_indices('@') {
        if out.len() >= max {
            break;
        }
        let mut s = at;
        while s > 0 && (b[s - 1].is_ascii_alphanumeric() || b"._%+-".contains(&b[s - 1])) {
            s -= 1;
        }
        let mut e = at + 1;
        while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'.' || b[e] == b'-') {
            e += 1;
        }
        if e - at < 4 || s == at {
            continue;
        }
        let cand = &text[s..e];
        let Some((local, domain)) = cand.split_once('@') else {
            continue;
        };
        if local.is_empty() || local.starts_with('.') || local.ends_with('.') {
            continue;
        }
        let Some((_, tld)) = domain.rsplit_once('.') else {
            continue;
        };
        if tld.len() < 2 || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        if !domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            continue;
        }
        let v = cand.to_string();
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// 域名归一化：小写、去尾点、去通配前缀。
pub fn normalize_host(h: &str) -> String {
    h.trim()
        .trim_start_matches("*.")
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// 是否同一主域下（`a.example.com` 与 `example.com` 为真）。
pub fn same_site(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_host(a), normalize_host(b));
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

/// 地址是否落在内网/保留段（10/8、172.16/12、192.168/16、127/8、169.254/16、
/// 100.64/10、fc00::/7、::1、IPv4-mapped）。
///
/// 侦察类插件靠它把"内网资产"和"公网资产"分开——两者的话术与后续动作完全不同。
pub fn is_private_ip(ip: &str) -> bool {
    let h = normalize_host(ip);
    let h = h.trim_start_matches('[').trim_end_matches(']');
    // IPv4-mapped IPv6：::ffff:10.0.0.1
    if let Some(rest) = h.strip_prefix("::ffff:") {
        return is_private_ip(rest);
    }
    if h.contains(':') {
        let l = h.to_ascii_lowercase();
        return l == "::1"
            || l == "::"
            || l.starts_with("fc")
            || l.starts_with("fd")
            || l.starts_with("fe80");
    }
    let p: Vec<u16> = h.split('.').filter_map(|s| s.parse().ok()).collect();
    if p.len() != 4 {
        return false;
    }
    let (a, b) = (p[0], p[1]);
    a == 10
        || a == 127
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 169 && b == 254)
        || (a == 100 && (64..=127).contains(&b))
        || a == 0
}

/// 是否 IPv4 字面量（不含域名）。
pub fn is_ip_literal(s: &str) -> bool {
    let h = s.trim().trim_start_matches('[').trim_end_matches(']');
    if h.contains(':') {
        return true;
    }
    let p: Vec<&str> = h.split('.').collect();
    p.len() == 4 && p.iter().all(|x| !x.is_empty() && x.parse::<u8>().is_ok())
}

/// 网段归属：`"internal"` / `"public"` / `"unknown"`。
pub fn addr_scope(host: &str) -> &'static str {
    let h = normalize_host(host);
    if h.is_empty() {
        return "unknown";
    }
    if !is_ip_literal(&h) {
        // 域名：`.local` / 无点主机名 / localhost 视为内网
        if h == "localhost" || h.ends_with(".local") || h.ends_with(".lan") || !h.contains('.') {
            return "internal";
        }
        return "public";
    }
    if is_private_ip(&h) {
        "internal"
    } else {
        "public"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = parse_url("https://Example.com:8443/a/b?x=1#f").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, Some(8443));
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query, "x=1");
        assert_eq!(u.origin(), "https://example.com:8443");
    }

    #[test]
    fn url_ipv6_and_default_port() {
        let u = parse_url("http://[::1]:8080/x").unwrap();
        assert_eq!(u.host, "[::1]");
        assert_eq!(u.port, Some(8080));
        let d = parse_url("http://a.com/x").unwrap();
        assert_eq!(d.port, None);
        assert_eq!(d.origin(), "http://a.com");
    }

    #[test]
    fn resolve_relative() {
        assert_eq!(
            resolve_url("http://a.com/dir/page", "next.html"),
            "http://a.com/dir/next.html"
        );
        assert_eq!(
            resolve_url("http://a.com/dir/page", "/root"),
            "http://a.com/root"
        );
        assert_eq!(resolve_url("http://a.com/x", "#frag"), "");
        assert_eq!(
            resolve_url("https://a.com/x", "//cdn.b.com/a.js"),
            "https://cdn.b.com/a.js"
        );
    }

    #[test]
    fn entities_and_strip() {
        assert_eq!(decode_entities("a &amp; b &#65; &lt;x&gt;"), "a & b A <x>");
        assert_eq!(
            strip_tags("<p>hi <b>there</b></p><script>bad()</script>"),
            "hi there"
        );
    }

    #[test]
    fn uuid_and_time_replace() {
        let s = "id=3f2504e0-4f89-11d3-9a0c-0305e82c3301 t=2024-01-02T03:04:05.123Z";
        let r = replace_uuid(s);
        assert!(r.contains("[UUID]"), "{r}");
        let r = replace_iso_time(&r);
        assert!(r.contains("[TIME]"), "{r}");
        assert_eq!(replace_uuid("deadbeef"), "deadbeef");
        assert_eq!(
            replace_uuid("x3f2504e0-4f89-11d3-9a0c-0305e82c3301"),
            "x3f2504e0-4f89-11d3-9a0c-0305e82c3301",
            "前面粘着字母不算 UUID"
        );
    }

    #[test]
    fn email_extraction() {
        let v = extract_emails("a admin@example.com b x@y no-tld@host c", 10);
        assert_eq!(v, vec!["admin@example.com".to_string()]);
    }

    #[test]
    fn host_helpers() {
        assert!(same_site("a.example.com", "example.com"));
        assert!(!same_site("a.example.com", "b.example.org"));
        assert_eq!(normalize_host("*.Example.COM."), "example.com");
    }

    #[test]
    fn private_ranges() {
        for ip in ["10.1.2.3", "172.16.0.1", "172.31.255.254", "192.168.1.1", "127.0.0.1", "169.254.1.1", "100.64.0.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(is_private_ip(ip), "{ip} 应为内网");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "1.1.1.1", "2001:4860::1", "192.169.1.1"] {
            assert!(!is_private_ip(ip), "{ip} 应为公网");
        }
    }

    #[test]
    fn ip_literal_and_scope() {
        assert!(is_ip_literal("192.168.1.1"));
        assert!(is_ip_literal("[::1]"));
        assert!(!is_ip_literal("example.com"));
        assert!(!is_ip_literal("999.1.1.1"));
        assert_eq!(addr_scope("10.0.0.5"), "internal");
        assert_eq!(addr_scope("example.com"), "public");
        assert_eq!(addr_scope("intranet"), "internal");
        assert_eq!(addr_scope("printer.local"), "internal");
        assert_eq!(addr_scope(""), "unknown");
    }
}
