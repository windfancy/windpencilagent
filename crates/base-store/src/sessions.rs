//! 会话持久化：对话流（用户/智能体/轨迹）跨重启保留，支持新建、切换、删除。
//!
//! 一条会话 = sled domain `sess` 下的一条记录；数据根随活跃工作空间分库，
//! 与经验库/任务库一致（exe 同级 data/ 或 workspaces/<name>/data/）。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// 落盘的一条对话行：role 0=用户 1=智能体 2=轨迹 3=提示 4=轮次分隔 5=工具结果（协议侧）。
///
/// R1-3 v2：新增 `tool_calls`（JSON 序列化的 Vec<ToolCall>）与 `tool_call_id`，
/// 旧记录缺字段时 serde default 兜底为 None——向后兼容。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredMessage {
    pub role: u8,
    pub text: String,
    /// assistant 消息携带的工具调用（JSON 数组），重启后可完整重建协议上下文。
    #[serde(default)]
    pub tool_calls: Option<String>,
    /// tool 角色消息对应的调用 id。
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl StoredMessage {
    /// 纯文本行（旧格式兼容构造）。
    pub fn plain(role: u8, text: impl Into<String>) -> Self {
        Self { role, text: text.into(), tool_calls: None, tool_call_id: None }
    }
}

/// R1-3 v2：会话的「结构化状态」——与展示行分离，由 worker 维护。
///
/// dsh 纪律「模型可见即已记录」：完整协议消息（含 tool_calls/tool 结果）原样落盘；
/// 「压缩只遮蔽不删除」：被压缩淘汰的原始消息进 archived，可回溯审计。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionState {
    /// 完整协议消息流（system 不落盘，每轮由 build_context 重建）。
    pub messages: Vec<StoredMessage>,
    /// 压缩后的滚动摘要（None=未压缩过）。
    pub rolling_summary: Option<String>,
    /// 压缩淘汰的原始消息归档（可逆压缩的关键）。
    pub archived: Vec<StoredMessage>,
}

/// 会话记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRecord {
    pub id: String,
    /// 标题：取首条用户输入前 24 字
    pub title: String,
    /// 创建时的工作空间名（空=全局）
    pub workspace: String,
    /// 创建时的模型 profile 名（空=未配置）
    pub profile: String,
    /// 创建时的工作区根（空间 cwd；空=未绑定）。仅作归属记录：
    /// 执行期仍以当前装配的工作区为准，避免打开旧会话时跨目录写入。
    #[serde(default)]
    pub cwd: String,
    pub turns: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub messages: Vec<StoredMessage>,
    /// R1-3 v2：结构化状态（None=旧记录或从未由 worker 写入）。
    #[serde(default)]
    pub state: Option<SessionState>,
}

impl SessionRecord {
    /// 标题摘要：首行前 24 字，空输入回退为「未命名会话」。
    pub fn title_from(input: &str) -> String {
        let first = input.lines().next().unwrap_or("").trim();
        let head: String = first.chars().take(24).collect();
        if head.is_empty() {
            "未命名会话".to_string()
        } else if first.chars().count() > 24 {
            format!("{head}…")
        } else {
            head
        }
    }

    /// UI 摘要文本：`N 轮 · K tok · 空间/模型`。
    pub fn summary(&self) -> String {
        let tokens = self.prompt_tokens + self.completion_tokens;
        format!(
            "{} 轮 · {tokens} tok · {}{}",
            self.turns,
            if self.workspace.is_empty() { "全局" } else { &self.workspace },
            if self.profile.is_empty() { String::new() } else { format!("/{}", self.profile) }
        )
    }
}

/// 会话库句柄（sled）。
pub struct SessionStore {
    db: Store,
}

impl SessionStore {
    pub fn open(dir: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        Ok(Self { db: Store::open(dir)? })
    }

    pub fn open_tmp() -> Result<Self, StoreError> {
        Ok(Self { db: Store::open_tmp()? })
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// 新建会话；id = 毫秒时间戳 + 进程内短序号（同秒多会话不冲突）。
    pub fn create(
        &self,
        title: &str,
        workspace: &str,
        profile: &str,
    ) -> Result<SessionRecord, StoreError> {
        self.create_in(title, workspace, profile, "")
    }

    /// 新建会话并记录工作区根（空间 cwd）。
    pub fn create_in(
        &self,
        title: &str,
        workspace: &str,
        profile: &str,
        cwd: &str,
    ) -> Result<SessionRecord, StoreError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let now = Self::now_ms();
        let rec = SessionRecord {
            id: format!("{now:013}-{seq:04x}"),
            title: title.to_string(),
            workspace: workspace.to_string(),
            profile: profile.to_string(),
            cwd: cwd.to_string(),
            turns: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            created_ms: now,
            updated_ms: now,
            messages: Vec::new(),
            state: None,
        };
        self.db.put("sess", &rec.id, &rec)?;
        Ok(rec)
    }

    pub fn get(&self, id: &str) -> Option<SessionRecord> {
        self.db.get("sess", id).ok()
    }

    /// 全部会话，按更新时间倒序（最近使用在前）。
    pub fn all(&self) -> Vec<SessionRecord> {
        let mut v: Vec<SessionRecord> = self
            .db
            .keys("sess")
            .iter()
            .filter_map(|k| self.db.get("sess", k).ok())
            .collect();
        v.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then_with(|| b.id.cmp(&a.id)));
        v
    }

    /// 新增/更新一条会话（外部先 get→改字段→save）。
    pub fn save(&self, rec: &SessionRecord) -> Result<(), StoreError> {
        self.db.put("sess", &rec.id, rec)
    }

    /// R5-2（dsh S7 会话检索）：关键词全文检索——标题 + 消息正文 + 滚动摘要 +
    /// 归档，大小写不敏感包含匹配（会话量级数百，全扫足够；免 FTS 索引依赖）。
    /// 返回按更新时间倒序的命中会话与命中片段（每会话最多 3 条、每片段 ≤120 字）。
    pub fn search(&self, query: &str, limit: usize) -> Vec<(SessionRecord, Vec<String>)> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(SessionRecord, Vec<String>)> = Vec::new();
        for rec in self.all() {
            let mut snippets: Vec<String> = Vec::new();
            // 标题命中
            if rec.title.to_lowercase().contains(&q) {
                snippets.push(format!("[标题] {}", rec.title));
            }
            // 正文 + 归档命中
            let mut scan = |m: &StoredMessage, src: &str| {
                if snippets.len() >= 3 {
                    return;
                }
                let lower = m.text.to_lowercase();
                if let Some(pos) = lower.find(&q) {
                    let start = pos.saturating_sub(40);
                    let snippet: String = m.text.chars().collect::<Vec<_>>()
                        [start..(start + 120).min(m.text.chars().count())]
                        .iter().collect();
                    let role_tag = match m.role {
                        0 => "用户",
                        1 => "智能体",
                        _ => "对话",
                    };
                    snippets.push(format!("[{src}·{role_tag}] …{snippet}…"));
                }
            };
            for m in &rec.messages {
                scan(m, "正文");
            }
            if let Some(state) = &rec.state {
                for m in &state.messages {
                    scan(m, "正文");
                }
                for m in &state.archived {
                    scan(m, "归档");
                }
                if let Some(s) = &state.rolling_summary {
                    if snippets.len() < 3 && s.to_lowercase().contains(&q) {
                        snippets.push(format!("[摘要] {}", s.chars().take(120).collect::<String>()));
                    }
                }
            }
            if !snippets.is_empty() {
                hits.push((rec, snippets));
            }
            if hits.len() >= limit {
                break;
            }
        }
        hits
    }

    /// 追加对话行并刷新时间戳（省去 UI 侧全量回写）。
    pub fn append_messages(
        &self,
        id: &str,
        messages: &[StoredMessage],
    ) -> Result<(), StoreError> {
        let mut rec = self.get(id).ok_or_else(|| StoreError::KeyNotFound(id.to_string()))?;
        rec.messages.extend_from_slice(messages);
        rec.updated_ms = Self::now_ms();
        self.save(&rec)
    }

    /// 写入结构化状态（R1-3 v2）：worker 每轮结束后调用。
    ///
    /// 读-改-写只覆盖 `state` 字段，展示行/messages 统计等其他字段保持落盘最新值，
    /// 与 UI 侧覆盖写互不踩踏（非并发同时写）。
    pub fn save_state(&self, id: &str, state: SessionState) -> Result<(), StoreError> {
        let mut rec = self.get(id).ok_or_else(|| StoreError::KeyNotFound(id.to_string()))?;
        rec.state = Some(state);
        self.save(&rec)
    }

    /// 更新会话统计（轮次/token/模型），刷新时间戳。
    pub fn update_stats(
        &self,
        id: &str,
        turns: Option<u64>,
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        profile: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut rec = self.get(id).ok_or_else(|| StoreError::KeyNotFound(id.to_string()))?;
        if let Some(n) = turns {
            rec.turns = n;
        }
        if let Some(n) = prompt_tokens {
            rec.prompt_tokens = n;
        }
        if let Some(n) = completion_tokens {
            rec.completion_tokens = n;
        }
        if let Some(p) = profile {
            rec.profile = p.to_string();
        }
        rec.updated_ms = Self::now_ms();
        self.save(&rec)
    }

    pub fn delete(&self, id: &str) -> Result<bool, StoreError> {
        self.db.remove("sess", id)
    }

    pub fn flush(&self) -> Result<(), StoreError> {
        self.db.flush()
    }

    /// 按空间过滤会话（按更新时间倒序）；workspace 为空时返回全部。
    pub fn list_by_workspace(&self, workspace: &str) -> Vec<SessionRecord> {
        let all = self.all();
        if workspace.is_empty() {
            return all;
        }
        all.into_iter().filter(|r| r.workspace == workspace).collect()
    }

    /// 导出会话头 + 全部消息为 JSONL：每行一个 `{"session":…, "message":…}` 对象。
    ///
    /// 首行是会话头（`session` = 去掉 messages 的会话字段，`message` = null），
    /// 之后每行一条消息（`session` = 会话 id）。目录不存在则创建，原子写。
    pub fn export_jsonl(&self, id: &str, path: &Path) -> Result<(), String> {
        let rec = self.get(id).ok_or_else(|| format!("会话不存在：{id}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建导出目录 {} 失败：{e}", parent.display()))?;
        }
        let mut lines: Vec<String> = Vec::new();

        let mut header = serde_json::to_value(&rec)
            .map_err(|e| format!("序列化会话头失败：{e}"))?;
        if let Some(obj) = header.as_object_mut() {
            obj.remove("messages"); // 消息另起行，头里不重复
        }
        lines.push(
            serde_json::to_string(&serde_json::json!({ "session": header, "message": null }))
                .map_err(|e| format!("序列化导出首行失败：{e}"))?,
        );
        for msg in &rec.messages {
            lines.push(
                serde_json::to_string(&serde_json::json!({ "session": id, "message": msg }))
                    .map_err(|e| format!("序列化消息行失败：{e}"))?,
            );
        }
        let mut text = lines.join("\n");
        text.push('\n');

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), nanos));
        std::fs::write(&tmp, &text)
            .map_err(|e| format!("写入临时文件 {} 失败：{e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("原子替换 {} 失败：{e}", path.display())
        })
    }

    /// 最近 limit 条消息的可读摘要：`角色: 截断文本`（每行 ≤120 字）。
    pub fn recall_preview(&self, id: &str, limit: usize) -> Result<Vec<String>, String> {
        let rec = self.get(id).ok_or_else(|| format!("会话不存在：{id}"))?;
        if limit == 0 || rec.messages.is_empty() {
            return Ok(Vec::new());
        }
        let start = rec.messages.len().saturating_sub(limit);
        Ok(rec.messages[start..]
            .iter()
            .map(|m| {
                let tag = match m.role {
                    0 => "用户",
                    1 => "智能体",
                    2 => "轨迹",
                    3 => "提示",
                    4 => "轮次",
                    5 => "工具结果",
                    _ => "消息",
                };
                let prefix = format!("{tag}: ");
                // 预留 1 字给截断省略号，保证整行 ≤120 字
                let room = 120usize.saturating_sub(prefix.chars().count() + 1);
                let chars: Vec<char> = m.text.trim().chars().collect();
                let mut body: String = chars.iter().take(room).collect();
                if chars.len() > room {
                    body.push('…');
                }
                format!("{prefix}{body}")
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_input() {
        assert_eq!(SessionRecord::title_from("扫描 127.0.0.1 的端口"), "扫描 127.0.0.1 的端口");
        assert_eq!(SessionRecord::title_from(""), "未命名会话");
        let long = "一".repeat(40);
        assert_eq!(SessionRecord::title_from(&long).chars().count(), 25);
    }

    #[test]
    fn crud_and_ordering() {
        let store = SessionStore::open_tmp().unwrap();
        let a = store.create("A", "", "deepseek").unwrap();
        let b = store.create("B", "ws1", "qwen").unwrap();
        assert_eq!(store.all().len(), 2);
        assert!(store.get(&a.id).is_some());
        let mut rec = store.get(&b.id).unwrap();
        rec.messages.push(StoredMessage::plain(0, "hello"));
        rec.messages.push(StoredMessage::plain(1, "world"));
        store.save(&rec).unwrap();
        store.update_stats(&b.id, Some(3), Some(120), Some(30), None).unwrap();
        let back = store.get(&b.id).unwrap();
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.messages[1].text, "world");
        assert_eq!(back.turns, 3);
        assert_eq!(back.prompt_tokens, 120);
        assert!(back.summary().contains("3 轮 · 150 tok"));
        assert!(store.delete(&a.id).unwrap());
        assert!(!store.delete(&a.id).unwrap());
        assert_eq!(store.all().len(), 1);
    }

    #[test]
    fn legacy_record_without_cwd_deserializes() {
        // 旧会话 JSON 没有 cwd 字段：必须能读回，且不会丢其他字段。
        let json = r#"{"id":"old","title":"旧会话","workspace":"ws","profile":"p",
            "turns":1,"prompt_tokens":2,"completion_tokens":3,
            "created_ms":0,"updated_ms":0,"messages":[]}"#;
        let rec: SessionRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.cwd, "");
        assert_eq!(rec.workspace, "ws");
        assert_eq!(rec.title, "旧会话");
    }

    #[test]
    fn create_in_records_workspace_cwd() {
        let store = SessionStore::open_tmp().unwrap();
        let rec = store.create_in("T", "ws", "p", "D:/work").unwrap();
        assert_eq!(rec.cwd, "D:/work");
        assert_eq!(store.get(&rec.id).unwrap().cwd, "D:/work");
    }

    #[test]
    fn append_messages_refreshes_timestamp() {
        let store = SessionStore::open_tmp().unwrap();
        let rec = store.create("C", "", "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        store.append_messages(&rec.id, &[StoredMessage::plain(2, "trace")]).unwrap();
        let back = store.get(&rec.id).unwrap();
        assert!(back.updated_ms >= rec.updated_ms);
        assert_eq!(back.messages[0].role, 2);
    }

    #[test]
    fn v2_state_roundtrip() {
        let store = SessionStore::open_tmp().unwrap();
        let rec = store.create("S", "", "").unwrap();
        let state = SessionState {
            messages: vec![
                StoredMessage::plain(0, "扫 80 端口"),
                StoredMessage {
                    role: 1,
                    text: String::new(),
                    tool_calls: Some(r#"[{"id":"c1","name":"net_tcp_scan","arguments":"{}"}]"#.into()),
                    tool_call_id: None,
                },
                StoredMessage {
                    role: 5,
                    text: "22,80 open".into(),
                    tool_calls: None,
                    tool_call_id: Some("c1".into()),
                },
            ],
            rolling_summary: Some("【关键发现】80/22 开放".into()),
            archived: vec![StoredMessage::plain(0, "旧轮次")],
        };
        store.save_state(&rec.id, state.clone()).unwrap();
        let back = store.get(&rec.id).unwrap().state.unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn v2_backward_compatible_deserialization() {
        // 旧版记录 JSON：无 state/tool_calls/tool_call_id 字段，必须能反序列化
        let old = r#"{
            "id": "old-1", "title": "旧会话", "workspace": "", "profile": "",
            "turns": 1, "prompt_tokens": 10, "completion_tokens": 5,
            "created_ms": 1, "updated_ms": 2,
            "messages": [{"role": 0, "text": "hi"}]
        }"#;
        let rec: SessionRecord = serde_json::from_str(old).unwrap();
        assert_eq!(rec.messages[0].text, "hi");
        assert!(rec.messages[0].tool_calls.is_none());
        assert!(rec.state.is_none());
        // 旧版单条消息落库后再读回，roundtrip 不丢
        let store = SessionStore::open_tmp().unwrap();
        store.save(&rec).unwrap();
        let back = store.get("old-1").unwrap();
        assert_eq!(back.messages[0].role, 0);
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("sess-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn list_by_workspace_filters_and_keeps_order() {
        let store = SessionStore::open_tmp().unwrap();
        let _global = store.create("G", "", "").unwrap();
        let w1a = store.create("W1a", "ws1", "").unwrap();
        let w2 = store.create("W2", "ws2", "").unwrap();
        let w1b = store.create("W1b", "ws1", "").unwrap();
        assert_eq!(store.list_by_workspace("").len(), 4, "空 workspace 返回全部");
        let ws1 = store.list_by_workspace("ws1");
        assert_eq!(ws1.len(), 2);
        assert_eq!(ws1[0].id, w1b.id, "最近创建的在前");
        assert_eq!(ws1[1].id, w1a.id);
        assert_eq!(store.list_by_workspace("ws2")[0].id, w2.id);
        assert!(store.list_by_workspace("nope").is_empty());
    }

    #[test]
    fn export_jsonl_writes_header_and_messages() {
        let store = SessionStore::open_tmp().unwrap();
        let rec = store.create("导出测试", "ws1", "p").unwrap();
        store
            .append_messages(
                &rec.id,
                &[
                    StoredMessage::plain(0, "你好"),
                    StoredMessage {
                        role: 1,
                        text: "收到".into(),
                        tool_calls: Some("[]".into()),
                        tool_call_id: None,
                    },
                ],
            )
            .unwrap();
        let dir = tmp_dir("export");
        let path = dir.join("nested/session.jsonl"); // 父目录不存在，应自动创建
        store.export_jsonl(&rec.id, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "会话头 + 两条消息");
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["session"]["id"].as_str().unwrap(), rec.id);
        assert_eq!(header["session"]["title"].as_str().unwrap(), "导出测试");
        assert!(header["session"].get("messages").is_none());
        assert!(header["message"].is_null());
        let m0: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(m0["session"].as_str().unwrap(), rec.id);
        assert_eq!(m0["message"]["role"].as_u64().unwrap(), 0);
        assert_eq!(m0["message"]["text"].as_str().unwrap(), "你好");

        // 错误路径：会话不存在
        assert!(store.export_jsonl("not-exist", &path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recall_preview_truncates_and_returns_recent() {
        let store = SessionStore::open_tmp().unwrap();
        let rec = store.create("预览", "", "").unwrap();
        store
            .append_messages(
                &rec.id,
                &[
                    StoredMessage::plain(0, "第一条"),
                    StoredMessage::plain(1, "第二条"),
                    StoredMessage::plain(5, &"长".repeat(200)),
                ],
            )
            .unwrap();
        assert!(store.recall_preview("not-exist", 3).is_err());
        assert!(store.recall_preview(&rec.id, 0).unwrap().is_empty());
        let preview = store.recall_preview(&rec.id, 2).unwrap();
        assert_eq!(preview.len(), 2);
        assert!(preview[0].starts_with("智能体: 第二条"), "{preview:?}");
        assert!(preview[1].starts_with("工具结果: "), "{preview:?}");
        assert!(preview.iter().all(|s| s.chars().count() <= 120));
    }
}
