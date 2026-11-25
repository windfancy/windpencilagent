//! DNS 协议（自研）：不调用系统解析器、不依赖 `dig`/`nslookup`。
//!
//! 只做"够用且可审计"的部分：构造查询、解析 A/AAAA/CNAME/MX/TXT/NS/SOA。
//! 报文压缩指针（0xC0）必须处理——真实响应里几乎每条记录都用它，不处理会得到乱码。

use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

/// 记录类型码。
pub mod rtype {
    pub const A: u16 = 1;
    pub const NS: u16 = 2;
    pub const CNAME: u16 = 5;
    pub const SOA: u16 = 6;
    pub const PTR: u16 = 12;
    pub const MX: u16 = 15;
    pub const TXT: u16 = 16;
    pub const AAAA: u16 = 28;
    pub const ANY: u16 = 255;
}

/// 类型名 → 码（未知返回 `None`，由调用方拒绝）。
pub fn parse_type(s: &str) -> Option<u16> {
    match s.trim().to_ascii_uppercase().as_str() {
        "A" => Some(rtype::A),
        "NS" => Some(rtype::NS),
        "CNAME" => Some(rtype::CNAME),
        "SOA" => Some(rtype::SOA),
        "PTR" => Some(rtype::PTR),
        "MX" => Some(rtype::MX),
        "TXT" => Some(rtype::TXT),
        "AAAA" => Some(rtype::AAAA),
        "ANY" => Some(rtype::ANY),
        _ => None,
    }
}

pub fn type_name(t: u16) -> &'static str {
    match t {
        rtype::A => "A",
        rtype::NS => "NS",
        rtype::CNAME => "CNAME",
        rtype::SOA => "SOA",
        rtype::PTR => "PTR",
        rtype::MX => "MX",
        rtype::TXT => "TXT",
        rtype::AAAA => "AAAA",
        rtype::ANY => "ANY",
        _ => "OTHER",
    }
}

/// 一条资源记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub rtype: u16,
    pub ttl: u32,
    pub value: String,
}

/// 解析结果。
#[derive(Debug, Clone, Default)]
pub struct DnsResponse {
    pub answers: Vec<Record>,
    /// 响应码（0 = NOERROR）。
    pub rcode: u8,
    pub truncated: bool,
    pub rtt_ms: u64,
}

impl DnsResponse {
    pub fn rcode_text(&self) -> &'static str {
        match self.rcode {
            0 => "NOERROR",
            1 => "FORMERR",
            2 => "SERVFAIL",
            3 => "NXDOMAIN",
            4 => "NOTIMP",
            5 => "REFUSED",
            _ => "OTHER",
        }
    }
}

/// 构造查询报文（公开以便单测逐字节校验）。
pub fn build_query(id: u16, name: &str, qtype: u16) -> Result<Vec<u8>, String> {
    let name = name.trim().trim_end_matches('.');
    if name.is_empty() {
        return Err("域名为空".into());
    }
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&id.to_be_bytes());
    buf.extend_from_slice(&0x0100u16.to_be_bytes()); // RD=1
    buf.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    for label in name.split('.') {
        if label.is_empty() {
            return Err(format!("域名含空标签：{name}"));
        }
        if label.len() > 63 {
            return Err(format!("域名标签过长（>63）：{label}"));
        }
        buf.push(label.len() as u8);
        buf.extend_from_slice(label.as_bytes());
    }
    buf.push(0);
    buf.extend_from_slice(&qtype.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes()); // IN
    Ok(buf)
}

fn read_u16(buf: &[u8], off: usize) -> Result<u16, String> {
    let b = buf.get(off..off + 2).ok_or("报文截断（u16）")?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn read_u32(buf: &[u8], off: usize) -> Result<u32, String> {
    let b = buf.get(off..off + 4).ok_or("报文截断（u32）")?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// 读取（可能压缩的）域名，返回 (域名, 下一个位置)。
fn read_name(buf: &[u8], mut off: usize) -> Result<(String, usize), String> {
    let mut parts: Vec<String> = Vec::new();
    let mut jumped = false;
    let mut next = off;
    let mut hops = 0usize;
    loop {
        let len = *buf.get(off).ok_or("报文截断（name）")?;
        if len == 0 {
            off += 1;
            if !jumped {
                next = off;
            }
            break;
        }
        // 压缩指针：高两位 11
        if len & 0xC0 == 0xC0 {
            let b2 = *buf.get(off + 1).ok_or("报文截断（ptr）")?;
            let ptr = (((len & 0x3F) as usize) << 8) | b2 as usize;
            if !jumped {
                next = off + 2;
            }
            jumped = true;
            hops += 1;
            if hops > 64 {
                return Err("域名压缩指针成环".into());
            }
            off = ptr;
            continue;
        }
        let l = len as usize;
        let label = buf.get(off + 1..off + 1 + l).ok_or("报文截断（label）")?;
        parts.push(String::from_utf8_lossy(label).to_string());
        off += 1 + l;
    }
    Ok((parts.join("."), next))
}

/// 解析响应报文。
pub fn parse_response(buf: &[u8]) -> Result<DnsResponse, String> {
    if buf.len() < 12 {
        return Err("响应过短".into());
    }
    let flags = read_u16(buf, 2)?;
    let rcode = (flags & 0x000F) as u8;
    let truncated = flags & 0x0200 != 0;
    let qd = read_u16(buf, 4)? as usize;
    let an = read_u16(buf, 6)? as usize;

    let mut off = 12usize;
    for _ in 0..qd {
        let (_, next) = read_name(buf, off)?;
        off = next + 4; // QTYPE + QCLASS
        if off > buf.len() {
            return Err("问题段越界".into());
        }
    }

    let mut answers = Vec::with_capacity(an);
    for _ in 0..an {
        let (name, next) = read_name(buf, off)?;
        off = next;
        let rtype = read_u16(buf, off)?;
        let _class = read_u16(buf, off + 2)?;
        let ttl = read_u32(buf, off + 4)?;
        let rdlen = read_u16(buf, off + 8)? as usize;
        let rdata_off = off + 10;
        let rdata = buf.get(rdata_off..rdata_off + rdlen).ok_or("rdata 越界")?;
        let value = decode_rdata(buf, rdata, rdata_off, rtype);
        answers.push(Record { name, rtype, ttl, value });
        off = rdata_off + rdlen;
    }
    Ok(DnsResponse { answers, rcode, truncated, rtt_ms: 0 })
}

fn decode_rdata(buf: &[u8], rdata: &[u8], rdata_off: usize, rtype: u16) -> String {
    match rtype {
        rtype::A if rdata.len() == 4 => {
            Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]).to_string()
        }
        rtype::AAAA if rdata.len() == 16 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(rdata);
            Ipv6Addr::from(o).to_string()
        }
        rtype::CNAME | rtype::NS | rtype::PTR => read_name(buf, rdata_off)
            .map(|(n, _)| n)
            .unwrap_or_else(|_| hex(rdata)),
        rtype::MX if rdata.len() > 2 => {
            let pref = u16::from_be_bytes([rdata[0], rdata[1]]);
            let host = read_name(buf, rdata_off + 2)
                .map(|(n, _)| n)
                .unwrap_or_else(|_| "?".into());
            format!("{pref} {host}")
        }
        rtype::TXT => {
            // TXT 由若干 len+bytes 片段组成，拼接后加引号
            let mut out = String::new();
            let mut i = 0;
            while i < rdata.len() {
                let l = rdata[i] as usize;
                let seg = rdata.get(i + 1..i + 1 + l).unwrap_or(&[]);
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push('"');
                out.push_str(&String::from_utf8_lossy(seg));
                out.push('"');
                i += 1 + l;
            }
            out
        }
        rtype::SOA => {
            let mut off = rdata_off;
            let mname = read_name(buf, off).map(|(n, x)| { off = x; n }).unwrap_or_default();
            let rname = read_name(buf, off).map(|(n, _)| n).unwrap_or_default();
            format!("{mname} {rname}")
        }
        _ => hex(rdata),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 发一次 DNS 查询（UDP，超时重试一次）。
pub async fn query(
    server: &str,
    name: &str,
    qtype: u16,
    timeout: Duration,
) -> Result<DnsResponse, String> {
    // 归一成 `host:port`：裸 IP 补 53；已带端口或方括号 IPv6 原样用。
    let addr = if server.starts_with('[') {
        if server.contains("]:") {
            server.to_string()
        } else {
            format!("{server}:53")
        }
    } else if server.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{server}]:53")
    } else if server.contains(':') {
        server.to_string()
    } else {
        format!("{server}:53")
    };
    let sock = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| format!("绑定本地端口失败：{e}"))?;
    sock.connect(&addr).await.map_err(|e| format!("连接 DNS {addr} 失败：{e}"))?;

    let started = Instant::now();
    let mut last_err = String::new();
    for attempt in 0..2u16 {
        let id = (std::process::id() as u16).wrapping_mul(31).wrapping_add(attempt);
        let pkt = build_query(id, name, qtype)?;
        if let Err(e) = sock.send(&pkt).await {
            last_err = format!("发送失败：{e}");
            continue;
        }
        let mut buf = vec![0u8; 4096];
        match tokio::time::timeout(timeout, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                let mut resp = parse_response(&buf[..n])?;
                resp.rtt_ms = started.elapsed().as_millis() as u64;
                return Ok(resp);
            }
            Ok(Err(e)) => last_err = format!("接收失败：{e}"),
            Err(_) => last_err = format!("DNS 查询超时（{}ms）", timeout.as_millis()),
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_packet_layout() {
        let pkt = build_query(0x1234, "example.com", rtype::A).unwrap();
        assert_eq!(&pkt[0..2], &[0x12, 0x34]);
        assert_eq!(&pkt[2..4], &[0x01, 0x00]);
        assert_eq!(&pkt[4..6], &[0x00, 0x01]);
        // 7"example" 3"com" 0
        assert_eq!(pkt[12], 7);
        assert_eq!(&pkt[13..20], b"example");
        assert_eq!(pkt[20], 3);
        assert_eq!(&pkt[21..24], b"com");
        assert_eq!(pkt[24], 0);
        assert_eq!(&pkt[25..27], &[0x00, 0x01]);
        assert_eq!(&pkt[27..29], &[0x00, 0x01]);
    }

    #[test]
    fn rejects_bad_names() {
        assert!(build_query(1, "", rtype::A).is_err());
        assert!(build_query(1, "a..b", rtype::A).is_err());
        let long = "a".repeat(64);
        assert!(build_query(1, &long, rtype::A).is_err());
    }

    /// 手工拼一个响应：1 问题 + 1 A 记录，name 用压缩指针。
    fn sample_response() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0x1234u16.to_be_bytes());
        b.extend_from_slice(&0x8180u16.to_be_bytes()); // QR=1 RD RA NOERROR
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        // question: example.com A IN
        for l in ["example", "com"] {
            b.push(l.len() as u8);
            b.extend_from_slice(l.as_bytes());
        }
        b.push(0);
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        // answer: ptr -> 12, A, IN, ttl 300, rdlen 4, 93.184.216.34
        b.extend_from_slice(&[0xC0, 0x0C]);
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&300u32.to_be_bytes());
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(&[93, 184, 216, 34]);
        b
    }

    #[test]
    fn parses_answer_with_compression() {
        let r = parse_response(&sample_response()).unwrap();
        assert_eq!(r.rcode, 0);
        assert_eq!(r.answers.len(), 1);
        assert_eq!(r.answers[0].name, "example.com");
        assert_eq!(r.answers[0].rtype, rtype::A);
        assert_eq!(r.answers[0].value, "93.184.216.34");
        assert_eq!(r.answers[0].ttl, 300);
    }

    #[test]
    fn parses_nxdomain_rcode() {
        let mut pkt = sample_response();
        pkt[3] = 0x83; // rcode = 3
        let r = parse_response(&pkt).unwrap();
        assert_eq!(r.rcode, 3);
        assert_eq!(r.rcode_text(), "NXDOMAIN");
    }

    #[test]
    fn truncated_and_short_packets() {
        assert!(parse_response(&[0u8; 4]).is_err());
        let mut pkt = sample_response();
        pkt[2] |= 0x02; // TC
        assert!(parse_response(&pkt).unwrap().truncated);
    }

    #[test]
    fn txt_and_mx_decode() {
        // TXT: 两段
        let rdata = {
            let mut v = Vec::new();
            for s in ["v=spf1", "-all"] {
                v.push(s.len() as u8);
                v.extend_from_slice(s.as_bytes());
            }
            v
        };
        let v = decode_rdata(&rdata, &rdata, 0, rtype::TXT);
        assert!(v.contains("v=spf1"));
        assert!(v.contains("all"));

        // MX: pref + 压缩名
        let mut pkt = sample_response();
        let qstart = 12;
        let rdata_off = pkt.len();
        pkt.extend_from_slice(&10u16.to_be_bytes());
        pkt.extend_from_slice(&[0xC0, qstart as u8]);
        let rd = &pkt[rdata_off..];
        let v = decode_rdata(&pkt, rd, rdata_off, rtype::MX);
        assert_eq!(v, "10 example.com");
    }

    #[test]
    fn type_roundtrip() {
        for t in ["A", "AAAA", "CNAME", "MX", "TXT", "NS", "SOA"] {
            let code = parse_type(t).unwrap();
            assert_eq!(type_name(code), t);
        }
        assert!(parse_type("BOGUS").is_none());
    }

    #[tokio::test]
    #[ignore = "需要外网 DNS"]
    async fn live_query() {
        let r = query("223.5.5.5", "example.com", rtype::A, Duration::from_secs(3))
            .await
            .expect("查询应成功");
        assert!(r.answers.iter().any(|a| a.rtype == rtype::A));
    }
}
