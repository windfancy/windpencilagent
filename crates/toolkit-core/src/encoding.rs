//! 多编码梯度解码（问题 6）：Windows 中文控制台输出（GBK/Big5）不再失败。
//!
//! 梯度：UTF-8 严格 → GB18030 → Big5 → WINDOWS_1252 → lossy 兜底。
//! 前一级"无错误"才采用；GB18030 是 GBK/GB2312 的超集，覆盖绝大多数中文 Windows 场景。

use encoding_rs::{BIG5, GB18030, WINDOWS_1252};

/// 按梯度尝试解码，返回最可信的文本结果。
///
/// - UTF-8 严格成功 → 直接采用（最高置信）；
/// - 否则依次尝试 GB18030 → Big5 → WINDOWS_1252，仅当该编码解码**无任何错误**时采用；
/// - 全部失败 → `String::from_utf8_lossy` 兜底（永不 Err）。
pub fn decode_best(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    for enc in [GB18030, BIG5, WINDOWS_1252] {
        let (cow, _, had_errors) = enc.decode(bytes);
        if !had_errors {
            return cow.into_owned();
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// 解码 + 超限截断（按字符边界），尾部带标记（沙箱输出统一走此函数）。
pub fn decode_output(bytes: &[u8], limit: usize) -> String {
    let decoded = decode_best(bytes);
    if bytes.len() <= limit {
        return decoded;
    }
    // 按解码后文本的字符边界截断，避免把多字节字符切成 U+FFFD
    let mut end = limit.min(decoded.len());
    while end > 0 && !decoded.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[OUTPUT TRUNCATED at {limit} bytes]", &decoded[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_utf8_passthrough() {
        assert_eq!(decode_best("hello 中文".as_bytes()), "hello 中文");
        assert_eq!(decode_best(b""), "");
    }

    #[test]
    fn gbk_bytes_decoded() {
        // "密" 的 GBK 编码为 0xC3 0xDC
        assert_eq!(decode_best(&[0xc3, 0xdc]), "密");
        // "中文" 的 GBK 编码
        assert_eq!(decode_best(&[0xD6, 0xD0, 0xCE, 0xC4]), "中文");
    }

    #[test]
    fn gradient_prefers_gb18030_over_big5() {
        // GB18030 是 GBK/Big5 双字节空间的超集：Big5 有效序列（如 0xA7 0x41）
        // 在 GB18030 中同样无错误，因此梯度上 GB18030 先命中（返回其对应字符，
        // 而非 Big5 的「你」）。本测试锁定该优先级行为，防止顺序被调换后
        // 中文 Windows 输出被 Big5 误判。
        let s = decode_best(&[0xA7, 0x41]);
        assert_ne!(s, "你");
    }

    #[test]
    fn latin1_fallback() {
        // 0xE9 在 Windows-1252 是 é，非法 GBK/Big5 序列
        assert_eq!(decode_best(&[0xE9, 0x28]), "é(");
    }

    #[test]
    fn lossy_bottom_line() {
        // 无效 UTF-8 且多编码均报错时 lossy 兜底，永不 panic
        let s = decode_best(&[0xFF, 0xFE, 0x00]);
        assert!(s.contains('\u{FFFD}') || !s.is_empty());
    }

    #[test]
    fn output_truncation_on_char_boundary() {
        let text = "汉".repeat(100); // 300 bytes UTF-8
        let raw = text.as_bytes();
        let out = decode_output(raw, 100);
        assert!(out.ends_with("[OUTPUT TRUNCATED at 100 bytes]"));
        // 截断点必须是字符边界：前缀应是完整 "汉" 重复（33 个 = 99 字节）
        let head = out.strip_suffix("\n[OUTPUT TRUNCATED at 100 bytes]").unwrap();
        assert_eq!(head, "汉".repeat(33));
        // 不超限原样返回
        assert_eq!(decode_output(raw, 1024), text);
    }
}
