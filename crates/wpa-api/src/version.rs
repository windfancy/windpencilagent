//! 极简语义化版本（不引入 semver crate：基座依赖要保持纯净）。
//!
//! 只实现清单校验真正需要的部分：`主.次.修订[-预发布]` 的解析与优先级比较。

use std::cmp::Ordering;
use std::fmt;

/// 语义化版本号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    /// 预发布标识（`1.0.0-beta.2` 的 `beta.2`）：有预发布者优先级低于同号正式版。
    pub pre: Option<String>,
}

impl SemVer {
    /// 解析版本串：容忍前导 `v`，容忍 `1.2`（缺省 patch=0）。
    pub fn parse(input: &str) -> Result<Self, String> {
        let raw = input.trim();
        let raw = raw.strip_prefix('v').unwrap_or(raw);
        if raw.is_empty() {
            return Err("版本号为空".into());
        }
        let (core, pre) = match raw.split_once('-') {
            Some((c, p)) if !p.trim().is_empty() => (c, Some(p.trim().to_string())),
            Some((c, _)) => (c, None),
            None => (raw, None),
        };
        let mut it = core.split('.');
        let major = parse_num(it.next(), "主版本")?;
        let minor = match it.next() {
            Some(s) => parse_num(Some(s), "次版本")?,
            None => 0,
        };
        let patch = match it.next() {
            Some(s) => parse_num(Some(s), "修订号")?,
            None => 0,
        };
        if it.next().is_some() {
            return Err(format!("版本号段数过多：{input}"));
        }
        Ok(Self { major, minor, patch, pre })
    }

    /// 是否满足最小版本要求（`self >= min`，预发布视为低于同号正式版）。
    pub fn satisfies_min(&self, min: &SemVer) -> bool {
        self.cmp(min) != Ordering::Less
    }
}

fn parse_num(s: Option<&str>, what: &str) -> Result<u32, String> {
    let s = s.ok_or_else(|| format!("缺少{what}"))?;
    s.parse::<u32>().map_err(|_| format!("{what}不是数字：{s}"))
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                // 有预发布 < 无预发布
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

impl serde::Serialize for SemVer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for SemVer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        SemVer::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_variants() {
        assert_eq!(SemVer::parse("1.2.3").unwrap().to_string(), "1.2.3");
        assert_eq!(SemVer::parse("v1.2").unwrap().to_string(), "1.2.0");
        assert_eq!(SemVer::parse(" 2 ").unwrap().to_string(), "2.0.0");
        assert!(SemVer::parse("").is_err());
        assert!(SemVer::parse("1.x.0").is_err());
        assert!(SemVer::parse("1.2.3.4").is_err());
    }

    #[test]
    fn ordering_and_min_gate() {
        let a = SemVer::parse("0.1.0").unwrap();
        let b = SemVer::parse("0.2.0").unwrap();
        assert!(a < b);
        assert!(b.satisfies_min(&a));
        assert!(!a.satisfies_min(&b));
        // 预发布低于同号正式版
        let pre = SemVer::parse("1.0.0-beta.2").unwrap();
        let rel = SemVer::parse("1.0.0").unwrap();
        assert!(pre < rel);
    }
}
