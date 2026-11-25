//! file 组测试插件：路径解析。
//!
//! 存在的意义不是"提供路径工具"，而是**验证插件机制链路**：
//! 扫描 PnC.toml → 进注册表（discovered）→ 安装 → 工具面出现 → 调用成功。
//!
//! 刻意做成纯计算（不碰磁盘）：插件铁律之一是"不许自己落盘"，
//! 而宿主侧的文件能力还没进 HostApi（见 docs/插件体系功能架构.md §4）。
//! 等文件能力追加到 HostApi 末尾后，这里可以直接换成真实读写。

use plugin_sdk::prelude::*;

fn run(args: &Value) -> Result<Reply, String> {
    let raw = args::req_str(args, "path")?;
    let normalized = raw.replace('\\', "/");
    // 盘符路径（D:/x）不是"以 / 开头"的绝对路径，拼目录时不能补前导斜杠
    let has_drive = normalized.len() >= 2 && normalized.as_bytes()[1] == b':';
    let is_abs = has_drive || normalized.starts_with('/') || normalized.starts_with("//");

    let segs: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    let name = segs.last().copied().unwrap_or("");
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    };
    let dir = if segs.len() <= 1 {
        String::new()
    } else {
        let head: Vec<&str> = segs[..segs.len() - 1].to_vec();
        let mut d = head.join("/");
        if is_abs && !has_drive && !d.starts_with('/') {
            d.insert(0, '/');
        }
        d
    };
    let trailing_sep = normalized.ends_with('/') || normalized.ends_with('\\');

    let summary = format!(
        "{}：目录 {} / 主名 {} / 扩展名 {} / 层级 {}",
        if name.is_empty() { "(空)" } else { name },
        if dir.is_empty() { "." } else { dir.as_str() },
        if stem.is_empty() { "-" } else { stem },
        if ext.is_empty() { "-" } else { ext },
        segs.len()
    );

    Ok(Reply::both(
        summary,
        json!({
            "input": raw,
            "normalized": normalized,
            "absolute": is_abs,
            "dir": dir,
            "name": name,
            "stem": stem,
            "ext": ext,
            "depth": segs.len(),
            "trailing_sep": trailing_sep,
        }),
    ))
}

declare_tool! {
    id: "file.path_info",
    group: "file",
    version: "0.1.0",
    risk: "low",
    permissions: [],
    summary: "把路径拆成目录/文件名/主名/扩展名/层级，并规范化分隔符（纯计算）",
    schema: r#"{
      "type": "object",
      "required": ["path"],
      "properties": {
        "path": { "type": "string", "description": "要分析的路径（Windows 或 POSIX 分隔符都行）" }
      }
    }"#,
    run: run,
}
