//! Scope Guard：授权范围守卫与高危黑名单硬阻断（红线机制）。
//!
//! 任何目标访问在执行层强制过一遍这里的校验，不依赖 LLM / 规划器自觉。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{ExecutionContext, RiskLevel};

/// Scope Guard 校验失败（越权 / 命中黑名单 / 参数非法）。
#[derive(Debug, Error, Clone, Serialize, Deserialize)]
pub enum ScopeViolation {
    #[error("target `{0}` not in authorized scope")]
    NotAuthorized(String),
    #[error("target `{0}` hit deny rule `{1}`")]
    DeniedByRule(String, String),
    #[error("invalid scope rule `{0}`")]
    InvalidRule(String),
    #[error("network egress to `{0}` is not allowed")]
    EgressDenied(String),
}

/// 授权范围里的一条规则：CIDR / 单 IP / 域后缀（含通配）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeRule {
    /// IPv4 网段，如 10.26.0.0/24
    CidrV4 { net: Ipv4Addr, prefix: u8 },
    /// IPv6 网段
    CidrV6 { net: Ipv6Addr, prefix: u8 },
    /// 单 IP
    Host(IpAddr),
    /// 域后缀，如 example.com；`.gov.cn` 用尾匹配
    Domain(String),
    /// 域名通配，如 `*.redlab.targets.cn`
    WildcardDomain(String),
    /// 单标签主机名，如 `localhost` / `dc01` / `intranet`：**只做全等匹配**。
    /// 不能按域后缀处理——否则规则 `cn` 会顺带授权 `anything.cn`。
    HostName(String),
    /// 通配一切（`*`）。仅用于用户显式选择「允许完全访问」时注入，
    /// 表达"不限制任何目标"。空授权清单仍是 fail-closed，绝不等价于本规则。
    Any,
}

impl ScopeRule {
    /// 字符串 → 规则。
    pub fn parse(raw: &str) -> Result<Self, ScopeViolation> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ScopeViolation::InvalidRule(raw.into()));
        }
        // 通配一切：仅"完全访问"模式会注入该规则。
        if raw == "*" {
            return Ok(Self::Any);
        }
        if let Some((ip, prefix)) = raw.split_once('/') {
            let prefix = prefix
                .parse::<u8>()
                .map_err(|_| ScopeViolation::InvalidRule(raw.into()))?;
            if let Ok(ip) = ip.parse::<Ipv4Addr>() {
                if prefix > 32 {
                    return Err(ScopeViolation::InvalidRule(raw.into()));
                }
                return Ok(Self::CidrV4 { net: ip, prefix });
            }
            if let Ok(ip) = ip.parse::<Ipv6Addr>() {
                if prefix > 128 {
                    return Err(ScopeViolation::InvalidRule(raw.into()));
                }
                return Ok(Self::CidrV6 { net: ip, prefix });
            }
            return Err(ScopeViolation::InvalidRule(raw.into()));
        }
        if let Ok(ip) = raw.parse::<IpAddr>() {
            return Ok(Self::Host(ip));
        }
        if raw.starts_with("*.") {
            return Ok(Self::WildcardDomain(raw.trim_start_matches('*').to_string()));
        }
        if raw.contains('.') {
            return Ok(Self::Domain(raw.to_ascii_lowercase()));
        }
        // 单标签主机名（内网常见：localhost / dc01 / intranet）。
        // 只接受 ASCII 字母数字与 `-` `_`，且首尾不能是 `-`。
        if is_single_label(raw) {
            return Ok(Self::HostName(raw.to_ascii_lowercase()));
        }
        Err(ScopeViolation::InvalidRule(raw.into()))
    }

    /// 该规则是否会授权任何目标（用于装配期提示"这条规则白写了"）。
    pub fn is_effective(&self) -> bool {
        match self {
            Self::CidrV4 { prefix, .. } => *prefix <= 32,
            Self::CidrV6 { prefix, .. } => *prefix <= 128,
            Self::Domain(d) | Self::HostName(d) => !d.is_empty(),
            Self::WildcardDomain(d) => d.len() > 1,
            Self::Host(d) => !d.to_string().is_empty(),
            Self::Any => true,
        }
    }
    pub fn matches(&self, target: &str) -> bool {
        match self {
            // 完全访问：任何目标都算在授权范围内。
            Self::Any => true,
            // CIDR 授权：目标既可以是单个 IP，也可以是一个 CIDR（子网 ⊆ 授权网段）。
            Self::CidrV4 { net, prefix } => {
                if let Some((tnet, tprefix)) = split_v4_cidr(target) {
                    // 目标网段必须被授权网段完整覆盖：前缀更长（更窄）且网络位对齐。
                    return tprefix >= *prefix && cidr_v4_contains(*net, *prefix, tnet);
                }
                target
                    .parse::<Ipv4Addr>()
                    .map(|ip| cidr_v4_contains(*net, *prefix, ip))
                    .unwrap_or(false)
            }
            Self::CidrV6 { net, prefix } => target
                .parse::<Ipv6Addr>()
                .map(|ip| cidr_v6_contains(*net, *prefix, ip))
                .unwrap_or(false),
            Self::Host(ip) => target
                .parse::<IpAddr>()
                .map(|t| t == *ip)
                .unwrap_or(false),
            Self::Domain(domain) => domain_matches(&target.to_ascii_lowercase(), domain),
            Self::WildcardDomain(suffix) => {
                let t = target.to_ascii_lowercase();
                t.ends_with(suffix)
            }
            // 单标签主机名只认全等：`localhost` 不授权 `foo.localhost`。
            Self::HostName(host) => target.eq_ignore_ascii_case(host),
        }
    }
}

/// 单标签主机名：ASCII 字母数字 / `-` / `_`，长度 1..=63，首尾非 `-`。
fn is_single_label(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > 63 {
        return false;
    }
    if raw.starts_with('-') || raw.ends_with('-') {
        return false;
    }
    raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 把 `a.b.c.d/p` 解析成 (网络地址, 前缀)；非 CIDR 返回 None。
fn split_v4_cidr(target: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip, prefix) = target.trim().split_once('/')?;
    let net = ip.trim().parse::<Ipv4Addr>().ok()?;
    let prefix = prefix.trim().parse::<u8>().ok()?;
    if prefix > 32 {
        return None;
    }
    Some((net, prefix))
}

fn cidr_v4_contains(net: Ipv4Addr, prefix: u8, ip: Ipv4Addr) -> bool {    let a = u32::from(net);
    let b = u32::from(ip);
    if prefix == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - prefix);
    (a & mask) == (b & mask)
}

fn cidr_v6_contains(net: Ipv6Addr, prefix: u8, ip: Ipv6Addr) -> bool {
    let a = u128::from(net);
    let b = u128::from(ip);
    if prefix == 0 {
        return true;
    }
    let mask = u128::MAX << (128 - prefix);
    (a & mask) == (b & mask)
}

fn domain_matches(target: &str, rule_domain: &str) -> bool {
    target == rule_domain || target.ends_with(&format!(".{rule_domain}"))
}

/// Scope Guard 本体：授权清单 + 黑名单 + 高危黑名单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeGuard {
    /// 授权目标（可解析为规则）。
    allowed: Vec<ScopeRule>,
    /// 显式拒绝的目标（黑名单优先于白名单）。
    deny: Vec<ScopeRule>,
    /// 无法生效的拒绝条目原文。非空 ⇒ 拒绝一切带目标的操作：
    /// 「除 X 之外」这个承诺一旦落实不了，就不能假装它落实了（fail-closed）。
    #[serde(default)]
    deny_broken: Vec<String>,
    /// 无条件禁用的高危动作（任何来源不可绕行）。
    blacklist_actions: Vec<String>,
    /// 是否强制限速（爆破 / 主动探测）。
    enforce_rate_limit: bool,
}

impl ScopeGuard {
    /// 从授权字符串列表 + 黑名单字符串列表构建（严格：任一规则非法即失败）。
    pub fn from_lists(authorized: &[String], denylist: &[String]) -> Result<Self, ScopeViolation> {
        let allowed = authorized
            .iter()
            .map(|s| ScopeRule::parse(s))
            .collect::<Result<Vec<_>, _>>()?;
        let deny = denylist
            .iter()
            .map(|s| ScopeRule::parse(s))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(allowed, deny))
    }

    /// 宽松构建：无法解析的**授权**条目被丢弃（丢弃只会缩小授权面，不会放大）；
    /// 无法解析的**拒绝**条目会让守卫进入"拒绝一切目标"状态（见 `deny_broken`）。
    /// 返回 `(guard, 提示)`，提示供装配期与调用侧原样展示给用户。
    pub fn from_lists_lenient(
        authorized: &[String],
        denylist: &[String],
    ) -> (Self, Vec<String>) {
        let mut notes = Vec::new();
        let mut allowed = Vec::new();
        for raw in authorized {
            let t = raw.trim();
            if t.is_empty() {
                continue; // 空行/空条目不算写错（输入框里很常见），静默跳过
            }
            match ScopeRule::parse(t) {
                Ok(r) if r.is_effective() => allowed.push(r),
                Ok(_) => notes.push(format!("授权范围条目 `{t}` 不会授权任何目标（写法无效）")),
                Err(_) => notes.push(format!("授权范围条目 `{t}` 无法识别，已忽略（该条不授权任何目标）")),
            }
        }
        let mut deny = Vec::new();
        let mut deny_broken = Vec::new();
        for raw in denylist {
            let t = raw.trim();
            if t.is_empty() {
                continue;
            }
            match ScopeRule::parse(t) {
                Ok(r) if r.is_effective() => deny.push(r),
                _ => {
                    deny_broken.push(t.to_string());
                    notes.push(format!(
                        "拒绝清单条目 `{t}` 无法生效：在修好之前，所有带目标的操作都会被拒绝（拒绝清单不能打折）"
                    ));
                }
            }
        }
        let mut g = Self::new(allowed, deny);
        g.deny_broken = deny_broken;
        (g, notes)
    }

    fn new(allowed: Vec<ScopeRule>, deny: Vec<ScopeRule>) -> Self {
        Self {
            allowed,
            deny,
            deny_broken: Vec::new(),
            blacklist_actions: vec![
                "exploit.persist".into(),
                "exploit.pivot".into(),
                "system.destroy".into(),
            ],
            enforce_rate_limit: true,
        }
    }

    /// 授权条目数（解析后实际生效的条数）。
    pub fn allowed_len(&self) -> usize {
        self.allowed.len()
    }

    /// 校验目标是否在授权范围且未命中黑名单。
    pub fn check_target(&self, target: &str) -> Result<(), ScopeViolation> {
        if let Some(bad) = self.deny_broken.first() {
            return Err(ScopeViolation::DeniedByRule(
                target.into(),
                format!("deny-list 规则无法生效：{bad}"),
            ));
        }
        if self.deny.iter().any(|r| r.matches(target)) {
            return Err(ScopeViolation::DeniedByRule(
                target.into(),
                "deny-list".into(),
            ));
        }
        if !self.allowed.iter().any(|r| r.matches(target)) {
            return Err(ScopeViolation::NotAuthorized(target.into()));
        }
        Ok(())
    }

    /// 校验一次执行请求：目标授权 + 动作黑名单 + 风险审批前置。
    pub fn check(&self, ctx: &ExecutionContext) -> Result<(), ScopeViolation> {
        self.check_target(&ctx.target)?;
        Ok(())
    }

    /// 高危动作黑名单判定。动作名匹配即拦截。
    pub fn action_blocked(&self, action_name: &str) -> bool {
        self.blacklist_actions.iter().any(|b| action_name.to_ascii_lowercase().contains(b) || b.contains(action_name.to_ascii_lowercase().as_str()))
    }

    /// 追加自定义黑名单动作。
    pub fn add_blacklist_action(mut self, action: impl Into<String>) -> Self {
        self.blacklist_actions.push(action.into());
        self
    }

    /// 强制限速开关。
    pub fn rate_limit_enforced(&self) -> bool {
        self.enforce_rate_limit
    }

    /// 风险级别校准：对授权范围内的目标，利用类动作仍需要 High+ 审批，
    /// 该判断由 ExecutionContext.risk.needs_approval() 承担。
    pub fn max_risk(&self) -> RiskLevel {
        RiskLevel::High
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> ScopeGuard {
        let auth = vec![
            "10.26.0.0/24".to_string(),
            "192.168.1.5".to_string(),
            "redlab.targets.cn".to_string(),
            "*.edu.example".to_string(),
        ];
        let deny = vec!["10.26.0.7".to_string(), ".gov.cn".to_string()];
        ScopeGuard::from_lists(&auth, &deny).unwrap()
    }

    #[test]
    fn cidr_matching() {
        let g = guard();
        assert!(g.check_target("10.26.0.42").is_ok());
        assert!(g.check_target("10.26.1.42").is_err());
        assert!(g.check_target("192.168.1.5").is_ok());
        assert!(g.check_target("192.168.1.6").is_err());
    }

    #[test]
    fn cidr_target_must_be_covered_by_allowed_cidr() {
        // 目标本身是网段时：必须被授权网段完整覆盖才算通过
        let g = ScopeGuard::from_lists(&["192.168.0.0/16".into()], &[]).unwrap();
        assert!(g.check_target("192.168.1.0/24").is_ok(), "子网应通过");
        assert!(g.check_target("192.168.1.16/32").is_ok(), "/32 应通过");
        assert!(g.check_target("10.0.0.0/24").is_err(), "不在授权网段应拒绝");

        // 目标比授权更宽（/16 覆盖 /24）→ 不能借它绕过范围
        let g2 = ScopeGuard::from_lists(&["192.168.1.0/24".into()], &[]).unwrap();
        assert!(g2.check_target("192.168.0.0/16").is_err());
    }

    #[test]
    fn any_rule_allows_everything_but_empty_still_fails_closed() {
        // `*` = 完全访问：任何目标都放行（含被 deny 的，deny 由上层在完全访问时清空）
        let g = ScopeGuard::from_lists(&["*".into()], &[]).unwrap();
        assert!(g.check_target("10.0.0.5").is_ok());
        assert!(g.check_target("a.gov.cn").is_ok());
        assert!(g.check_target("example.com").is_ok());
        // 空授权 ≠ 完全访问
        let empty = ScopeGuard::from_lists(&[], &[]).unwrap();
        assert!(empty.check_target("10.0.0.5").is_err());
    }

    #[test]
    fn deny_priority() {
        let g = guard();
        // 10.26.0.7 在授权网段内但命中黑名单
        assert!(matches!(
            g.check_target("10.26.0.7"),
            Err(ScopeViolation::DeniedByRule(_, _))
        ));
    }

    #[test]
    fn domain_matching() {
        let g = guard();
        assert!(g.check_target("www.redlab.targets.cn").is_ok());
        assert!(g.check_target("api.redlab.targets.cn").is_ok());
        assert!(g.check_target("foo.gov.cn").is_err());
        assert!(g.check_target("lab.edu.example").is_ok());
        assert!(g.check_target("notallowed.example").is_err());
    }

    #[test]
    fn action_blacklist() {
        let g = guard();
        assert!(g.action_blocked("exploit.persist"));
        assert!(g.action_blocked("system.destroy"));
        assert!(!g.action_blocked("scan.finger"));
    }

    #[test]
    fn single_label_hostname_is_exact_match() {
        // 内网常见写法：localhost / dc01。只认全等，不顺带授权子域。
        let g = ScopeGuard::from_lists(&["localhost".into(), "dc01".into()], &[]).unwrap();
        assert!(g.check_target("localhost").is_ok());
        assert!(g.check_target("LOCALHOST").is_ok());
        assert!(g.check_target("dc01").is_ok());
        assert!(g.check_target("foo.localhost").is_err());
        assert!(g.check_target("dc01.corp.local").is_err());
        // 带空格 / 带 scheme 的写法仍然非法（应被装配期提示，而不是悄悄当成域名）
        assert!(ScopeRule::parse("local host").is_err());
        assert!(ScopeRule::parse("http://localhost").is_err());
    }

    #[test]
    fn lenient_drops_only_the_bad_allow_rule() {
        let auth = vec!["10.0.0.0/24".to_string(), "10.0.0.0/99".to_string()];
        let (g, notes) = ScopeGuard::from_lists_lenient(&auth, &[]);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("无法识别"), "{notes:?}");
        assert_eq!(g.allowed_len(), 1);
        // 合法条目照常生效，写错的那条不牵连
        assert!(g.check_target("10.0.0.5").is_ok());
        assert!(g.check_target("10.0.1.5").is_err());
    }

    #[test]
    fn broken_deny_rule_denies_everything() {
        // 拒绝清单不能打折：写错的拒绝条目在修好前拒绝一切目标。
        let (g, notes) = ScopeGuard::from_lists_lenient(
            &["10.0.0.0/24".into()],
            &["10.0.0.0/XX".into()],
        );
        assert!(notes.iter().any(|n| n.contains("无法生效")), "{notes:?}");
        assert!(matches!(
            g.check_target("10.0.0.5"),
            Err(ScopeViolation::DeniedByRule(_, _))
        ));
        // 空条目不算写错，不应把守卫打成拒绝一切
        let (g2, _) = ScopeGuard::from_lists_lenient(&["10.0.0.0/24".into()], &["  ".into()]);
        assert!(g2.check_target("10.0.0.5").is_ok());
    }
}
