//! 自研轻量 HTML 解析：标签/属性/表单/链接/脚本/注释。
//!
//! 不做完整 DOM、不做 CSS 选择器全语法——**够抓"接口、参数、表单、入口"就行**。
//! 用不上真正的解析器（引入 html5ever 会让每个插件多几百 KB）。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::textx;

/// 一个属性。
#[derive(Debug, Clone, PartialEq)]
pub struct Attr {
    pub name: String,
    pub value: String,
}

impl Attr {
    pub fn get<'a>(attrs: &'a [Attr], name: &str) -> Option<&'a str> {
        attrs
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
            .map(|a| a.value.as_str())
    }
}

/// 扫描出的标签。
#[derive(Debug, Clone)]
pub struct TagRef {
    pub name: String,
    pub attrs: Vec<Attr>,
    /// 起始偏移（`<` 的位置）。
    pub start: usize,
    /// 结束偏移（`>` 之后）。
    pub end: usize,
    pub closing: bool,
    pub self_closing: bool,
}

impl TagRef {
    pub fn attr(&self, name: &str) -> Option<&str> {
        Attr::get(&self.attrs, name)
    }

    pub fn class_contains(&self, needle: &str) -> bool {
        self.attr("class")
            .map(|c| c.split_whitespace().any(|x| x == needle))
            .unwrap_or(false)
            || self
                .attr("id")
                .map(|i| i.eq_ignore_ascii_case(needle))
                .unwrap_or(false)
    }
}

/// 解析属性串（`<tag` 之后到 `>` 之前）。
fn parse_attrs(s: &str) -> Vec<Attr> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let ns = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'>' {
            i += 1;
        }
        let name = s[ns..i].to_string();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let vs = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = textx::decode_entities(&s[vs..i.min(s.len())]);
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let vs = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                value = textx::decode_entities(&s[vs..i]);
            }
        }
        if !name.is_empty() {
            out.push(Attr { name, value });
        }
    }
    out
}

/// 扫描全部标签（跳过注释与 script/style 内容）。
pub fn scan_tags(html: &str) -> Vec<TagRef> {
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // 注释
        if lower[i..].starts_with("<!--") {
            match html[i..].find("-->") {
                Some(k) => i = i + k + 3,
                None => break,
            }
            continue;
        }
        if lower[i..].starts_with("<!") || lower[i..].starts_with("<?") {
            match html[i..].find('>') {
                Some(k) => i = i + k + 1,
                None => break,
            }
            continue;
        }
        let closing = lower[i + 1..].starts_with('/');
        let ns = if closing { i + 2 } else { i + 1 };
        let mut ne = ns;
        while ne < bytes.len() && (bytes[ne].is_ascii_alphanumeric() || bytes[ne] == b'-' || bytes[ne] == b':') {
            ne += 1;
        }
        if ne == ns {
            i += 1;
            continue;
        }
        let name = lower[ns..ne].to_string();
        // 找 `>`
        let (end, inner) = match html[ne..].find('>') {
            Some(k) => (ne + k + 1, &html[ne..ne + k]),
            None => break,
        };
        let self_closing = inner.trim_end().ends_with('/');
        out.push(TagRef {
            name: name.clone(),
            attrs: if closing { Vec::new() } else { parse_attrs(inner) },
            start: i,
            end,
            closing,
            self_closing,
        });
        // 脚本/样式内容整段跳过
        if !closing && (name == "script" || name == "style") && !self_closing {
            let close = format!("</{name}");
            match lower[end..].find(&close) {
                Some(k) => i = end + k,
                None => {
                    i = end;
                }
            }
        } else {
            i = end;
        }
    }
    out
}

/// 链接。
#[derive(Debug, Clone)]
pub struct Link {
    pub href: String,
    pub text: String,
    pub rel: String,
}

/// 表单字段。
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub kind: String,
    pub value: String,
}

/// 表单。
#[derive(Debug, Clone)]
pub struct Form {
    pub action: String,
    pub method: String,
    pub fields: Vec<Field>,
}

impl Form {
    /// 是否有密码框（登录表单的判据）。
    pub fn has_password(&self) -> bool {
        self.fields.iter().any(|f| f.kind == "password")
    }

    /// 有密码框且没有提交按钮以外的复杂字段 ⇒ 极可能是登录表单。
    pub fn looks_like_login(&self) -> bool {
        self.has_password()
            && self
                .fields
                .iter()
                .any(|f| f.kind == "text" || f.kind == "email" || f.kind == "tel")
    }
}

/// 解析结果。
#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub title: String,
    pub links: Vec<Link>,
    pub forms: Vec<Form>,
    pub scripts: Vec<String>,
    pub inline_js: String,
    pub comments: Vec<String>,
    pub emails: Vec<String>,
    pub text: String,
    pub meta: BTreeMap<String, String>,
    /// 表单/URL 里出现过的参数名（去重、保序）。
    pub param_names: Vec<String>,
    /// 内联事件处理器（onclick 等）、`javascript:` 链接——XSS 审计用。
    pub handlers: Vec<String>,
}

impl Doc {
    pub fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "links": self.links.iter().map(|l| json!({"href": l.href, "text": l.text, "rel": l.rel})).collect::<Vec<_>>(),
            "forms": self.forms.iter().map(|f| json!({
                "action": f.action, "method": f.method,
                "fields": f.fields.iter().map(|x| json!({"name": x.name, "type": x.kind, "value": x.value})).collect::<Vec<_>>()
            })).collect::<Vec<_>>(),
            "scripts": self.scripts,
            "comments": self.comments,
            "emails": self.emails,
            "text": self.text.chars().take(4000).collect::<String>(),
            "meta": self.meta,
            "param_names": self.param_names,
            "handlers": self.handlers,
        })
    }
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !s.is_empty() && !v.iter().any(|x| x == &s) {
        v.push(s);
    }
}

/// 解析整页。
pub fn parse(html: &str, base_url: &str) -> Doc {
    let tags = scan_tags(html);
    let mut doc = Doc::default();
    let lower = html.to_ascii_lowercase();

    // 注释（scan_tags 跳过，但这里要单独抽）
    let mut rest = html;
    let mut consumed = 0usize;
    while let Some(s) = rest.find("<!--") {
        let abs = consumed + s;
        match lower[abs..].find("-->") {
            Some(k) => {
                let body = &html[abs + 4..abs + k];
                let body = body.trim();
                if !body.is_empty() && body.len() < 2000 {
                    doc.comments.push(body.to_string());
                }
                consumed = abs + k + 3;
                rest = &html[consumed..];
            }
            None => break,
        }
    }

    // 标题 / meta
    for (idx, t) in tags.iter().enumerate() {
        if t.name == "title" && !t.closing {
            if let Some(close) = tags[idx..].iter().position(|x| x.name == "title" && x.closing) {
                let ct = &tags[idx + close];
                // 与下方 <a> 分支对齐：畸形 HTML 里可能后一个标签的 start 早于
                // 前一个的 end，直接切片会 panic。
                doc.title = textx::strip_tags(&html[t.end.min(ct.start)..ct.start]);
            }
        }
        if t.name == "meta" {
            if let (Some(k), Some(v)) = (t.attr("name").or_else(|| t.attr("property")), t.attr("content")) {
                doc.meta.insert(k.to_ascii_lowercase(), v.to_string());
            }
        }
        if t.name == "a" && !t.closing {
            if let Some(href) = t.attr("href") {
                let resolved = textx::resolve_url(base_url, href);
                if let Some(k) = tags[idx..].iter().position(|x| x.name == "a" && x.closing) {
                    let ct = &tags[idx + k];
                    let text = textx::strip_tags(&html[t.end.min(ct.start)..ct.start]);
                    if !resolved.is_empty() {
                        let mut params: Vec<(String, String)> = Vec::new();
                        if let Some(u) = textx::parse_url(&resolved) {
                            params = crate::codec::parse_query(&u.query);
                        }
                        for (k, _) in params {
                            push_unique(&mut doc.param_names, k);
                        }
                        doc.links.push(Link {
                            href: resolved,
                            text: text.chars().take(120).collect(),
                            rel: t.attr("rel").unwrap_or("").to_string(),
                        });
                    }
                }
            }
        }
        if t.name == "script" && !t.closing {
            if let Some(src) = t.attr("src") {
                let resolved = textx::resolve_url(base_url, src);
                push_unique(&mut doc.scripts, resolved);
            } else if !t.self_closing {
                if let Some(k) = tags[idx..].iter().position(|x| x.name == "script" && x.closing) {
                    let ct = &tags[idx + k];
                    let body = &html[t.end.min(ct.start)..ct.start];
                    doc.inline_js.push_str(body);
                    doc.inline_js.push('\n');
                }
            }
        }
        if t.name == "form" && !t.closing {
            let action = textx::resolve_url(
                base_url,
                t.attr("action").unwrap_or(""),
            );
            let method = t.attr("method").unwrap_or("get").to_ascii_uppercase();
            let mut fields = Vec::new();
            if let Some(k) = tags[idx..].iter().position(|x| x.name == "form" && x.closing) {
                for inner in &tags[idx..idx + k] {
                    if inner.name == "input" || inner.name == "select" || inner.name == "textarea" {
                        let name = inner.attr("name").unwrap_or("").to_string();
                        if name.is_empty() {
                            continue;
                        }
                        let kind = inner
                            .attr("type")
                            .unwrap_or(if inner.name == "input" { "text" } else { &inner.name })
                            .to_ascii_lowercase();
                        fields.push(Field {
                            name: name.clone(),
                            kind,
                            value: inner.attr("value").unwrap_or("").to_string(),
                        });
                        push_unique(&mut doc.param_names, name);
                    }
                }
                // 表单 method 缺省时按 GET 处理；action 为空则落在当前页
                doc.forms.push(Form {
                    action: if action.is_empty() { base_url.to_string() } else { action },
                    method,
                    fields,
                });
            }
        }
    }

    // 事件处理器（XSS 审计）
    for t in &tags {
        for a in &t.attrs {
            if a.name.to_ascii_lowercase().starts_with("on") && !a.value.is_empty() {
                doc.handlers.push(format!("{}={}", a.name, a.value));
            }
            if a.name.eq_ignore_ascii_case("href") && a.value.trim_start().starts_with("javascript:") {
                doc.handlers.push(format!("href={}", a.value));
            }
        }
    }

    // 邮箱（手写扫描，不引正则）
    doc.emails = crate::textx::extract_emails(html, 50);

    // 纯文本
    doc.text = textx::strip_tags(html);
    doc
}

/// 简易节点查询：按标签名 + 属性名/值 + 文本包含过滤。
pub fn query(html: &str, tag: &str, attr_name: Option<&str>, attr_value: Option<&str>, text_contains: Option<&str>) -> Vec<TagRef> {
    let tags = scan_tags(html);
    let mut out = Vec::new();
    for (idx, t) in tags.iter().enumerate() {
        if t.closing || !t.name.eq_ignore_ascii_case(tag) {
            continue;
        }
        if let Some(n) = attr_name {
            match t.attr(n) {
                Some(v) => {
                    if let Some(need) = attr_value {
                        if !v.eq_ignore_ascii_case(need) {
                            continue;
                        }
                    }
                }
                None => continue,
            }
        }
        if let Some(need) = text_contains {
            if let Some(k) = tags[idx..].iter().position(|x| x.name.eq_ignore_ascii_case(tag) && x.closing) {
                let ct = &tags[idx + k];
                let body = &html[t.end.min(ct.start)..ct.start];
                if !body.contains(need) {
                    continue;
                }
            } else {
                continue;
            }
        }
        out.push(t.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html>
<html><head><title>Demo &amp; Test</title>
<meta name="generator" content="Joomla! 3.9">
<script src="/js/app.js"></script>
<script>var api="/api/v1/users?id=1";</script>
</head><body>
<!-- build: 2024-01-01 -->
<a href="/login?next=/admin">Login</a>
<a href="https://other.com/x">ext</a>
<form action="/login" method="post">
  <input name="user" type="text">
  <input name="pass" type="password">
  <input type="submit" value="Go">
</form>
<div onclick="alert(1)">x</div>
<a href="javascript:void(0)">js</a>
<p>contact: admin@example.com</p>
</body></html>"#;

    #[test]
    fn parses_core_fields() {
        let d = parse(PAGE, "http://site.local/");
        assert_eq!(d.title, "Demo & Test");
        assert_eq!(d.meta.get("generator").map(String::as_str), Some("Joomla! 3.9"));
        assert_eq!(d.scripts, vec!["http://site.local/js/app.js"]);
        assert!(d.inline_js.contains("/api/v1/users"));
        assert_eq!(d.comments, vec!["build: 2024-01-01"]);
        assert!(d.emails.contains(&"admin@example.com".to_string()));
        assert!(d.handlers.iter().any(|h| h.contains("onclick")));
        assert!(d.handlers.iter().any(|h| h.contains("javascript:")));
    }

    /// 中文页面：title / 正文 / 链接文字都不能乱码，也不能在内联脚本处断掉。
    #[test]
    fn parses_chinese_page() {
        let page = r#"<html><head><title>中文标题：测试</title></head>
<body><h1>产品介绍</h1>
<p>这是一段中文正文，含全角标点：（）【】</p>
<script>var 名字 = "中文";</script>
<a href="/详情?id=1">查看详情</a>
</body></html>"#;
        let d = parse(page, "http://cn.local/");
        assert_eq!(d.title, "中文标题：测试");
        assert!(d.text.contains("产品介绍"), "正文乱码：{}", d.text);
        assert!(
            d.text.contains("全角标点：（）【】"),
            "多字节字符被破坏：{}",
            d.text
        );
        assert!(
            !d.text.contains("var 名字"),
            "内联脚本不应进入正文：{}",
            d.text
        );
        assert!(
            d.links.iter().any(|l| l.text == "查看详情"),
            "链接文字乱码：{:?}",
            d.links.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parses_form_and_link() {
        let d = parse(PAGE, "http://site.local/");
        assert_eq!(d.forms.len(), 1);
        let f = &d.forms[0];
        assert_eq!(f.action, "http://site.local/login");
        assert_eq!(f.method, "POST");
        assert!(f.looks_like_login());
        let names: Vec<&str> = f.fields.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["user", "pass"]);
        // submit 无 name 应被跳过
        assert!(d.links.iter().any(|l| l.href == "http://site.local/login?next=/admin"));
        assert!(d.param_names.contains(&"user".to_string()));
        assert!(d.param_names.contains(&"next".to_string()));
    }

    #[test]
    fn query_by_attr() {
        let hits = query(PAGE, "input", Some("type"), Some("password"), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].attr("name"), Some("pass"));
        // 表单内部含 password 输入框 ⇒ 按文本包含能命中
        assert_eq!(query(PAGE, "form", None, None, Some("password")).len(), 1);
        assert!(query(PAGE, "form", None, None, Some("no-such-text")).is_empty());
    }

    #[test]
    fn unquoted_attrs_and_self_closing() {
        let d = parse(r#"<img src=/a.png><input name=x value=y>"#, "http://h/");
        let tags = scan_tags(r#"<img src=/a.png>"#);
        assert_eq!(tags[0].attr("src"), Some("/a.png"));
        assert!(d.forms.is_empty());
    }
}
