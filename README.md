# wxherd

[![CI](https://github.com/fxz2018/wxherd/actions/workflows/ci.yml/badge.svg)](https://github.com/fxz2018/wxherd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-stable-orange.svg)

**微信 ↔ [Herdr](https://herdr.dev) 全功能桥接插件 —— 在手机上遥控你的 AI 编码 agent。**

> **English** — wxherd bridges [Herdr](https://herdr.dev) (a terminal workspace
> manager for AI coding agents) to personal WeChat over the iLink bot protocol.
> When an agent blocks on approval, finishes a task, or asks a multi-question
> questionnaire, you get a formatted WeChat message; reply from your phone to
> approve, reject, interrupt, read output, or type a follow-up. Written in Rust,
> runs as a Herdr plugin with a background daemon and a TUI settings pane.

---

## 为什么需要它

Herdr 让你同时盯着十几个 agent pane。但你不可能一直坐在终端前——去泡咖啡、出门买饭、
躺床上时，agent 卡在「是否允许执行这条命令？」就白白干等。

wxherd 把这条链路搬到微信：

```
┌─────────┐  iLink 协议   ┌───────────┐   herdr CLI   ┌──────────────┐
│  微信   │ ⇄  AES+CDN  ⇄ │  wxherd   │ ⇄  pane API ⇄ │ herdr agents │
│ (手机)  │               │  daemon   │   会话 jsonl  │ Claude/pi/…  │
└─────────┘               └───────────┘               └──────────────┘
```

## 特性

| 能力 | 说明 |
|---|---|
| **格式化推送** | 有限 Markdown（代码块 / 表格 / 加粗）+ 终端画面清洗，手机上读得懂；agent 的「末次输出」直读会话 jsonl，只给干净原文 |
| **双向遥控** | 批准 / 拒绝 / 中断 / 读输出 / 发指令 / 切 pane，全部在微信里完成 |
| **省输入设计** | 数字快捷回复（`1 批准`）、`paneID 直达`（`w8:p3 批准`）、口语指令（「批准第二个」）匹配零 AI 调用 |
| **问卷一次答完** | agent 用结构化问卷提问时，微信侧一次列出全部题面与选项；回 `1.2 2.1` 即逐题作答，无需来回多轮 |
| **AI 意图层（可选）** | 任意 OpenAI 兼容端点；自然语言 → 结构化指令，执行成功后自动沉淀为规则，第二次起零 AI 调用 |
| **媒体收发** | 收图 / 文件 / 视频自动落盘并回传路径；也能把本地图片、文件发给微信（CDN + AES-128-ECB） |
| **语音遥控** | 语音消息直接当命令用（服务端 ASR，零额外成本） |
| **Herdr 原生集成** | startup hook 跟随启动、事件钩子 + 轮询双路径去重、`prefix+u` 弹出设置框、无常驻 UI |
| **推送不烦人** | 同一 pane 冷却 60s、全局最小间隔 4s、免打扰时段、短任务（<10s）不推；嫌吵可关掉「已聚焦也推送」 |

## 环境要求

- **Rust** stable（较新版本；从源码编译时需要）
- **[Herdr](https://herdr.dev) 0.9.0+**
- 一台能扫码登录微信的设备
- **Windows / macOS / Linux**：代码在三个平台均可编译并通过单元测试（CI 矩阵持续验证）；
  守护进程的分离拉起、进程探活/终止已按平台分别实现
  （Windows `DETACHED_PROCESS` + `tasklist`/`taskkill`，Unix `process_group(0)` + `kill`）。
  实际使用验证以 Windows + herdr 0.9.5 为主，Unix 侧运行时表现欢迎反馈

## 安装

### 方式零：直接下载预编译二进制（不用装 Rust）

到 [Releases](https://github.com/fxz2018/wxherd/releases) 下载对应平台的包：

| 平台 | 文件 |
|---|---|
| Windows x64 | `wxherd-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `wxherd-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `wxherd-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `wxherd-x86_64-apple-darwin.tar.gz` |

解压出来的目录**就是可直接使用的 herdr 插件目录**（内含 `herdr-plugin.toml`
与 `target/release/wxherd`）：

```bash
herdr plugin link /path/to/wxherd-x86_64-unknown-linux-gnu
./wxherd-x86_64-unknown-linux-gnu/target/release/wxherd login   # 扫码绑定
```

### 方式一：作为 Herdr 插件（从源码编译）

```bash
git clone https://github.com/fxz2018/wxherd
cd wxherd
cargo build --release    # 注意：herdr plugin link 不会执行清单里的 [[build]]
herdr plugin link .
```

> `herdr plugin link` 只登记本地目录、**不执行** `[[build]]`，所以先自己编译。
> 若改用 `herdr plugin install fxz2018/wxherd`（从 GitHub 安装），
> 清单里的 `[[build]]` 才会被自动执行。

链接成功后即可用 `prefix+u` 打开设置弹框，或在 herdr 重启时自动拉起守护进程。

可选：给 `prefix+u` 绑定设置动作（写进 herdr 的 `config.toml`）：

```toml
[[keys.command]]
key = "prefix+u"
type = "plugin_action"
command = "wxherd.bridge.settings"
description = "WxHerd 微信桥设置"
```

### 方式二：只用命令行

> 下面用 `./target/release/wxherd`，把 `target/release` 加入 PATH 后可直接写 `wxherd`

```bash
./target/release/wxherd init      # 生成 ~/.wxherd/config.toml 与状态目录
./target/release/wxherd login     # 终端显示二维码，微信扫码绑定
./target/release/wxherd doctor    # 自检：配置 / herdr 连通 / 微信绑定 / AI 端点
./target/release/wxherd start     # 后台启动守护进程
```

### 绑定微信

```bash
./target/release/wxherd login
```

终端里会渲染二维码，用微信扫码确认即可。绑定时可能出现「配对码」或二维码过期，
命令会自动刷新并提示重定向。凭据保存在状态目录的 `credentials.json`，
**日志全程脱敏**（token / `api_key` / 手机号自动替换为 `***`）。

## 微信侧用法

绑定后给这个 bot 发消息即可。`/agents` 和每次推送都是「可回复菜单」：

```text
🤖 Herdr Agent 状态（4）

1️⃣ 🚨 FluidBar · pi    等待批准
2️⃣ 🟢 FluidBar · pi2   工作中
3️⃣ ✅ FluidBar · pi3   已完成

回复编号看详情；「编号 + 动作」直接操作，如：1 批准
```

### 精确命令

| 命令 | 作用 | 别名 |
|---|---|---|
| `/help` | 命令帮助 | 帮助 |
| `/agents` | agent 列表（带编号） | `/list`、列表 |
| `/status` | 状态汇总 | 状态 |
| `/read <目标> [行数]` | 读最近输出 | 读取 |
| `/send <目标> <文本>` | 向 agent 输入文本并回车 | `/reply`、发送 |
| `/approve <目标>` | 批准（输入 `y`） | 批准 |
| `/trust <目标>` | 信任（输入 `trust, always allow`） | 信任 |
| `/deny <目标>` | 拒绝（输入 `n`） | 拒绝 |
| `/interrupt <目标>` | 中断（Ctrl+C） | `/stop`、中断 |
| `/rules` | 列出已学规则 | 规则 |
| `/forget <序号>` | 删除学习规则 | 忘掉 |
| `/debug` | 守护进程 / 链路诊断 | 诊断 |

### 数字快捷回复

- **裸数字** `3` → 展开第 3 个 agent 的详情卡（状态 + 最近输出 + 操作菜单）
- **「编号 + 动作」** `1 批准` → 直接执行
- **pane ID 直达** `w8:p3 批准` → 菜单重排也不会错
- **`N 全文`** → 最后一轮回复的完整原文（超长自动分片）

### 口语直达（规则层，零 AI 调用）

```
批准第二个
看看 FluidBar 在干什么
中断第一个
```

未命中规则时才走 AI 意图层（需配 `[ai]`）；执行成功的新说法自动沉淀为规则，
**同样的说法第二次起不再调用 AI**。

### 设置命令

| 说法 | 效果 |
|---|---|
| `通知 off` / `通知 on` | 关 / 开全部推送 |
| `静音1小时` / `静音到9点` | 临时免打扰 |
| `规则清空` | 清空学习规则（需回复「确认」） |

## 配置

配置文件在状态目录（默认 `~/.wxherd/config.toml`）。首次 `wxherd init` 生成，
也可用 `prefix+u` 设置弹框可视化修改。**所有字符串值支持 `${ENV_NAME}` 环境变量引用。**

状态目录解析顺序：`--state-dir` → `WXHERD_STATE_DIR` → `HERDR_STATE_DIR` → `~/.wxherd`。

```toml
# ── 基础 ──────────────────────────────────────────
herdr_bin = "herdr"                # 插件运行时优先用 HERDR_BIN_PATH
read_lines = 40                    # /read 默认行数
poll_interval_seconds = 2          # 状态扫描间隔
auto_start = true                  # 跟随 herdr 启动
auto_restart = true                # 崩溃自动重启
herdr_gone_exit_minutes = 5        # herdr 不可达多久后退出（0 = 常驻）

# ── 通知 ──────────────────────────────────────────
[notify]
blocked = true                     # agent 等审批时推送
done = true                        # 任务完成时推送
notify_when_focused = true         # false = 你正看着的 pane 不打扰
cooldown_seconds = 60              # 同一 pane 推送冷却
min_done_duration_seconds = 10     # 短于此耗时的任务不推（0 = 全推）
quiet_hours = []                   # 免打扰时段，如 ["22:00-08:00"]
blocked_delay_seconds = 10         # 阻塞推送延迟，等 agent 自动批准机制先跑

# ── 微信通道 ──────────────────────────────────────
[wechat]
bot_agent = "wxherd"               # 观测标识，仅日志归因
long_poll_seconds = 35             # 长轮询超时
max_command_bytes = 4000           # 命令长度上限
max_send_bytes = 2000              # /send 文本上限
media_dir = "media"                # 入站媒体目录（相对状态目录）

# ── AI 意图层（可选，不配则只用规则层）────────────
[ai]
enabled = true
base_url = "https://open.bigmodel.cn/api/paas/v4"   # 任意 OpenAI 兼容端点
model = "glm-4-flash"
api_key = "${WXHERD_AI_KEY}"       # 推荐环境变量，不落盘
timeout_seconds = 180              # 慢端点给足时间
min_confidence = 0.7               # 低于此置信度回确认菜单
max_learned_rules = 500            # 学习规则上限
rule_match_threshold = 0.9         # 规则命中相似度阈值
thinking_notice = true             # 调 AI 前先回「⏳ 正在理解你的意思」

# ── 输入状态指示 ──────────────────────────────────
[typing]
enabled = true                     # 处理命令期间显示「正在输入…」
```

## CLI 子命令

| 子命令 | 用途 |
|---|---|
| `init` | 生成默认 config.toml 与状态目录 |
| `login` | 终端扫码绑定微信（含配对码 / 过期刷新 / 重定向） |
| `serve` | 前台运行守护进程 |
| `start` / `stop` / `status` | 后台守护进程管理 |
| `doctor` | 配置、herdr 连通、微信链路、AI 端点检测 |
| `send-test` | 给绑定账号发测试文本 |
| `send-image <PATH> [说明]` | 上传并发送图片 |
| `send-file <PATH> [说明]` | 上传并发送文件 |
| `ai-test "<文本>"` | 直接测试 AI 意图解析（打印 prompt / 原始响应 / 解析结果） |
| `event` | herdr 事件入口（由清单 `[[events]]` 调用） |
| `startup-hook` | 跟随 herdr 启动的拉起钩子（由清单 `[[startup]]` 调用） |
| `settings-tui` | 设置弹框 TUI |
| `open-settings` | 打开设置弹框 pane（`prefix+u` 快捷键入口） |

## 架构

依赖方向严格单向，`ilink` 协议层可独立阅读：

```
main.rs          CLI 入口与子命令分发
  └─ daemon.rs   收消息循环（长轮询）+ 通知循环（状态 diff）+ 单实例锁 + 自愈
       └─ bridge.rs   目标解析、命令路由、Markdown 排版、终端画面清洗
            ├─ herdr.rs   herdr CLI 封装 / 会话 jsonl 读取 / 进程探活
            └─ ilink/     iLink 协议（登录、长轮询、发送、AES-128-ECB、CDN）
intent/          四级意图流水线：命令 → 规则 → 学习规则 → AI 兜底
tui.rs           prefix+u 设置弹框（ratatui）
```

意图四级流水线是省成本的关键：精确命令与内置规则命中时**完全不调用 AI**，
只有全新说法才走 AI，成功后沉淀为学习规则（`learned-rules.json`）。

### 状态目录内容

| 文件 | 用途 |
|---|---|
| `config.toml` | 配置 |
| `credentials.json` | 微信绑定凭据（**勿分享**） |
| `sync.json` | 长轮询游标 |
| `runtime.json` | 心跳与失败计数 |
| `notify-state.json` | 通知去重状态（event 与轮询双路径原子 claim） |
| `learned-rules.json` | 学习到的规则 |
| `daemon.log` | 守护进程日志（自动脱敏） |
| `media/` | 入站媒体落盘目录 |

## 安全与隐私

- **密钥不落盘**：`api_key` 支持 `${ENV_NAME}` 引用，推荐用环境变量注入。
- **日志脱敏**：`src/redact.rs` 在写日志前统一抹掉 token、密钥、手机号。
- **凭据隔离**：`credentials.json` 与状态文件只存本地状态目录，不进版本库。
- **单实例保证**：`daemon.lock` 文件锁防止两个守护进程抢同一个 bot 连接。
- **本地优先**：除微信协议本身与你显式配置的 AI 端点外，不向任何第三方发送数据。

## 已知限制

- 微信侧 text 消息**没有按钮、标签页或交互组件**（协议限制），因此所有交互都是
  「编号选项 + 数字回复」的文本形式。
- 「末次输出」依赖 agent 自己上报的会话 jsonl 路径（`herdr agent list` 的
  `agent_session`）。目前适配 pi 与 Claude Code 的 jsonl 格式；其他 agent 若格式特殊需补分支。
- 终端画面回读有硬上限（约 60–75 行），需要全文时用 `N 全文` 读会话文件。
- **macOS / Linux 未做真机运行验证**：代码路径已实现且 CI 编译/测试通过，
  但作者主要在 Windows 上使用。遇到问题请带 `daemon.log` 提 issue。
- 主动推送依赖 iLink 的 `context_token` 滚动续期；守护进程重启后需你先发一条消息
  重新播种 token。长时间无往来若被平台节流（`ret=-2`），会退避等待而不会疯狂重试。

## 风险声明

> ⚠️ 本项目使用**个人微信的 iLink bot 协议**接入，**不是**微信官方开放接口。
> 这类接入方式可能违反微信的服务条款，存在账号被限制的风险。
> 请**仅在你自己拥有并愿意承担风险的账号上使用**，不要用于任何批量、营销或
> 骚扰用途。作者不对使用本项目导致的任何账号处罚、数据丢失或其它后果负责。
>
> 项目与腾讯、微信、Herdr 官方均无关联。

## 开发

```bash
cargo test                    # 149 个单元测试
cargo clippy --all-targets    # 零警告基线
cargo fmt --all               # 提交前格式化
```

### 发布新版本

推一个 `v*` tag 即可，CI 会自动编译四平台二进制并挂到 Release：

```bash
git tag v0.1.0
git push origin v0.1.0
```

协议细节、AES / CDN 处理集中在 `src/ilink/`。改行为请带测试——
这个项目一半的测试是踩坑换来的回归用例。

欢迎 issue 与 PR。

## License

[MIT](LICENSE)
