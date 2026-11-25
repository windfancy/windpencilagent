//! TCP 探测（自研）：端口解析 / connect 扫描 / banner 抓取。
//!
//! 不用 nmap，也不用 ICMP（后者要管理员权限）：connect 扫描对"目标是否可达"已经够用，
//! 且天然受目标主机日志记录——这是渗透测试要的可审计性。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

/// 单次扫描端口上限（防误操作打满全端口×多目标）。
pub const MAX_PORTS: usize = 16384;
/// 并发上限。
pub const MAX_CONCURRENCY: usize = 512;

/// 解析端口表达式：`80`、`80,443`、`1-1024`、`22,8000-8100`。
///
/// 容忍空格与重复（去重后升序），任何非法片段整体拒绝（不静默丢弃）。
pub fn parse_ports(spec: &str) -> Result<Vec<u16>, String> {
    let mut out: Vec<u16> = Vec::new();
    let s = spec.trim();
    if s.is_empty() {
        return Err("端口表达式为空".into());
    }
    for part in s.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Some((a, b)) = p.split_once('-') {
            let start: u16 = a.trim().parse().map_err(|_| format!("端口范围起点非法：{a}"))?;
            let end: u16 = b.trim().parse().map_err(|_| format!("端口范围终点非法：{b}"))?;
            if start == 0 || end == 0 {
                return Err("端口 0 不可用".into());
            }
            if start > end {
                return Err(format!("端口范围倒置：{p}"));
            }
            for port in start..=end {
                out.push(port);
                if out.len() > MAX_PORTS {
                    return Err(format!("端口数量超过上限 {MAX_PORTS}"));
                }
            }
        } else {
            let port: u16 = p.parse().map_err(|_| format!("端口非法：{p}"))?;
            if port == 0 {
                return Err("端口 0 不可用".into());
            }
            out.push(port);
        }
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        return Err("端口表达式未解析出任何端口".into());
    }
    Ok(out)
}

/// 常用端口（未指定时用）。
pub const COMMON_PORTS: &[u16] = &[
    21, 22, 23, 25, 53, 80, 110, 111, 135, 139, 143, 161, 389, 443, 445, 873, 993, 995, 1080,
    1433, 1521, 2049, 2375, 3000, 3306, 3389, 5432, 5900, 6379, 7001, 8000, 8080, 8081, 8443,
    8888, 9000, 9090, 9200, 11211, 27017,
];

/// 一个开放端口。
#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenPort {
    pub port: u16,
    /// 服务名（按端口号的常见映射；仅提示，不作为结论）。
    pub service: String,
    /// 抓到的 banner（首行，截断）。
    pub banner: Option<String>,
    /// connect 耗时（毫秒）。
    pub connect_ms: u64,
}

/// 扫描结果。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScanResult {
    pub host: String,
    pub resolved: Vec<String>,
    pub open: Vec<OpenPort>,
    pub scanned: usize,
    pub filtered: usize,
    pub duration_ms: u64,
}

/// 端口 → 服务名（常见端口）。
pub fn service_name(port: u16) -> &'static str {
    match port {
        21 => "ftp",
        22 => "ssh",
        23 => "telnet",
        25 | 465 | 587 => "smtp",
        53 => "dns",
        80 | 8000 | 8080 | 8081 | 8888 | 3000 => "http",
        110 | 995 => "pop3",
        111 => "rpcbind",
        135 => "msrpc",
        139 => "netbios-ssn",
        143 | 993 => "imap",
        161 | 162 => "snmp",
        389 | 636 => "ldap",
        443 | 8443 => "https",
        445 => "microsoft-ds",
        873 => "rsync",
        1080 => "socks",
        1433 => "mssql",
        1521 => "oracle",
        2049 => "nfs",
        2375 | 2376 => "docker",
        3306 => "mysql",
        3389 => "rdp",
        5432 => "postgresql",
        5900 => "vnc",
        6379 => "redis",
        7001 => "weblogic",
        9000 => "http-alt",
        9090 => "http-alt",
        9200 => "elasticsearch",
        11211 => "memcached",
        27017 => "mongodb",
        _ => "",
    }
}

/// 解析目标为 socket 地址列表（IP 直连；域名走系统解析器，仅此一处）。
pub async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    // 去掉可能的 scheme / 路径 / 端口，只留 host
    let mut h = host.trim();
    if let Some((_, rest)) = h.split_once("://") {
        h = rest;
    }
    let h = h.split('/').next().unwrap_or(h);
    let h = if h.starts_with('[') {
        // IPv6 字面量 [::1]:80
        h.split(']').next().unwrap_or(h).trim_start_matches('[')
    } else if h.matches(':').count() == 1 {
        h.split(':').next().unwrap_or(h)
    } else {
        h
    };
    let target = format!("{h}:{port}");
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(target.as_str())
        .await
        .map_err(|e| format!("解析目标失败 {h}：{e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("目标无法解析：{h}"));
    }
    Ok(addrs)
}

/// 探测单端口：`Ok(Some(banner))` = 开放；`Ok(None)` = 关闭/拒绝。
pub async fn probe(host: &str, port: u16, timeout: Duration, want_banner: bool) -> Result<Option<(Option<String>, u64)>, String> {
    let addrs = resolve(host, port).await?;
    let mut last_err = None;
    for addr in addrs {
        let started = Instant::now();
        match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
            Ok(Ok(mut stream)) => {
                let ms = started.elapsed().as_millis() as u64;
                let banner = if want_banner {
                    read_banner(&mut stream, Duration::from_millis(400)).await
                } else {
                    None
                };
                return Ok(Some((banner, ms)));
            }
            Ok(Err(e)) => last_err = Some(e.to_string()),
            Err(_) => last_err = Some("连接超时".into()),
        }
    }
    let _ = last_err;
    Ok(None)
}

async fn read_banner(stream: &mut TcpStream, wait: Duration) -> Option<String> {
    let mut buf = vec![0u8; 256];
    match tokio::time::timeout(wait, stream.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            let text = String::from_utf8_lossy(&buf[..n]);
            let first = text.lines().next().unwrap_or("").trim();
            let cleaned: String = first
                .chars()
                .filter(|c| !c.is_control())
                .take(120)
                .collect();
            if cleaned.is_empty() { None } else { Some(cleaned) }
        }
        _ => None,
    }
}

/// 并发 connect 扫描。
pub async fn scan(
    host: &str,
    ports: &[u16],
    timeout: Duration,
    concurrency: usize,
    want_banner: bool,
) -> Result<ScanResult, String> {
    if ports.is_empty() {
        return Err("端口列表为空".into());
    }
    let resolved = resolve(host, ports[0]).await?;
    let started = Instant::now();
    let sem = std::sync::Arc::new(Semaphore::new(concurrency.clamp(1, MAX_CONCURRENCY)));
    let mut tasks = Vec::with_capacity(ports.len());
    for &port in ports {
        let sem = sem.clone();
        let h = host.to_string();
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.ok()?;
            let r = probe(&h, port, timeout, want_banner).await;
            Some((port, r))
        }));
    }
    let mut open = Vec::new();
    let mut filtered = 0usize;
    for t in tasks {
        if let Ok(Some((port, Ok(Some((banner, ms)))))) = t.await {
            open.push(OpenPort {
                port,
                service: service_name(port).to_string(),
                banner,
                connect_ms: ms,
            });
        } else {
            filtered += 1;
        }
    }
    open.sort_by_key(|p| p.port);
    Ok(ScanResult {
        host: host.to_string(),
        resolved: resolved.iter().map(|a| a.ip().to_string()).collect(),
        open,
        scanned: ports.len(),
        filtered,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_specs() {
        assert_eq!(parse_ports("80").unwrap(), vec![80]);
        assert_eq!(parse_ports("443,80,80").unwrap(), vec![80, 443]);
        assert_eq!(parse_ports("1-3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_ports(" 22 , 8000-8002 ").unwrap(), vec![22, 8000, 8001, 8002]);
    }

    #[test]
    fn rejects_bad_port_specs() {
        assert!(parse_ports("").is_err());
        assert!(parse_ports("0").is_err());
        assert!(parse_ports("90-80").is_err());
        assert!(parse_ports("abc").is_err());
        assert!(parse_ports("99999").is_err());
        assert!(parse_ports("1-60000").is_err(), "超上限应拒绝");
    }

    #[test]
    fn service_names() {
        assert_eq!(service_name(445), "microsoft-ds");
        assert_eq!(service_name(6379), "redis");
        assert_eq!(service_name(65000), "");
    }

    #[tokio::test]
    async fn scan_localhost_closed_port() {
        // 127.0.0.1 上大概率没有 1 号端口；不依赖外部网络。
        let r = scan("127.0.0.1", &[1], Duration::from_millis(300), 4, false)
            .await
            .expect("扫描应能执行");
        assert_eq!(r.scanned, 1);
        assert!(r.open.is_empty());
        assert_eq!(r.resolved, vec!["127.0.0.1"]);
    }

    #[tokio::test]
    async fn scan_open_port_on_local_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut s, _)) = listener.accept().await {
                    use tokio::io::AsyncWriteExt;
                    let _ = s.write_all(b"SSH-2.0-TestServer\r\n").await;
                }
            }
        });
        let r = scan("127.0.0.1", &[port], Duration::from_secs(2), 4, true)
            .await
            .unwrap();
        assert_eq!(r.open.len(), 1, "本机监听端口应判定为开放");
        assert_eq!(r.open[0].banner.as_deref(), Some("SSH-2.0-TestServer"));
    }
}
