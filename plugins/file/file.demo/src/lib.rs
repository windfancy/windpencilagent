//! 演示工具
//!
//! 由 `wpa-cli new-plugin` 生成。改 `run()` 即可，导出符号由 `declare_tool!` 负责。

use plugin_sdk::prelude::*;

fn run(args: &Value) -> Result<Reply, String> {
    // 必填参数用 args::req_str（缺失即报错），可选参数给默认值
    let input = args::req_str(args, "input")?;
    Ok(Reply::both(
        format!("收到：{input}"),
        json!({ "input": input }),
    ))
}

declare_tool! {
    id: "file.demo",
    group: "file",
    version: "0.1.0",
    risk: "low",
    permissions: [],
    summary: "向导生成的可运行示例（验证 P0-8）",
    schema: r#"{"type":"object","required":["input"],"properties":{"input":{"type":"string","description":"输入"}}}"#,
    run: run,
}
