<div align="center">
<h1>windpencilagent</h1>
<p><strong>一个不含业务语义的通用智能体基座</strong></p>
<p>装上模型就能对话、能用工具；换一份领域描述就换一种工作方式；<br>
专业能力以插件或声明式清单外挂，基座本身不认识任何行业词汇。</p>
<p>
<a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.92%2B-orange.svg" alt="Rust"></a>
<img src="https://img.shields.io/badge/platform-Windows-0078D6.svg" alt="Windows">
<a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue.svg" alt="License"></a>
</p>
<p>Apache License 2.0 · 可商用、可修改、可再分发 · 见 <a href="LICENSE">LICENSE</a></p>
</div>

---

## 这是什么

不是又一个聊天客户端，而是一个**可装配的智能体底座**。它把四件事拆开，各自独立演进：

| 层 | 内容 | 换掉它意味着 |
|---|---|---|
| 模型层 | OpenAI 兼容 / Anthropic / Gemini 三条原生协议 | 换厂商不用改代码路径 |
| 工具层 | 以「组」为单位的插件管理树 | 装卸能力不用重编译主程序 |
| 领域层 | 四份描述文件：角色 / 范式 / 常识 / 红线 | 改文件即改行为 |
| 权限层 | 全局组 → 领域引用 → 空间白名单 → 本轮阶段 | 只能逐级收窄，不能反向放大 |

基座与业务的边界是硬的：**任何 crate 不得依赖业务 crate**，专业能力只能以插件、声明式清单或领域私有外部工具的形式装进来。

---

## 核心设计

### 注册表是「组 ⇄ 插件」的管理树，与领域无关

全局唯一一份 `data/registry.toml`。组是独立管理的资产，所有领域共用；领域只能**引用**它，不能拥有、不能增删、不能禁用。

```
组（group）          条目（entry）
├── core/            plan.phase, util.echo, kb.search ...
├── file/            file.read, file.write, file.path_info(插件)
├── shell/           shell.run, shell.jobs, shell.sessions
├── unassigned/      裸 dll 首次收编的落点
└── 用户自建组        放什么由你说了算
```

状态是**全局**的：一个组在「能力」页被禁用，引用了它的所有领域同时收窄。领域页看到的组状态是只读的——那里只能决定"我要不要引用"，不能决定"这个组存不存在"。

内置工具与内置组有额外约束：内置条目不能换组，内置组不能删除。被手工挪走的内置条目会在下次启动自动归位。

### 领域只做收窄，不做管理

引用链是一条单向窄化的路：

```
全局组已启用  →  领域 domains/<id>/domain.toml 的 groups  →  空间白名单  →  本轮阶段
     ↑ 能力页决定              ↑ 领域页决定                    ↑ 空间页决定      ↑ 运行时决定
```

领域没有自己的插件状态——不存在"某个领域把某个组关了"这种概念。想让某个领域少几个工具，就少引用一个组。

### 零手写：把 dll 拷进来就能用

不写 `PnC.toml`、不改 `Cargo.toml`、不重编主程序：

```
plugins/foo.dll
   │  下次启动
   ▼
pnc-probe.exe 子进程探测（隔离 DllMain，5 秒超时，(size, mtime) 缓存）
   │  读出内嵌清单
   ▼
plugins/unassigned/file.path_info/    ← 真搬文件，不是虚拟挂载
   ├── foo.dll
   └── PnC.toml                        ← 按清单倒推生成
   │
   ▼
「能力」页可见 → 点安装 → 分配到组 → 启用
```

探测刻意放在**独立进程**里：`LoadLibrary` 会执行对方 `DllMain`，未受信任的 dll 不该在宿主地址空间里跑第一行代码。探测进程只做一件事——问一句"你叫什么、要什么权限"，然后退出，不给插件任何宿主句柄，也不调 `wbp_init`。

> `pnc-probe.exe` 必须与主程序同目录。缺了它，裸 dll 收编会失败，可以退到"手写 `PnC.toml`"这条老路（设置里关掉 `plugins.auto_probe`）。

### 换组 = 搬目录

账上在 B 组、文件却在 A 组，是这类系统最常见的隐性错误。所以换组时 dll 目录跟着走。

dll 被 IDE 或编译器占用（`os error 32`）时不回滚账目——用户的意图是对的，只是时机不对——记进 `.pending-moves.json`，下次启动在扫描之前补做，一次收敛。

### 双闸门：未安装的 dll 连加载都不会发生

```rust
if !registry.is_installed(&name) || !registry.is_active(&name) {
    continue;          // 不注册，不 dlopen
}
```

扫描只产出**候选**，注册表才是账本。磁盘上放一个 dll 不会立刻出现在工具面里——必须先安装。这样半成品插件不会被模型调用。

### 领域私有外部工具

领域可以带自己的外部工具，写在 `domain.toml` 里：

```toml
[[external]]
kind      = "exe"                    # exe | declarative
name      = "DOCX 转 PDF"
path      = "./bin/docx2pdf.exe"     # 工具名 = docx.docx2pdf
usage     = "把 docx 转成 pdf；参数是输入文件路径，输出到同目录"
risk      = "low"                    # 省略则为 high（外部程序 fail-closed）
namespace = "docx"                   # 省略则为 ext

[[external]]                         # 声明式：指向一个含 PnC.toml 的目录
kind = "declarative"
name = "转大写"
path = "./upper"                     # 工具名 = ext.upper
```

工具名由 `namespace` + `path` 派生（`<namespace>.<文件名或目录名>`），不需要也不支持手写 id。`exe` 类还可以用 `args = ["{{input}}"]` 配 `[external.params]` 声明参数、用 `stdin` 传模板。

这些工具**不进 `plugins/`、不进全局注册表、不出现在能力页**，由领域独立管理，换领域就消失。相对路径按领域目录解析，`..` 一律拒绝。

---

## 快速开始

### 预编译包

从 [Releases](https://github.com/windfancy/windpencilagent/releases) 下载 `windpencilagent-<version>-win64.zip`，解压即用，无安装步骤。

```
windpencilagent.exe        桌面端
windpencilagent-cli.exe    命令行
pnc-probe.exe              裸 dll 探测器（必须与上面两个同目录）
domains/default/           默认领域模板
plugins/file/              三个示例插件（dll + PnC.toml）
```

首次启动会在程序同级生成 `data/`（注册表、会话库、经验库）与 `workspaces/`。

### 从源码构建

```bash
git clone https://github.com/windfancy/windpencilagent.git
cd windpencilagent

cargo build --release --workspace
```

产物都在 `target/release/`：
`windpencilagent.exe` / `windpencilagent-cli.exe` / `pnc-probe.exe`，天然同目录，可直接运行。

```bash
./target/release/windpencilagent-cli.exe check     # 配置自检
./target/release/windpencilagent-cli.exe tools     # 当前工具面
./target/release/windpencilagent-cli.exe           # 交互对话
./target/release/windpencilagent.exe               # 桌面端
```

**没有配置模型也不会崩**，只是进入降级模式：只有通用工具，不调 LLM。

### 加插件

仓库**不含插件源码**，`plugins/` 是运行时目录，从空开始。

**用现成的 dll**：把 dll 丢进 `plugins/` 根目录，下次启动自动收编进「未分配」组，再在「能力」页安装、分配、启用。

**自己写一个**：需要 `plugin-sdk`（插件与宿主共用的 HostApi 封装）与 `plugins/Cargo.toml` 这个 workspace 骨架。二者不随本仓库发布，需向作者索取；拿到后放进 `plugins/`，再用 `windpencilagent-cli new-plugin --id <命名空间.名字> --group <组>` 生成可编译的插件工程。

### 配模型

桌面端 `配置 → 模型 → 从预设新增`，选一家厂商，填 API 密钥即可。预设已带好该厂商的协议、地址与模型名。

内置预设覆盖 DeepSeek、通义、豆包、智谱、Kimi、MiniMax、文心、混元、OpenRouter、硅基流动、Ollama、vLLM、LM Studio 等。驱动按**协议**分而不按厂商分——写错协议名会直接报错并列出支持项，不会静默回落到 OpenAI。

---

## 命令行

```
windpencilagent-cli [--config <path>] [--yes] [<command>]

  (无子命令) / chat              交互对话
  run --objective <s>            单次任务（退出码 = 成功与否）
  tools                          列出当前工具面（按组）
  call <tool> [json]             直接调工具，不经过 LLM（冒烟 / 调试；授权范围取 [scope] authorized）
  domain list | show | set-groups 领域列表 / 详情 / 改写组引用
  registry list | install | uninstall | enable | disable | move
  registry export [--check]      生成或校验领域的工具清单
  new-plugin --id <id> --group <g>   生成插件骨架
  init                           写出示例配置（已存在不覆盖）
  check                          配置自检
```

`--yes` 用于无人值守场景（自动批准高风险工具）。交互模式下高风险工具逐次询问。

---

## 领域

`domains/<id>/` 由四份文件定义，格式刻意不同：意图用自然语言，判定用结构化。

| 文件 | 作用 | 注入方式 |
|---|---|---|
| `domain.toml` | 角色、引用哪些组、领域参数、私有外部工具 | 装配期读取 |
| `workflow.md` | 工作范式（怎么写、按什么顺序） | front matter 控制注入时机与字数预算 |
| `knowledge.md` | 常识条目，按 `## 标题` 切分 | 命中才进上下文 |
| `constraints.toml` | 红线（强度 + 条件 + 工具/目标匹配） | 编译成可执行 guard，不是"靠自觉"的 prompt |
| `toolkit.toml` | 工具清单 | 生成物，`registry export` 产出，不参与装配 |

切换领域 = 换一份工具面 + 换一段 prompt，不卸载任何 dll。插件是否加载只看**组是否被引用且启用**。

---

## 配置

`crates/wpa-engine/src/config_schema.rs` 是唯一真相：一张表同时驱动界面渲染、读写、校验、重启提示。**新增一个参数只改这一个文件。**

界面上是 4 个二级页、11 张卡片：

| 二级页 | 包含 |
|---|---|
| 配置 · 模型 | 模型接入（档案列表：启用 / 测连通） |
| 配置 · 内置能力 | 外观与窗口、上下文与压缩、智能体循环、记忆与知识、网络与请求 |
| 配置 · 插件与安全 | 沙箱与审批、授权与红线、插件运行时 |
| 配置 · 数据与路径 | 日志与数据 |
| （领域页内嵌） | 激活领域、切换后是否保留会话、领域目录 |

约定：**能点选就不让用户手打**。领域与模型档案都是运行期候选项，控件按运行期列表渲染；配置文件只作为高级双通道入口。

---

## 目录结构

```
crates/
  wpa-api          契约层：群组插件契约、注册表类型、树清单、SemVer
  wpa-engine       装配与会话构建：工具面、上下文预算、领域装载、配置 schema
  wpa-desktop      Slint 桌面端（只驱动引擎：读快照 / 写命令 / 收回执）
  wpa-cli          命令行入口（同上）
  agent-core       智能体循环：感知 → 规划 → 记忆 → 行动 → 反思 → 协作
  base-llm         LLM 驱动：openai / anthropic / gemini + 内置预设 + 降级链
  data-core        SQLite 数据表与向量检索（kb.* 工具的实现）
  toolkit-core     通用工具：文件 / 沙箱 / 审批 / 钩子 / 会话检索
  shell-core       持久 shell 会话与后台作业
  net-core         网络原语（自研 TCP/DNS/HTTP，不调 curl/nmap/dig）
  text-core        编解码 / 哈希 / 正则 / 线索提取
  workspace-core   工作空间：分项目隔离的记忆、目录、授权范围
  knowledge-core   知识条目加载与检索
  tree-core        群组装配、阶段机、声明式执行（pnc / exec）
  plugin-abi       插件 C ABI（布局有静态断言守门）
  plugin-host      插件装载、探测、收编、搬迁与调用桥
  kern-core        工具注册表、执行器、授权守卫
  base-store       经验库 / 任务库 / 会话库 / 大结果转存
  browser-core     无头 HTTP 浏览器（会话池 + Cookie + 限流）
  wpa-kit          宿主与插件共用的纯工具库（编解码 / 哈希 / HTML / 差分，只依赖 std）
```

运行时目录（首次启动自动生成，与 exe 同级）：

```
data/          registry.toml（全局注册表）、会话库、经验库、迁移归档
domains/       领域描述包
plugins/       插件目录，<组>/<条目>/ 结构
workspaces/    工作空间
```

---

## 开发

```bash
cargo test --workspace        # 全量测试
cargo build --release         # 发布构建
```

几条写进代码注释的约定：

- **功能必须自己实现**，不借第三方程序兜底（不调外部 exe、不蹭别人进程的 IPC）。
- **基座不认识业务**：任何 crate 不得依赖业务 crate，不得出现行业专有词汇。
- **权限只做减法**：全局 → 领域 → 空间 → 阶段，每一级只能收窄。
- **失败要可见**：缺文件记 `broken` 而不是让条目消失；探测失败报原因而不是静默跳过。

插件开发规范：用 `new-plugin` 生成骨架，写 `run()`，编译成 cdylib，把 dll 放进 `plugins/<组>/<条目>/`。

> **本仓库只发布源代码**：设计文档、内部记录与编译产物（`docs/`、`*.dll`）不随仓库分发，在 `.git/info/exclude` 里做了本地排除。真相优先级：源码 > README > 自动生成的 `toolkit.toml`。

---

## 已知边界

- **不含插件源码与 SDK**：`plugins/` 由你自建，仓库只提供运行时、ABI 与收编链路；示例插件与 `plugin-sdk` 不随仓库分发。
- **只支持 Windows**：受限令牌、AppContainer、插件 ABI 均按 Windows 实现。部分沙箱测试在受限环境里跑不起来，标了 `#[ignore]`。
- **界面未签名**：首次运行可能弹 SmartScreen「未知发布者」，需点「更多信息 → 仍要运行」。
- **`web.*`（无头浏览器）暂未暴露成工具**，只供插件通过 HostApi 使用。
- **单机运行**：无服务端、无多租户，会话与注册表都在本地文件里。

---

## 许可证

本项目采用 **Apache License 2.0**（全文 [LICENSE](LICENSE)，版权声明 [NOTICE](NOTICE)）。

| | 内容 |
|---|---|
| 允许 | 商业使用、修改、再分发、私人使用，并附带专利许可 |
| 条件 | 保留版权、许可与 `NOTICE`；显著标注修改过的文件 |
| 不授予 | 商标权；不得用本项目名称或作者名义为衍生品背书 |
| 担保 | 无。按「原样」提供，不承担任何明示或默示担保 |

分发二进制或衍生作品时，请一并附带 `LICENSE` 与 `NOTICE`。

第三方依赖各自遵循其原始许可证（多为 MIT / Apache-2.0），完整清单见 `Cargo.lock`。
