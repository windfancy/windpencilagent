//! 内置技能：随程序预装的技能包素材。
//!
//! 素材内嵌在二进制中（include_str!），启动时若技能目录缺失对应技能
//! 则物化落盘（skills/<name>/SKILL.md），保证「安装即用」。

/// 内置技能包列表：(目录名, SKILL.md 全文)。
pub const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("attack-chain", ATTACK_CHAIN_SKILL),
    ("threat-intel", THREAT_INTEL_SKILL),
    ("nuclei-template-scan", NUCLEI_TEMPLATE_SKILL),
    ("usage-guide", USAGE_GUIDE_SKILL),
];

/// 通用攻击链规划技能：指导 LLM 按标准渗透链路分阶段规划任务。
pub const ATTACK_CHAIN_SKILL: &str = r#"---
name: attack-chain
version: 1.0.0
trigger: 渗透 攻击链 规划 测试计划 评估 安全评估 打靶 漏洞利用 web应用 网络探测
deps: scan.port, scan.finger, scan.web-banner, scan.web-outdated, scan.web-files, scan.host-vuln, scan.poc, scan.template, scan.sqlmap, scan.sqlmap-inject, scan.sqlmap-exploit, net.http, net.dns
---
# 通用攻击链规划

把用户的授权测试目标拆成标准链路，**按阶段推进、逐阶段汇报**，
不要跳过侦察直接进入利用。全程只使用授权范围内的目标。

## 阶段模型（依次推进）

1. **侦察 Recon**：DNS 解析确认目标可达与资产范围（net.dns）；
   端口扫描探活与服务发现（scan.port）。
2. **指纹 Fingerprint**：Web 指纹识别框架/中间件/语言（scan.finger）；
   HTTP 请求确认首页与关键路径响应（net.http）。
3. **漏洞映射 Vulnerability Mapping**：基于指纹结果推断已知风险面
   （scan.web-banner 产品版本、scan.web-outdated 缺陷版本、scan.host-vuln 主机风险）；
   敏感路径用 scan.web-files，危险端口如 21/23/445/3389 用 scan.port。
4. **验证 Verify**：单点到 scan.poc、批量模板到 scan.template；
   SQL 注入按 scan.sqlmap（扫描）→ scan.sqlmap-inject（验证）→ scan.sqlmap-exploit（利用）逐能力推进；
   高风险或破坏性动作必须先请求人工审批。
5. **汇报 Report**：输出结构化结论——资产清单、发现风险（按严重度排序）、
   证据（端口/响应头/版本号）、下一步建议。禁止编造未观察到的数据。

## 规划规则

- 每阶段先用 1-2 个工具收集事实，再决定是否进入下一阶段；
- 前一阶段结果改变后一阶段策略（例如发现 IIS → 优先查 ASP.NET 相关面）;
- 扫描类工具触发审批属正常流程，向用户说明理由；
- 目标超出授权范围 → 停止并提示用户修正范围；
- 汇报格式：`结论 → 证据 → 建议`，简洁分条。
"#;

/// 互联网搜索威胁情报技能：指导 LLM 用 web.* / net.* / kb.* 工具从免密钥公开源收集 IOC。
pub const THREAT_INTEL_SKILL: &str = r#"---
name: threat-intel
version: 1.0.0
trigger: 威胁情报 情报 IOC 恶意样本 恶意域名 恶意IP 钓鱼 情报搜集 威胁源 信誉查询 情报验证
deps: web.search, web.get, web.form, net.http, net.dns, kb.search, kb.add
---
# 互联网搜索威胁情报

围绕用户给定的目标（IP / 域名 / URL / 样本哈希 / CVE 编号 / 组织名）收集公开威胁情报。
**只用免密钥公开源**；全程使用 web.search / web.get / net.http / net.dns 检索，
结论沉淀到本地漏洞知识库（kb.add）。

## 工作流（按序推进，逐步收敛）

1. **明确查询对象**：把用户输入规范化为 IOC 清单（区分 ip / domain / url / hash / cve / org），
   逐条列出将要查询的对象，避免一次性泛搜。
2. **引擎检索**：web.search 以「目标 + 情报词」组合查询（如 `1.2.3.4 threat`、`evil.com malicious`、
   `CVE-2024-XXXX exploit poc`），取前 3-8 条结果，记录标题/URL/摘要。
3. **源页核证**：对可信结果用 web.get / net.http 打开原文，提取关键事实：
   关联恶意样本 SHA256、恶意 URL 路径、首次出现时间、活跃状态、攻击组织、目标行业。
4. **关联扩展**：net.dns 解析域名当前解析（A/CNAME/MX/TXT/NS）识别快速变动基础设施；
   证书透明度扩展子域名面；同段 IP、相似域名（DGA/typosquatting）纳入 IOC 清单。
5. **本地交叉验证**：kb.search 在漏洞知识库检索已知 POC / 历史记录；未命中的新 IOC 用 kb.add 沉淀
   （kind=poc 或 custom，source 标注情报源 URL，tags 写 IOC 类型）。
6. **输出情报报告**：严格按「结论 → 证据 → 建议」：
   - 结论：目标风险评级（高/中/低/无发现）+ 一句话定性；
   - 证据：IOC 表（类型 / 值 / 来源 / 首次观察 / 置信度 高中低）+ 关键原文摘录 URL；
   - 建议：封禁清单、监控点、后续验证动作。

## 免密钥公开源清单（web.get / net.http 可直接访问）

- **URLhaus（abuse.ch）**：恶意 URL/样本分发库。检索页 `https://urlhaus.browse/`，
  API `https://urlhaus-api.staging.abuse.ch/v1/{url,host,payload}/`（POST，部分接口免密钥）；
- **crt.sh 证书透明度**：`https://crt.sh/?q=<domain>` 输出子域名与签发记录（HTML/JSON，加 `&output=json`）；
- **ipinfo.io 免费层**：`https://ipinfo.io/<ip>/json` 归属 ASN/地理位置（限频 1k/天）；
- **Wayback CDX**：`https://web.archive.org/cdx/search/cdx?url=<domain>&output=json&limit=50` 历史快照；
- **NVD**：`https://services.nvd.nist.gov/rest/json/cves/2.0?keyword=<kw>` CVE 详情（免密钥限频）；
- **Exploit-DB 镜像页**：`https://www.exploit-db.com/search?q=<kw>`（web.search 兜底）；
- **DNS 公共解析情报**：8.8.8.8 / 1.1.1.1 DoH 端点（net.dns 工具已覆盖常规记录）。

各源限频与字段可能变动：解析失败时改用 web.search 引擎检索兜底，不要反复硬打同一端点。

## 置信度规则

- 两个及以上独立源一致 → 高；单源 → 中，且在报告中注明「单源待核实」；
- 只有搜索引擎摘要、未打开原文 → 低，禁止写入结论性判断；
- 数据时间超过 1 年的 IOC 标注「历史情报」，活跃性需重新验证。

## 红线

- 不对未授权目标发起主动探测（端口扫描/注入等不属于本技能；如需推进转 attack-chain 技能）；
- 情报源页面上的可疑链接**不要**直接访问恶意载荷 URL——只记录 URL 与哈希，不下载样本；
- 编造未观察到的数据、把推断写成事实均禁止。
"#;

/// nuclei 模板扫描技能：指导 LLM 用 tmpl.* 工具完成模板解析/扫描/工作流编排/知识库关联。
pub const NUCLEI_TEMPLATE_SKILL: &str = r#"---
name: nuclei-template-scan
version: 1.0.0
trigger: nuclei 模板 扫描模板 模板扫描 tmpl 漏洞模板 批量扫描 yaml模板 工作流编排 模板编排 模板检测
deps: tmpl.parse, tmpl.scan, tmpl.workflow, tmpl.kb, kb.import, kb.search, kb.add, net.http
---
# nuclei 模板扫描与工作流编排

用 tmpl.* 工具族执行 nuclei 风格 YAML 模板的解析、扫描、编排与知识库关联。
引擎为自研子集（HTTP 协议；matchers: status/word/regex/binary/dsl；extractors: regex/kval；
payloads 三攻击模式；workflow 门控递归）。不含 unsafe/race/pipeline/interactsh/headless 等高级特性。

## 工作流（六步，按序推进）

1. **确认授权**：目标必须在授权范围内；tmpl.scan / tmpl.workflow 为 High 风险，
   触发审批属正常流程，向用户说明将发起的请求形态。
2. **模板获取**（三选一）：
   - 手写模板：按 nuclei YAML 规范编写（id 正则、info.name/author 必填、severity 枚举）；
   - 知识库：先 kb.import 导入 nuclei-templates 目录 → kb.search 检索候选；
   - 本地目录：tmpl.kb 的 source=dir 直接指向模板目录。
3. **解析校验**：tmpl.parse 逐个校验模板，先修复 id/info/severity 错误再执行，
   消除"扫描一半才发现模板非法"的浪费。
4. **扫描执行**：
   - 单目标多模板 → tmpl.scan（templates 传 YAML 文本数组）；
   - 多目标 → tmpl.scan 的 targets 数组（≤64）；
   - 模板量大/需过滤 → tmpl.kb（query/tags/severity/author/id 过滤，全小写归一，
     severity=undefined 的探测模板恒通过）；
   - 含 interactsh（{{interactsh-url}}）的模板会被自动跳过并在 stats.errors 提示。
5. **递进编排**：tmpl.workflow 串联「探测 → 指纹确认 → 漏洞利用」链：
   workflows 步骤按 template 引用（模板池 id/名称匹配）或 tags 选取；
   matchers 门控（name 需出现在命中 matcher 名或 extractor 输出名，任一匹配即触发，
   matcher 内多 name 按 condition 组合默认 OR）；subtemplates 仅在上一步 matched 后递归执行，
   前步 extractor 输出自动注入子模板变量（{{name}}）。嵌套深度上限 8。
6. **复核与沉淀**：命中项逐条人工复核（匹配器命中 ≠ 漏洞成立，防误报）；
   确认的结果用 kb.add 沉淀（kind=poc，附命中 URL、matcher、提取值）；
   高危发现按「结论 → 证据 → 建议」汇报。

## 模板编写要点

- 变量：{{BaseURL}}/{{Hostname}}(含端口)/{{Host}}(不含)/{{Path}}(目录)/{{File}}/{{Scheme}}/
  {{Port}}/{{RootURL}}/{{Query}}；{{BaseURL}}:8080 触发端口覆盖；
- 优先级：URL 变量 < variables 块 < constants < workflow 注入 < payloads；
- payloads：内联多行或文件路径；attack=batteringram(单集)/pitchfork(对齐)/clusterbomb(笛卡尔积)；
- matchers-condition 默认 OR，单 matcher 内默认 AND；part 默认 body，
  header→all_headers，all=body+headers 拼接；kval extractor 直接查响应头/固定键（小写下划线）；
- dsl 子集：== != < > <= >= && || ! ~ contains/contains_any/starts_with/ends_with/
  to_upper/to_lower/len/regex/replace/substr/concat/base64/md5/sha256/hex/url 编码/unix_time；
- 多请求：internal extractor 提取的值自动回填后续请求 {{变量}}，前序事件可用
  `reqID_键` 引用（如 http_1_status_code）；stop-at-first-match 命中即停。

## 规则与红线

- 每步先少量模板试跑（max_templates=3~5），确认请求形态无破坏性后再放量；
- 破坏性 payload（删除/重启/爆破型）模板禁止直接执行，需用户明确批准；
- 命中的凭证/敏感数据输出必须脱敏（密码类字段掩码）；
- 结果不确定（误报风险高）时标注「待复核」，不写入结论性判断；
- 禁止编造模板执行结果；所有结论必须来自工具返回。
"#;

/// 使用指南技能：目录用途 / 启动模式 / 模型配置 / 工具速查 / 授权与审批 / 常见问题。
pub const USAGE_GUIDE_SKILL: &str = r#"---
name: usage-guide
version: 1.0.0
trigger: 使用指南 帮助 文档 目录 怎么用 操作手册 怎么启动 如何配置 使用说明 启动模式 模型配置 配置说明 常见问题
---
# windPenAgent 使用指南

面向授权渗透测试的智能体框架。本文回答「目录是什么 / 怎么启动 / 怎么配模型 /
工具有哪些 / 权限怎么控」。

## 目录用途（exe 同级）

| 路径 | 用途 |
|---|---|
| `dict/` | 字典库，前缀约定：`pwd_` 密码、`user_` 用户名、`dic_` 目录暴破、`dns_` 子域/vhost；每行一词，`#` 注释，缺文件回退内置精简表 |
| `scripts/` | Python 脚本（sandbox.python 只能执行此目录内脚本；file.write 可写入） |
| `python/` | 随包解释器（python-3.x-embed） |
| `data/` | 经验库 + 任务库（全局兜底）；`data/downloads/` 为 transfer.download 默认落点 |
| `skills/` | 技能库（内置技能缺失自动补装；state.json 记录禁用状态） |
| `workspaces/<name>/` | 工作空间（独立 data/ 分库）；`workspaces/workspaces.toml` 持久化空间定义 |
| `dbindex/` | 知识库（SQLite `vuln.db`；kb.import 导入 POCdb / nuclei-templates / exploitdb） |
| `config.toml` | 主配置（首次启动自动生成缺省；`init` 子命令输出示例） |
| `plugins/` / `reports/` | 自定义 Wasm 插件 / headless 作业报告 |

## 三种启动模式

| 模式 | 命令 | 说明 |
|---|---|---|
| 桌面客户端 | `windpenagent.exe`（无参/`chat`） | Slint 对话流 + 审批弹窗 + 八视图（对话/会话/任务/模型/插件/技能/工具/空间） |
| Web 主控台 | `serve --bind 127.0.0.1:8400` | 仪表盘 + SSE 事件流 + HITL 审批 API + `/api/chat` |
| 终端 REPL | `repl` | 内置 `/help` `/scope` `/model` `/exit`；Medium+ 逐次终端审批 |

其他子命令：`run --target X --objective "..."`（headless 作业出报告）、`observe`（旁观窗口）、
`plugins` / `skills` / `tasks`（管理）、`init`（示例配置）。全局 `--config <path>` 指定配置文件。

## 模型配置

- `config.toml [llm]`：driver / base_url / model / api_key / temperature / max_tokens（OpenAI 兼容接口，含本地 Ollama/vLLM）；
- `mode = "agent"`（默认，带工具执行任务）/ `"chat"`（纯聊天，不发工具）；
  **DeepSeek 等模型工具调用报 function.name 错误时确认 mode=agent**（工具名点号自动转义）；
- `max_tool_rounds`：单次任务最多工具轮数（ReAct 兜底终止条件）；
- 多模型：`[[llm.profiles]]`（含内置 10 预设）+ `llm.active`；桌面「模型」视图增删改/激活/测试，
  API 联通测试用 `llm.test` 工具（带最小 tools 探测，真实验证 agent 能力）；
- LLM 优先级：config `[llm]` base_url 非空 > 空间 llm_profile > profiles 的 active；
  API 不可用自动规则降级（不报错退出）。

## 工具分类速查（80+ 原子工具，命名「类型.功能」）

| 类别 | 工具 | 典型风险 |
|---|---|---|
| 侦察 | net.http / net.dns / scan.port / scan.finger / recon.* / detection.ct / detection.permute | Low~Medium |
| 枚举暴破 | detection.dir / detection.vhost / detection.dns（字典走 dict/） | Medium |
| 密码爆破（高危） | pwd.* 全部 / hash.crack / hash.identify | **High** |
| 漏洞验证（高危） | scan.poc / scan.poc-batch / scan.poc-exploit / scan.host-vuln / scan.sqlmap* / tmpl.scan / tmpl.workflow / scan.template | **High** |
| 载荷攻击（高危） | payload.reverse-shell / 载荷生成类 | **High** |
| Web 专项 | scan.web-banner / web-outdated / web-files / web-methods / web-robots / web-listing / web-auth / web-headers | Medium |
| 无头浏览器 | web.open / web.get / web.form / web.cookies / web.close / web.search | Low~High |
| 执行 | os.exec / shell.run / shell.sessions / sandbox.python / sandbox.pip / file.read / file.write | Medium~High |
| 传输 | transfer.download（落点 data/downloads 或 scripts）/ transfer.upload（源限 data/）/ socks.http | Medium |
| 知识库 | kb.search / kb.get / kb.add / kb.delete / kb.import | Low/Medium |
| 任务/空间 | task.* / ws.* | Low |
| LLM 管理 | llm.profiles / llm.test | Low |

三级决策树：优先现有工具 → 系统自带命令（os.exec/shell.run）→ 自写脚本
（file.write 写 scripts/ + sandbox.pip 装依赖 + sandbox.python 执行）。

## 授权范围与审批

- **scope = 仅这些**：授权目标白名单（逗号分隔），所有网络工具执行前经 ScopeGuard 校验；
- **deny = 除这些之外**：授权内排除清单，命中即拒绝（即使已在 scope）；
- 优先级：CLI `--target` > 活跃空间 scope > config.toml `[scope]`；空间 deny 合并生效；
- **审批（HITL）**：High 风险工具执行前弹审批卡片（桌面弹窗 / REPL 问答 / Web API），
  超时（默认 300s）视为拒绝；可选「本会话全部放行 / 全部拒绝」，轨道区徽标可复位；
- **工作空间**：空间 = scope/deny + 权限四开关（读/写/网/执）+ 模型 + 技能白名单 + cwd +
  工具白名单的隔离环境；切换**下一次会话生效**；数据随空间分库。

## 常见问题

1. **DeepSeek 工具调用报错** → 模型配置 mode 保持 `agent`；工具名点号已自动转义兼容；
2. **字典没生效** → 放 exe 同级 `dict/` 且前缀 `pwd_/user_/dic_/dns_` 开头；**字典不进知识库**，
   kb.search 查不到字典属正常；显式 `words`/`passwords` 参数始终优先；
3. **知识库导入体积对不上** → 看 kb.import 返回的导入统计（scanned/skipped/added/by-reason）逐项核对，
   超大文件跳过与去重都会计入 skipped；
4. **长对话变慢** → 上下文估算 token 达窗口 80% 自动压缩：保留最近几轮原始消息 + 更早历史转结构化摘要，
   压缩事件会上报前后 token 数；
5. **执行被拒** → 目标不在 scope 或命中 deny，或审批超时——先核对授权范围。
"#;
