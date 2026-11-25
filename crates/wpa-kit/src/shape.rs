//! 响应形态分析与差分：盲注、LFI、软 404、越权判定的共用底座。
//!
//! 差分做两件事：
//! 1. **归一化**：把每次都变的片段（时间戳、随机 token、CSRF、会话 id）抹平；
//! 2. **多维度比较**：状态码 / 长度 / 关键词出现数 / token 相似度 / 耗时。

use serde_json::{json, Value};

/// 把"每次都变"的片段替换掉，剩下的才是可比较的骨架。
pub fn normalize_dynamic(body: &str) -> String {
    let mut s = body.to_string();
    // 长随机十六进制（CSRF / nonce / 会话 id）
    s = replace_runs(&s, |c| c.is_ascii_hexdigit(), 16, "[HEX]");
    // 纯数字串（时间戳、自增 id）≥ 6 位
    s = replace_runs(&s, |c| c.is_ascii_digit(), 6, "[NUM]");
    // UUID
    s = crate::textx::replace_uuid(&s);
    // ISO 时间
    s = crate::textx::replace_iso_time(&s);
    s
}

/// 把连续满足 `pred` 且长度 ≥ `min` 的片段替换成 `with`。
fn replace_runs(s: &str, pred: impl Fn(char) -> bool, min: usize, with: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    for c in s.chars() {
        if pred(c) {
            run.push(c);
        } else {
            if run.len() >= min {
                out.push_str(with);
            } else {
                out.push_str(&run);
            }
            run.clear();
            out.push(c);
        }
    }
    if run.len() >= min {
        out.push_str(with);
    } else {
        out.push_str(&run);
    }
    out
}

/// 分词（按非字母数字切），返回小写词集合。
pub fn tokens(s: &str) -> std::collections::BTreeSet<String> {
    let mut set = std::collections::BTreeSet::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        } else if !cur.is_empty() {
            set.insert(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        set.insert(cur);
    }
    set
}

/// Jaccard 相似度（0.0 ~ 1.0）。两边都空视为 1.0。
pub fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    let inter = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    if union == 0.0 {
        1.0
    } else {
        inter / union
    }
}

/// 香农熵（bit/字符）：用来判"随机串"（反射标记、密钥）。
pub fn entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = std::collections::BTreeMap::new();
    for b in s.bytes() {
        *counts.entry(b).or_insert(0usize) += 1;
    }
    let n = s.len() as f64;
    counts
        .values()
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// 一次比较的判定结果。
#[derive(Debug, Clone)]
pub struct Comparison {
    pub status_same: bool,
    pub len_delta: i64,
    /// 相对长度变化（0.0 ~ 1.0+）。
    pub len_ratio: f64,
    pub sim: f64,
    pub elapsed_delta_ms: i64,
    /// 关键词是否从"没有"变成"有"。
    pub keyword_appeared: bool,
}

impl Comparison {
    /// 是否"实质不同"：状态不同、长度变化超过 `len_ratio`、相似度低于 `sim`、或关键词新出现。
    pub fn materially_different(&self, len_ratio: f64, sim: f64) -> bool {
        !self.status_same || self.len_ratio >= len_ratio || self.sim < sim || self.keyword_appeared
    }

    pub fn to_json(&self) -> Value {
        json!({
            "status_same": self.status_same,
            "len_delta": self.len_delta,
            "len_ratio": (self.len_ratio * 1000.0).round() / 1000.0,
            "similarity": (self.sim * 1000.0).round() / 1000.0,
            "elapsed_delta_ms": self.elapsed_delta_ms,
            "keyword_appeared": self.keyword_appeared,
        })
    }
}

/// 观测值（来自一次 HTTP 响应）。
#[derive(Debug, Clone, Default)]
pub struct Observation {
    pub status: u16,
    pub body: String,
    pub elapsed_ms: u64,
}

impl Observation {
    pub fn new(status: u16, body: impl Into<String>, elapsed_ms: u64) -> Self {
        Self {
            status,
            body: body.into(),
            elapsed_ms,
        }
    }
}

/// 比较基线与探测结果。`keyword` 给出时额外判"关键词是否新出现"。
pub fn compare(base: &Observation, probe: &Observation, keyword: Option<&str>) -> Comparison {
    let (na, nb) = (normalize_dynamic(&base.body), normalize_dynamic(&probe.body));
    let len_delta = nb.len() as i64 - na.len() as i64;
    let len_ratio = if na.is_empty() {
        if nb.is_empty() { 0.0 } else { 1.0 }
    } else {
        (len_delta.unsigned_abs() as f64) / (na.len() as f64)
    };
    let kw_in_base = keyword
        .map(|k| !k.is_empty() && base.body.contains(k))
        .unwrap_or(false);
    let kw_in_probe = keyword
        .map(|k| !k.is_empty() && probe.body.contains(k))
        .unwrap_or(false);
    Comparison {
        status_same: base.status == probe.status,
        len_delta,
        len_ratio,
        sim: similarity(&na, &nb),
        elapsed_delta_ms: probe.elapsed_ms as i64 - base.elapsed_ms as i64,
        keyword_appeared: kw_in_probe && !kw_in_base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_kills_noise() {
        let a = normalize_dynamic("tok=deadbeefdeadbeef ts=1712345678");
        let b = normalize_dynamic("tok=1234abcd5678ef90 ts=1712345999");
        assert_eq!(a, b, "{a} vs {b}");
    }

    #[test]
    fn similarity_bounds() {
        assert_eq!(similarity("abc", "abc"), 1.0);
        assert!(similarity("hello world", "hello there") < 1.0);
        assert_eq!(similarity("", ""), 1.0);
        let s = similarity("a b c", "a b c");
        assert!(s > 0.99);
    }

    #[test]
    fn entropy_grows_with_randomness() {
        assert!(entropy("aaaaaaaa") < 0.1);
        assert!(entropy("a1b2c3d4e5f6") > 3.0);
    }

    #[test]
    fn compare_detects_keyword() {
        let base = Observation::new(200, "<html>not found</html>", 10);
        let probe = Observation::new(500, "<html>SQL syntax error near 'x'</html>", 12);
        let c = compare(&base, &probe, Some("SQL syntax error"));
        assert!(!c.status_same);
        assert!(c.keyword_appeared);
        assert!(c.materially_different(0.05, 0.95));
    }

    #[test]
    fn compare_identical_is_stable() {
        let a = Observation::new(200, "same body here", 100);
        let b = Observation::new(200, "same body here", 130);
        let c = compare(&a, &b, None);
        assert!(c.status_same);
        assert_eq!(c.len_delta, 0);
        assert!((c.sim - 1.0).abs() < 1e-9);
        assert!(!c.materially_different(0.05, 0.95));
    }
}
