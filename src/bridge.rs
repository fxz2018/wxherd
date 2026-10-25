//! 业务桥接：微信命令解析 → herdr 操作、通知文案（Markdown + 终端清洗）、
//! 通知去重引擎、入站媒体落盘。不接触协议细节与 HTTP（媒体下载经 ilink 客户端）。

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::{Config, NotifyConfig, RuntimeState, StatePaths, SyncState};
use crate::herdr::{self, PaneSnapshot};
use crate::ilink::client::IlinkClient;
use crate::ilink::types::Message;
use crate::intent::ai::{self, AiClient, AiReply};
use crate::intent::learn::LearnBook;
use crate::intent::rules::extract_named_target;
use crate::intent::{self, rules, Intent};
use crate::redact::redact_text;

/// 层①命令帮助文案（中文硬编码）。
pub const HELP_TEXT: &str = "🤖 WxHerd 命令\n\n\
/agents  agent 列表（别名：列表）\n\
/status  状态汇总\n\
/read <目标> [行数]  读最近输出\n\
/send <目标> <文本>  发送文本并回车\n\
/approve <目标>  批准（输入 y）\n\
/trust <目标>  信任（trust, always allow）\n\
/deny <目标>  拒绝（输入 n）\n\
/interrupt <目标>  中断（Ctrl+C）\n\
/rules  已学习规则\n\
/forget <序号>  忘掉规则\n\
/debug  链路诊断\n\
\n\
目标可为：#序号 / pane ID / 项目名\n\
快捷回复：数字看详情；「数字 + 动作」直接操作，如：1 批准\n\
\n\
设置（无需斜杠）：\n\
通知 on/off  开关全部推送\n\
静音1小时 / 静音到9点  临时免打扰\n\
规则清空  清空学习规则（需回复 确认）";

// ═══════════════════════════════════════════════════════════════════════
// 终端清洗
// ═══════════════════════════════════════════════════════════════════════

/// 去除 ANSI 转义序列：CSI（ESC[...final）、OSC（ESC]...BEL 或 ESC\）、两字符转义。
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // CSI：消费到 0x40..=0x7E 的 final byte
                for n in chars.by_ref() {
                    if ('\u{40}'..='\u{7E}').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC：消费到 BEL 或 ESC \
                loop {
                    match chars.next() {
                        None | Some('\x07') => break,
                        Some('\x1b') => {
                            chars.next();
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Some(_) => {
                // 两字符转义（如 ESC =）；带中间字节的（如 ESC ( B）消费到 final byte
                for c in chars.by_ref() {
                    if ('\u{30}'..='\u{7E}').contains(&c) {
                        break;
                    }
                }
            }
            None => {}
        }
    }
    out
}

fn is_box_char(c: char) -> bool {
    matches!(
        c,
        '─' | '│'
            | '┌'
            | '┐'
            | '└'
            | '┘'
            | '├'
            | '┤'
            | '┬'
            | '┴'
            | '┼'
            | '╭'
            | '╮'
            | '╰'
            | '╯'
            | '═'
            | '║'
            | '╔'
            | '╗'
            | '╚'
            | '╝'
            | '╠'
            | '╣'
            | '╦'
            | '╩'
            | '╬'
            | '━'
            | '┄'
            | '┆'
    )
}

/// 是否 agent TUI 的装饰行（状态栏/进度条/分支栏等），非真实命令输出。
/// pi/claude 类全屏 TUI 的底部 2-4 行都是这类，若取作「末次输出」
/// 用户看到的就是 MCP/进度条而不是任务结果。
pub fn is_tui_chrome_line(line: &str) -> bool {
    let l = line.trim();
    if l.is_empty() {
        return true;
    }
    // 底部状态栏特征
    if l.contains("MCP:") || l.contains("servers enabled") {
        return true;
    }
    // pi 性能统计栏（TPS/TTFT/token 统计），实测出现在完成推送末行
    if l.contains("TPS") && l.contains("tok/s") || l.contains("TTFT") {
        return true;
    }
    // 分支/组/模型栏（如 `· Magpie · group/never-stuck · high`、
    // `· Doulor · workbuddy-deepseek-v4.1-flash · high`）——不靠字面量，
    // 用结构：以 · 开头且含 ≥2 个 · 分隔（不同项目/模型/分支组合千变万化，
    // 靠字面匹配必然漏）。
    if l.starts_with('·') && l.matches('·').count() >= 2 {
        return true;
    }
    // 同类栏偶尔不以 · 开头（首列被裁切/图标替换）：以 · 分隔 + 末尾是优先级词
    {
        let seps = l.matches('·').count();
        let ends_priority = ["high", "low", "medium", "mid"]
            .iter()
            .any(|p| l.ends_with(p));
        if seps >= 2 && ends_priority {
            return true;
        }
    }
    // 进度条与 token 统计
    if l.contains('█') && l.contains('░') {
        return true;
    }
    if l.contains("ctx") && l.contains('%') {
        return true;
    }
    // 路径+计时栏（如 `E:\dir 1.99.1 working 1m 4s … 18.5% · 194k/1.0M`）
    if l.contains('%') && (l.contains("working") || l.contains("blocked") || l.contains("idle")) {
        return true;
    }
    // pi 的工作指示框（Working / Waiting 标题框剥边后剩余内容）
    if l == "Working"
        || l.starts_with("⠋")
        || l.starts_with("⠙")
        || l.starts_with("⠹")
        || l.starts_with("⠸")
        || l.starts_with("⠼")
        || l.starts_with("⠴")
        || l.starts_with("⠦")
        || l.starts_with("⠧")
    {
        return true;
    }
    false
}

/// 终端画面清洗：去 ANSI → 去边框字符包裹 → 去空行/纯边框行/TUI 装饰行
/// → 保留末尾 max_lines 行。
pub fn clean_terminal_output(input: &str, max_lines: usize) -> String {
    let stripped = strip_ansi(input);
    let mut kept: Vec<&str> = Vec::new();
    for raw in stripped.lines() {
        let line = raw.trim_end();
        let content = line.trim_matches(|c: char| is_box_char(c) || c.is_whitespace());
        if content.is_empty() || is_tui_chrome_line(content) {
            continue;
        }
        kept.push(content);
    }
    let start = kept.len().saturating_sub(max_lines.max(1));
    kept[start..].join("\n")
}

// ═══════════════════════════════════════════════════════════════════════
// 状态显示与 agent 菜单
// ═══════════════════════════════════════════════════════════════════════

pub fn status_zh(status: &str) -> &'static str {
    match status {
        "blocked" => "等待批准",
        "working" => "工作中",
        "idle" => "空闲",
        "done" => "已完成",
        _ => "未知",
    }
}

pub fn status_icon(status: &str) -> &'static str {
    match status {
        "blocked" => "🚨",
        "working" => "🟢",
        "idle" | "done" => "✅",
        _ => "⚪",
    }
}

/// 菜单序号 emoji：1️⃣..9️⃣ 🔟，超出退回 "N."。
pub fn num_emoji(n: usize) -> String {
    match n {
        1..=9 => format!("{}️⃣", n),
        10 => "🔟".to_string(),
        _ => format!("{n}."),
    }
}

/// 参与菜单的 agent 条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    /// 菜单序号（1 起始，blocked 优先排序后回填）。
    pub index: usize,
    pub pane_id: String,
    pub agent: String,
    pub status: String,
    /// 终端标题（strip 后）。
    pub title: String,
    /// 项目名：cwd 最后一段。
    pub project: String,
    pub cwd: String,
    pub focused: bool,
    /// pi 会话文件（jsonl）路径：done 推送取真实回复用。非 pi / 未提供时为 None。
    pub session_file: Option<String>,
}

impl AgentEntry {
    pub fn display_name(&self) -> String {
        format!("{} · {}", self.project, self.agent)
    }
}

/// 从 pane 快照提取有 agent 的条目（保持 herdr 顺序）。
pub fn collect_agents(panes: &[PaneSnapshot]) -> Vec<AgentEntry> {
    panes
        .iter()
        .filter(|p| p.agent.is_some())
        .map(|p| AgentEntry {
            index: 0,
            pane_id: p.pane_id.clone(),
            agent: p.agent.clone().unwrap_or_default(),
            status: p.agent_status.clone().unwrap_or_else(|| "unknown".into()),
            title: p.terminal_title_stripped.clone().unwrap_or_default(),
            project: p
                .cwd
                .as_deref()
                .map(|c| {
                    c.trim_end_matches(['\\', '/'])
                        .rsplit(['\\', '/'])
                        .next()
                        .unwrap_or(c)
                        .to_string()
                })
                .unwrap_or_default(),
            cwd: p.cwd.clone().unwrap_or_default(),
            focused: p.focused,
            session_file: p.agent_session.as_ref().and_then(|s| s.value.clone()),
        })
        .collect()
}

fn status_rank(status: &str) -> u8 {
    match status {
        "blocked" => 0,
        "working" => 1,
        "idle" | "done" => 2,
        _ => 3,
    }
}

/// 菜单定稿：与 herdr 侧栏排序对齐后回填序号。
///
/// herdr 的 agent 侧栏排序由 `ui.agent_panel_sort` 控制：
/// - `"spaces"`（默认）：workspace → tab 顺序，即 `herdr pane list` 的返回序；
/// - `"priority"`：状态优先（blocked → working → done → idle），同状态内保持原序。
///
/// 读用户 herdr 配置决定按哪种序编号，微信菜单与屏幕侧栏一致，
/// 避免「编号对不上、不知道发给谁」。
pub fn finalize_menu(mut entries: Vec<AgentEntry>) -> Vec<AgentEntry> {
    if agent_panel_sort_is_priority() {
        entries.sort_by_key(|e| status_rank(&e.status));
    }
    // spaces 模式（含未知值）：保持 herdr pane list 返回序，不重排
    for (i, e) in entries.iter_mut().enumerate() {
        e.index = i + 1;
    }
    entries
}

/// 读 herdr 配置中 `ui.agent_panel_sort`；返回 true 表示 priority 模式。
fn agent_panel_sort_is_priority() -> bool {
    // 配置路径：HERDR_CONFIG_DIR/config.toml，或回退 %APPDATA%/herdr → D:/herdr-data 链接
    let candidates = [
        std::env::var("HERDR_CONFIG_DIR")
            .ok()
            .map(|d| std::path::PathBuf::from(d).join("config.toml")),
        dirs_config_path(),
    ];
    for path in candidates.into_iter().flatten() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("agent_panel_sort") {
                    let v = v.trim().trim_start_matches('=');
                    let v = v.trim().trim_matches(['"', '\'']);
                    return v.eq_ignore_ascii_case("priority");
                }
            }
        }
    }
    false // herdr 默认 "spaces"
}

/// %APPDATA%\herdr\config.toml（Windows）。
fn dirs_config_path() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA").ok().map(|d| {
        std::path::PathBuf::from(d)
            .join("herdr")
            .join("config.toml")
    })
}

// ═══════════════════════════════════════════════════════════════════════
// 目标解析（#N / pane ID / 项目名 / 模糊包含）
// ═══════════════════════════════════════════════════════════════════════

/// 按四选一规则解析目标；Err 为用户可读的中文错误。
pub fn resolve_target(menu: &[AgentEntry], target: &str) -> Result<AgentEntry, String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("请指定目标，如：/read 1 或 /read 项目名".into());
    }
    if menu.is_empty() {
        return Err("当前没有活跃 agent，发送 /agents 查看".into());
    }
    // #N 或纯数字 → 菜单序号
    let num = t.strip_prefix('#').unwrap_or(t);
    if let Ok(n) = num.parse::<usize>() {
        if n == 0 {
            return Err("序号从 1 开始".into());
        }
        return menu.get(n - 1).cloned().ok_or_else(|| {
            format!(
                "序号 {n} 超出范围：当前共 {} 个 agent，发送 /agents 查看",
                menu.len()
            )
        });
    }
    // last / 最后 → 菜单末尾（列表已按 blocked 优先排序，末尾最不紧急）
    if t.eq_ignore_ascii_case("last") || t == "最后" {
        return menu
            .last()
            .cloned()
            .ok_or_else(|| "当前没有活跃 agent".into());
    }
    // pane ID 精确
    if let Some(e) = menu.iter().find(|e| e.pane_id == t) {
        return Ok(e.clone());
    }
    // 项目名 / agent 名 / 标题精确匹配（大小写不敏感）
    let tl = t.to_lowercase();
    let field = |e: &AgentEntry| -> Vec<String> {
        vec![
            e.project.to_lowercase(),
            e.agent.to_lowercase(),
            e.title.to_lowercase(),
        ]
    };
    let exact: Vec<&AgentEntry> = menu.iter().filter(|e| field(e).contains(&tl)).collect();
    if exact.len() == 1 {
        return Ok(exact[0].clone());
    }
    // 大小写不敏感包含匹配
    let fuzzy: Vec<&AgentEntry> = menu
        .iter()
        .filter(|e| {
            field(e)
                .iter()
                .chain(std::iter::once(&e.pane_id.to_lowercase()))
                .any(|f| f.contains(&tl))
        })
        .collect();
    match fuzzy.len() {
        1 => Ok(fuzzy[0].clone()),
        0 => Err(format!("未找到目标「{t}」，发送 /agents 查看列表")),
        _ => {
            let names = fuzzy
                .iter()
                .map(|e| format!("#{} {}", e.index, e.display_name()))
                .collect::<Vec<_>>()
                .join("、");
            Err(format!("匹配到多个，请更具体：{names}"))
        }
    }
}

/// 目标缺省时按上下文：最近交互 → blocked 最优先（menu 已排序取首项）。
pub fn default_target(
    menu: &[AgentEntry],
    last_target: Option<&str>,
) -> Result<AgentEntry, String> {
    if menu.is_empty() {
        return Err("请先发 /agents 查看 agent 列表".into());
    }
    if let Some(p) = last_target {
        if let Some(e) = menu.iter().find(|e| e.pane_id == p) {
            return Ok(e.clone());
        }
    }
    Ok(menu[0].clone())
}

// ═══════════════════════════════════════════════════════════════════════
// 层①命令与数字快捷回复解析
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Agents,
    Status,
    Debug,
    Read {
        target: Option<String>,
        lines: Option<u32>,
    },
    /// 全文模式：读 pi 会话文件最近 N 条 assistant 消息（不受终端快照 60 行限制）
    ReadFull {
        target: Option<String>,
        count: Option<u32>,
    },
    Send {
        target: Option<String>,
        text: String,
    },
    Approve {
        target: Option<String>,
    },
    Trust {
        target: Option<String>,
    },
    Deny {
        target: Option<String>,
    },
    Interrupt {
        target: Option<String>,
    },
    Rules,
    Forget {
        index: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickAction {
    Approve,
    Trust,
    Deny,
    Interrupt,
    Read,
    /// 「N 发送 文本」→ 把文本发给第 N 个 agent。
    Send(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickReply {
    /// 裸数字 N → 第 N 个 agent 详情卡。
    Detail(usize),
    /// 「N 全文」→ 读会话文件最近几条 assistant 消息全文。
    DetailFull(usize),
    /// 序号或 pane ID + 动作词 → 直接执行。
    Act(TargetRef, QuickAction),
}

fn split_cmd(text: &str) -> (&str, &str) {
    match text.find(char::is_whitespace) {
        Some(i) => (&text[..i], text[i..].trim()),
        None => (text, ""),
    }
}

/// 解析层①精确命令（含中文别名）。未命中返回 None（交给后续层级）。
pub fn parse_command(input: &str) -> Option<Command> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    let (head, rest) = match text.strip_prefix('/') {
        Some(stripped) => split_cmd(stripped),
        None => split_cmd(text),
    };
    let rest = rest.trim();
    match head.to_lowercase().as_str() {
        "help" | "帮助" | "菜单" => Some(Command::Help),
        "agents" | "list" | "列表" => Some(Command::Agents),
        "status" | "状态" => Some(Command::Status),
        "debug" | "诊断" => Some(Command::Debug),
        "read" | "读取" | "看" | "查看" => {
            // 「read 4 full」/「查看 4 全文」→ 全文模式
            let rest_l = rest.to_lowercase();
            if rest_l.ends_with("full") || rest_l.ends_with("全文") {
                let head = rest_l
                    .trim_end_matches("full")
                    .trim_end_matches("全文")
                    .trim()
                    .trim_start_matches("read")
                    .trim();
                let target = if head.is_empty() {
                    None
                } else {
                    Some(head.to_string())
                };
                return Some(Command::ReadFull {
                    target,
                    count: None,
                });
            }
            let parts: Vec<&str> = rest.split_whitespace().collect();
            match parts.as_slice() {
                [] => Some(Command::Read {
                    target: None,
                    lines: None,
                }),
                [one] => Some(Command::Read {
                    target: Some((*one).into()),
                    lines: None,
                }),
                [t, l, ..] if l.parse::<u32>().is_ok() => Some(Command::Read {
                    target: Some((*t).into()),
                    lines: l.parse::<u32>().ok(),
                }),
                [t, ..] => Some(Command::Read {
                    target: Some((*t).into()),
                    lines: None,
                }),
            }
        }
        "send" | "reply" | "发送" => {
            let (target, body) = split_cmd(rest);
            Some(Command::Send {
                target: if target.is_empty() {
                    None
                } else {
                    Some(target.into())
                },
                text: body.to_string(),
            })
        }
        "approve" | "批准" | "同意" => Some(
            target_only(rest)
                .map(|t| Command::Approve { target: t })
                .ok()?,
        ),
        "trust" | "信任" => Some(
            target_only(rest)
                .map(|t| Command::Trust { target: t })
                .ok()?,
        ),
        "deny" | "拒绝" => Some(
            target_only(rest)
                .map(|t| Command::Deny { target: t })
                .ok()?,
        ),
        "interrupt" | "stop" | "中断" | "停止" => Some(
            target_only(rest)
                .map(|t| Command::Interrupt { target: t })
                .ok()?,
        ),
        "rules" | "规则" => {
            // 带后缀的「规则清空」等交给设置层处理
            if rest.is_empty() {
                Some(Command::Rules)
            } else {
                None
            }
        }
        "forget" | "忘掉" => {
            let n: usize = rest.trim().trim_start_matches('#').parse().ok()?;
            Some(Command::Forget { index: n })
        }
        _ => None,
    }
}

/// 动作命令的目标参数：允许缺省（None → 上下文默认目标）。
fn target_only(rest: &str) -> Result<Option<String>, ()> {
    Ok(if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    })
}

fn classify_action(word: &str) -> Option<QuickAction> {
    match word.to_lowercase().as_str() {
        "批准" | "同意" | "ok" | "通过" | "yes" | "y" => Some(QuickAction::Approve),
        "拒绝" | "不行" | "不对" | "no" | "n" => Some(QuickAction::Deny),
        "信任" | "trust" => Some(QuickAction::Trust),
        "中断" | "停止" | "停下" | "打断" | "stop" => Some(QuickAction::Interrupt),
        "看" | "看看" | "看下" | "输出" | "日志" | "进度" | "状态" | "干什么" | "干啥" | "跑啥"
        | "结果" | "read" => Some(QuickAction::Read),
        // 「N 发送 …」/「N send …」→ Send(剩余全部文本)；QuickReply 解析层负责传内容
        "send" | "发送" | "发" | "说" | "告诉" => Some(QuickAction::Send(String::new())),
        _ => None,
    }
}

/// 快捷动作的目标：菜单序号或 pane ID（pane ID 不受菜单重排影响）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetRef {
    Index(usize),
    PaneId(String),
}

/// 解析快捷回复：裸数字 N → 详情卡；「N 动作词 [文本…]」→ 直接执行；
/// 「pane_id 动作」（如 `w8:p3 批准`）与数字等价但不受菜单重排影响。
pub fn parse_quick_reply(input: &str) -> Option<QuickReply> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(n) = text.parse::<usize>() {
        return Some(QuickReply::Detail(n));
    }
    // 「N 全文」/「N more」→ 会话文件全文模式（不受终端快照 60 行限制）
    {
        let lower = text.to_lowercase();
        for suffix in ["全文", "more", "full"] {
            if let Some(head) = lower.strip_suffix(suffix) {
                if let Ok(n) = head.trim().parse::<usize>() {
                    return Some(QuickReply::DetailFull(n));
                }
            }
        }
    }
    // 连写兼容：「发送N+内容」/「发送N：内容」（如 发送1问我中午吃什么）。
    // 动作词在前，后面紧跟序号与内容，没有空格分隔。
    if let Some(qr) = parse_compact_send(text) {
        return Some(qr);
    }
    let mut parts = text.split_whitespace();
    let first = parts.next()?;
    let first_lower = first.to_ascii_lowercase();
    let target = if first_lower.starts_with('w') && first_lower.contains(':') && first.len() > 2 {
        TargetRef::PaneId(first.to_string())
    } else {
        let n = first.trim_start_matches('#').parse::<usize>().ok()?;
        TargetRef::Index(n)
    };
    let action_word = parts.next()?;
    let rest = text
        .splitn(3, char::is_whitespace)
        .nth(2)
        .unwrap_or_default()
        .trim()
        .to_string();
    match classify_action(action_word)? {
        // 带文本的动作：rest 为空视为无效（send 没内容就不该走这）
        QuickAction::Send(_) if rest.is_empty() => None,
        QuickAction::Send(_) => Some(QuickReply::Act(target, QuickAction::Send(rest))),
        // 「1 说 …」之类有两个以上 token 的纯动作交由规则层/AI 层处理
        _a if !rest.is_empty() => None,
        a => Some(QuickReply::Act(target, a)),
    }
}

/// 连写发送：动作词 + 序号 + 内容无空格分隔，如：
/// `发送1问我中午吃什么` / `send2run tests` / `发送w8:p3:你好`。
/// 返回 None 表示不是这种格式。
fn parse_compact_send(text: &str) -> Option<QuickReply> {
    // 识别中文动作词前缀（多字节，不能用 ascii 前缀匹配）
    for prefix in ["发送", "发", "说", "告诉"] {
        if let Some(after) = text.strip_prefix(prefix) {
            if after.is_empty() {
                return None;
            }
            // 目标：pane ID（w8:p3 后跟 :或非ID字符）或纯数字序号
            let (target, consumed) = if starts_with_pane_id(after) {
                // pane ID 到首个非 [alnum/:] 字符为止，再剥掉尾部单个 ':' 分隔符
                let mut end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == ':'))
                    .unwrap_or(after.len());
                if after[..end].ends_with(':') {
                    end -= 1;
                }
                (TargetRef::PaneId(after[..end].to_string()), end)
            } else {
                let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                if digits.is_empty() {
                    return None;
                }
                (TargetRef::Index(digits.parse().ok()?), digits.len())
            };
            let rest = after[consumed..]
                .trim_start_matches([':', '：', ' '])
                .trim()
                .to_string();
            if rest.is_empty() {
                return None;
            }
            return Some(QuickReply::Act(target, QuickAction::Send(rest)));
        }
    }
    None
}

/// 是否 pane ID 开头（wN:pM 形式，大小写不敏感）。
fn starts_with_pane_id(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.starts_with('w')
        && lower.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit())
        && lower.contains(':')
}

// ═══════════════════════════════════════════════════════════════════════
// 设置命令（微信侧远程管理）

/// 设置命令（通知开关、静音、规则清空及其确认）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCommand {
    NotifyOn,
    NotifyOff,
    Mute { until_ms: u64 },
    Unmute,
    RulesClear,
    ConfirmClear,
    Cancel,
}

/// 解析设置命令。未命中返回 None。
pub fn parse_settings_command(input: &str) -> Option<SettingsCommand> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    if text == "确认" {
        return Some(SettingsCommand::ConfirmClear);
    }
    if text == "取消" {
        return Some(SettingsCommand::Cancel);
    }
    if let Some(rest) = text.strip_prefix("通知") {
        let rest = rest.trim().to_lowercase();
        return match rest.as_str() {
            "on" | "开" | "打开" | "开启" => Some(SettingsCommand::NotifyOn),
            "off" | "关" | "关闭" | "关掉" => Some(SettingsCommand::NotifyOff),
            _ => None,
        };
    }
    if matches!(text, "规则清空" | "清空规则" | "清空学习规则") {
        return Some(SettingsCommand::RulesClear);
    }
    parse_mute(text)
}

/// 解析静音命令：「静音1小时」「静音两小时」「静音到9点」「静音到9点半」「取消静音」；
/// 裸「静音」默认 1 小时。
fn parse_mute(text: &str) -> Option<SettingsCommand> {
    if text == "取消静音" {
        return Some(SettingsCommand::Unmute);
    }
    let rest = text.strip_prefix("静音")?.trim_start();
    if let Some(after) = rest.strip_prefix('到') {
        // 「到9点」/「到九点半」→ 本地下一个该时刻
        let after = after.trim_start();
        let idx = after.find('点')?;
        let after_dot = &after[idx + '点'.len_utf8()..];
        let minute = if after_dot.trim_start().starts_with('半') {
            30
        } else {
            0
        };
        let hour = rules::parse_chinese_number(after[..idx].trim())?;
        if hour > 23 {
            return None;
        }
        Some(SettingsCommand::Mute {
            until_ms: next_occurrence_ms(hour, minute),
        })
    } else {
        let hours = if rest.is_empty() {
            1
        } else {
            let n = rules::parse_chinese_number(rest.strip_suffix("小时")?.trim())?;
            if !(1..=72).contains(&n) {
                return None;
            }
            n
        };
        let now = chrono::Local::now().timestamp_millis().max(0) as u64;
        Some(SettingsCommand::Mute {
            until_ms: now + hours as u64 * 3_600_000,
        })
    }
}

/// 本地时间下一个 hour:minute 的毫秒时间戳（已过则取明天）。
fn next_occurrence_ms(hour: u32, minute: u32) -> u64 {
    use chrono::{Duration, Local, TimeZone};
    let now = Local::now();
    let naive = now
        .date_naive()
        .and_hms_opt(hour, minute, 0)
        .unwrap_or_else(|| now.naive_local());
    let mut target = Local.from_local_datetime(&naive).single().unwrap_or(now);
    if target <= now {
        target += Duration::hours(24);
    }
    target.timestamp_millis().max(0) as u64
}

// 通知文案（Markdown）
// ═══════════════════════════════════════════════════════════════════════

/// agent 列表菜单（可回复菜单：裸数字 / 数字+动作）。
pub fn fmt_agents_menu(entries: &[AgentEntry]) -> String {
    let mut out = format!("🤖 Herdr Agent 状态（{}）\n\n", entries.len());
    for e in entries {
        out.push_str(&format!(
            "{} {} {}\t{}\n",
            num_emoji(e.index),
            status_icon(&e.status),
            e.display_name(),
            status_zh(&e.status)
        ));
    }
    out.push_str("\n回复编号看详情；「编号 全文」看完整回复；「编号 + 动作」直接操作，如：1 批准");
    out
}

/// agent 详情卡（最近输出 + 操作菜单）。
pub fn fmt_detail(entry: &AgentEntry, recent_output: &str) -> String {
    let mut out = format!(
        "🤖 #{} {}\n\n状态：{}\npane：`{}`\n项目：{}\n",
        entry.index,
        entry.display_name(),
        status_zh(&entry.status),
        entry.pane_id,
        entry.cwd,
    );
    if entry.status == "blocked" {
        out.push_str("\n⏳ agent 正在等待批准");
    }
    out.push_str(&format!("\n\n**最近输出**\n```\n{recent_output}\n```"));
    out.push_str(&format!(
        "\n\n**操作**\n`{} 批准` · `{} 拒绝` · `{} 中断` · `{} 看`",
        entry.index, entry.index, entry.index, entry.index
    ));
    out
}

/// 状态汇总。
pub fn fmt_status_summary(entries: &[AgentEntry]) -> String {
    let (mut b, mut w, mut i, mut u) = (0usize, 0, 0, 0);
    for e in entries {
        match e.status.as_str() {
            "blocked" => b += 1,
            "working" => w += 1,
            "idle" | "done" => i += 1,
            _ => u += 1,
        }
    }
    let mut out = format!("📊 状态汇总（共 {} 个 agent）\n\n", entries.len());
    out.push_str(&format!("🚨 等待批准：{b}\n🟢 工作中：{w}\n✅ 空闲：{i}\n"));
    if u > 0 {
        out.push_str(&format!("⚪ 未知：{u}\n"));
    }
    let blocked: Vec<&AgentEntry> = entries.iter().filter(|e| e.status == "blocked").collect();
    if !blocked.is_empty() {
        out.push_str("\n**需要处理**\n");
        for e in &blocked {
            out.push_str(&format!(
                "{} {} · `{}`\n",
                num_emoji(e.index),
                e.display_name(),
                e.pane_id
            ));
        }
        out.push_str("\n回复编号看详情，或直接「编号 批准」");
    }
    out
}

/// 从 agent 输出中提取编号选项列表（编号→原文）。
/// 识别 `1. xx` / `1) xx` / `1、xx` / `[1] xx` / `1️⃣ xx` 形式，连续段才有效（≥2 项）。
/// 单行是否为编号选项行（`1. xx` / `1、xx` / `[1] xx` / `1️⃣ xx` 等）。
/// 是则返回选项文本。
pub fn is_numbered_choice_line(line: &str) -> Option<String> {
    let l = line.trim();
    let l = l.trim_start_matches(['-', '*', '•']);
    let l = l.trim();
    let after_num = if let Some(rest) = l.strip_prefix('[') {
        rest.split_once(']')
            .and_then(|(n, r)| n.trim().parse::<usize>().ok().map(|_| r.trim().to_string()))
    } else {
        let bytes = l.as_bytes();
        if bytes.first().is_some_and(|b| b.is_ascii_digit()) {
            let sep = l.find(['.', ')', '）', '、', '：', ':']);
            sep.and_then(|i| {
                let num = l[..i].trim();
                // 全部字符都是数字（避免「1. 5 分钟后重试」这类把句中数字当编号）
                if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                    // i 是分隔符起始字节；跳过分隔符（可能多字节）取余下文本
                    let sep_len = l[i..].chars().next().unwrap().len_utf8();
                    l[i + sep_len..].trim().to_string().into()
                } else {
                    None
                }
            })
        } else if l.starts_with("1️⃣")
            || l.starts_with("2️⃣")
            || l.starts_with("3️⃣")
            || l.starts_with("4️⃣")
            || l.starts_with("5️⃣")
            || l.starts_with("6️⃣")
            || l.starts_with("7️⃣")
            || l.starts_with("8️⃣")
            || l.starts_with("9️⃣")
        {
            // keycap emoji：数字字节 + VS16 + 变体选择符，共 7 字节
            Some(l[7..].trim().to_string())
        } else {
            None
        }
    };
    after_num.filter(|t| !t.is_empty())
}

/// 从 agent 输出中提取连续的编号选项列表（≥2 项才算，避免误伤普通输出）。
pub fn extract_numbered_choices(output: &str) -> Vec<String> {
    let mut choices: Vec<String> = Vec::new();
    for raw in output.lines() {
        match is_numbered_choice_line(raw) {
            Some(text) => choices.push(text),
            None => {
                // 非选项行打断连续段
                if !choices.is_empty() {
                    break;
                }
            }
        }
    }
    if choices.len() < 2 {
        return Vec::new();
    }
    choices
}

/// 阻塞推送。同时提取选项列表存入 pending_choices，供用户回数字时翻译。
/// 返回 (推送文本, 提取到的选项列表)。
/// 阻塞推送。questionnaire = 结构化多题问卷（来自会话文件，含全部题目与选项）。
/// 有问卷时列出全部题目（回「题号.选项号」一次答完）；否则回退画面编号选项提取。
/// 返回 (推送文本, 待回答的问卷/选项)。
pub fn fmt_blocked_push(
    entry: &AgentEntry,
    recent_output: &str,
    menu_index: Option<usize>,
    questionnaire: Option<crate::herdr::PendingQuestionnaire>,
) -> (String, Vec<crate::herdr::PendingQuestion>) {
    let mut out = String::from(
        "🚨 **Herdr 需要你**

",
    );
    out.push_str(&format!(
        "{}
",
        entry.display_name()
    ));
    out.push_str(&format!(
        "状态：{}
",
        status_zh(&entry.status)
    ));
    out.push_str(&format!(
        "pane：`{}`
",
        entry.pane_id
    ));
    out.push_str(
        "
**回复即可操作**
",
    );
    if let Some(n) = menu_index {
        out.push_str(&format!(
            "`{n} 批准` 批准
"
        ));
    }
    out.push_str(&format!(
        "`/approve {}` 批准
",
        entry.pane_id
    ));
    out.push_str(&format!(
        "`/send {} 你的指示`
",
        entry.pane_id
    ));

    if let Some(q) = &questionnaire {
        out.push_str(&format!(
            "
**问卷（{} 题）**
",
            q.questions.len()
        ));
        if q.questions.len() > 1 {
            out.push_str(
                "回「题号.选项号」一次答完，如 `1.2 2.1`
",
            );
        } else {
            out.push_str(
                "直接回数字选择
",
            );
        }
        for (qi, question) in q.questions.iter().enumerate() {
            out.push_str(&format!(
                "
【{} {}】{}
",
                qi + 1,
                question.header,
                question.question
            ));
            for (oi, opt) in question.options.iter().enumerate() {
                out.push_str(&format!(
                    "{} {}
",
                    num_emoji(oi + 1),
                    opt.label
                ));
                if !opt.description.is_empty() {
                    out.push_str(&format!(
                        "    {}
",
                        opt.description
                    ));
                }
            }
        }
        out.push_str(
            "
或直接打字回复",
        );
        let questions = q.questions.clone();
        return (out, questions);
    }

    // 无问卷：回退画面提取的单段编号选项
    let choices = extract_numbered_choices(recent_output);
    if !choices.is_empty() {
        out.push_str(
            "
**回答选项**（直接回数字）
",
        );
        for (i, c) in choices.iter().enumerate() {
            out.push_str(&format!(
                "{} {}
",
                num_emoji(i + 1),
                c
            ));
        }
        out.push_str("或直接打字回复");
    }
    out.push_str(&format!(
        "
**最近输出**
```
{recent_output}
```"
    ));
    // 画面提取的选项转成单题问卷结构（统一 translate_reply 的入口）
    let questions = if choices.is_empty() {
        Vec::new()
    } else {
        vec![crate::herdr::PendingQuestion {
            header: String::new(),
            question: String::new(),
            options: choices
                .into_iter()
                .map(|c| crate::herdr::PendingOption {
                    label: c,
                    description: String::new(),
                })
                .collect(),
        }]
    };
    (out, questions)
}
/// 完成推送带菜单序号：首行 emoji 即菜单同号，正文不再重复提示。
pub fn fmt_done_push(entry: &AgentEntry, duration_ms: u64, last_line: &str) -> String {
    let mut out = String::from("✅ **任务完成**\n\n");
    out.push_str(&format!(
        "{} {}\n",
        num_emoji(entry.index),
        entry.display_name()
    ));
    out.push_str(&format!("状态：{}\n", status_zh(&entry.status)));
    out.push_str(&format!("耗时：{}\n", fmt_duration(duration_ms)));
    out.push_str(&format!("pane：`{}`\n", entry.pane_id));
    if !last_line.is_empty() {
        out.push_str(&format!("\n末次输出：{}", truncate_chars(last_line, 80)));
    }
    out
}

/// 秒级时长人性化：`25m 30s` / `2h 05m` / `3d 04h`。
pub fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{}m {:02}s", m, s % 60);
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h {:02}m", m % 60);
    }
    let d = h / 24;
    format!("{d}d {:02}h", h % 24)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 微信单条 Markdown 消息的字节上限（超过需分片）。
pub const MAX_MARKDOWN_BYTES: usize = 3500;

/// 长 Markdown 安全分片：每片 ≤ max_bytes 字节且不切断 UTF-8 字符（M5）。
///
/// - 行边界优先：按行累积，装不下即换片
/// - 单行超过预算时按字符边界（char boundary）硬切，绝不切在多字节字符中间
/// - 跨片的代码块在片尾闭合、下一片开头重开 ``` 围栏，保证微信渲染不乱
/// - 至少返回一片；空文本返回单空串片
pub fn split_markdown_chunks(text: &str, max_bytes: usize) -> Vec<String> {
    if text.len() <= max_bytes {
        return vec![text.to_string()];
    }
    // 预留围栏闭合/重开的字节余量（片尾 "\n```" + 片首 "```" 最多 8 字节）
    let budget = max_bytes.saturating_sub(8).max(1);

    // 展平成行序列；超长行按字符边界硬切为多行（每行 ≤ budget 字节）
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.len() <= budget {
            lines.push(line);
        } else {
            let mut rest = line;
            while rest.len() > budget {
                let mut idx = budget;
                while !rest.is_char_boundary(idx) {
                    idx -= 1;
                }
                let (head, tail) = rest.split_at(idx);
                lines.push(head);
                rest = tail;
            }
            lines.push(rest);
        }
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let mut in_code = false;
    for line in &lines {
        let is_fence = line.trim_start().starts_with("```");
        let need = line.len() + if cur.is_empty() { 0 } else { 1 };
        if cur_len + need > budget && !cur.is_empty() {
            if in_code {
                cur.push_str("\n```");
            }
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
            if in_code {
                cur.push_str("```");
                cur_len = 3;
            }
        }
        if !cur.is_empty() {
            cur.push('\n');
            cur_len += 1;
        }
        if is_fence {
            in_code = !in_code;
        }
        cur.push_str(line);
        cur_len += line.len();
    }
    if in_code {
        cur.push_str("\n```");
    }
    if !cur.is_empty() || chunks.is_empty() {
        chunks.push(cur);
    }
    chunks
}

// ═══════════════════════════════════════════════════════════════════════
// 通知去重引擎（notify-state.json）
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneNotifyState {
    pub last_status: String,
    /// 进入当前状态的时间（ms）。
    pub entered_status_ms: u64,
    pub last_blocked_push_ms: u64,
    pub last_done_push_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyState {
    #[serde(default)]
    pub panes: HashMap<String, PaneNotifyState>,
}

impl NotifyState {
    pub fn load(paths: &StatePaths) -> Self {
        std::fs::read_to_string(paths.notify_state_file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &StatePaths) -> anyhow::Result<()> {
        std::fs::create_dir_all(&paths.root)?;
        std::fs::write(
            paths.notify_state_file(),
            serde_json::to_string_pretty(self)?,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    Blocked,
    Done { duration_ms: u64 },
}

/// 解析免打扰时段 `"22:00-08:00"` → (起始分钟, 结束分钟)。非法格式返回 None。
pub fn parse_quiet_range(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.trim().split_once('-')?;
    let parse_hm = |x: &str| -> Option<u32> {
        let (h, m) = x.trim().split_once(':')?;
        let h: u32 = h.parse().ok()?;
        let m: u32 = m.parse().ok()?;
        if h > 23 || m > 59 {
            return None;
        }
        Some(h * 60 + m)
    };
    Some((parse_hm(a)?, parse_hm(b)?))
}

/// 判断当前分钟是否落在免打扰时段（支持跨天区间，如 22:00-08:00）。
pub fn in_quiet_hours(minute_of_day: u32, ranges: &[(u32, u32)]) -> bool {
    ranges.iter().any(|&(s, e)| {
        if s <= e {
            minute_of_day >= s && minute_of_day < e
        } else {
            minute_of_day >= s || minute_of_day < e
        }
    })
}

/// 通知决策（纯函数）：输入上一状态与新状态，输出更新后的状态 + 应推送类型。
///
/// 规则：
/// - 进入 blocked 后延迟 `blocked_delay_seconds` 才推（等 agent 自动批准机制先跑）
/// - 同一 blocked 周期只推一次（last_blocked_push_ms < entered_status_ms 判定）
/// - working|blocked → idle|done 视为完成，推 Done（herdr 0.9.5 结束态上报 done，
///   只认 idle 会一个完成通知都发不出）
/// - 免打扰时段静默但照常记录推送时间，避免时段结束时爆发式补推
/// - 前台 pane 在 notify_when_focused=false 时不推
pub fn evaluate_notification(
    prev: Option<&PaneNotifyState>,
    new_status: &str,
    focused: bool,
    now_ms: u64,
    minute_of_day: u32,
    cfg: &NotifyConfig,
) -> (PaneNotifyState, Option<NotifyKind>) {
    let prev_status = prev.map(|p| p.last_status.clone()).unwrap_or_default();
    // Done 时长按上一状态持续期计算（状态切换前保留 entered）
    let prev_entered_ms = prev.map(|p| p.entered_status_ms);
    let mut st = prev.cloned().unwrap_or_else(|| PaneNotifyState {
        last_status: new_status.to_string(),
        entered_status_ms: now_ms,
        last_blocked_push_ms: 0,
        last_done_push_ms: 0,
    });
    if st.last_status != new_status {
        st.entered_status_ms = now_ms;
        st.last_status = new_status.to_string();
    }

    let mut push = None;
    let focused_allowed = cfg.notify_when_focused || !focused;
    let quiet = in_quiet_hours(minute_of_day, &cfg.quiet_hours_parsed());

    if new_status == "blocked" && cfg.blocked && focused_allowed {
        let due = now_ms.saturating_sub(st.entered_status_ms)
            >= cfg.blocked_delay_seconds.saturating_mul(1000);
        // 本周期尚未推过：推送时间早于进入时间即视为未推
        if due && st.last_blocked_push_ms < st.entered_status_ms {
            st.last_blocked_push_ms = now_ms;
            if !quiet {
                push = Some(NotifyKind::Blocked);
            }
        }
    }
    if matches!(new_status, "idle" | "done")
        && matches!(prev_status.as_str(), "working" | "blocked")
        && cfg.done
        && focused_allowed
    {
        let duration_ms = now_ms.saturating_sub(prev_entered_ms.unwrap_or(now_ms));
        // 短任务不推（可配置，默认 10s）：agent 每次跑命令都会 working↔idle 横跳，
        // 全推会触发微信服务端发送节流。阈值 0 = 全部推送。
        if duration_ms < cfg.min_done_duration_seconds.saturating_mul(1000) {
            return (st, None);
        }
        st.last_done_push_ms = now_ms;
        if !quiet {
            push = Some(NotifyKind::Done { duration_ms });
        }
    }
    (st, push)
}

impl NotifyConfig {
    /// 预解析免打扰时段；解析失败的段忽略。
    pub fn quiet_hours_parsed(&self) -> Vec<(u32, u32)> {
        self.quiet_hours
            .iter()
            .filter_map(|s| parse_quiet_range(s))
            .collect()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 媒体文件命名
// ═══════════════════════════════════════════════════════════════════════

/// 清洗原始文件名：去路径分隔与控制字符，空名兜底 "file"。
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 入站媒体落盘名：`YYYYMMDD-HHmmss_<文件名>`。
pub fn media_file_name(
    now_local: &chrono::DateTime<chrono::Local>,
    original: Option<&str>,
) -> String {
    let stamp = now_local.format("%Y%m%d-%H%M%S");
    match original.map(sanitize_filename).filter(|s| !s.is_empty()) {
        Some(name) => format!("{stamp}_{name}"),
        None => format!("{stamp}_file"),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Bridge 主体
// ═══════════════════════════════════════════════════════════════════════

#[derive(Default)]
struct BridgeState {
    /// 最近一次交互的 agent pane。
    last_target: Option<String>,
    /// 最近一次菜单排序（/agents 与推送共用，保证编号一致）。
    menu: Vec<AgentEntry>,
    /// 「规则清空」待二次确认标记。
    pending_clear: bool,
    /// 各 pane 最近一次推送里展示的选项列表（编号→原文）。
    /// agent 提问带选项时，用户回数字即可翻译成原文发送。
    pending_choices: HashMap<String, Vec<crate::herdr::PendingQuestion>>,
}

/// 业务桥接核心：微信消息处理 + herdr 操作 + 通知引擎状态。
pub struct Bridge {
    pub paths: StatePaths,
    pub cfg: Config,
    pub client: IlinkClient,
    pub bin: String,
    /// 学习规则簿（层④，内部 RwLock，多循环共享）。
    pub learn: Arc<LearnBook>,
    /// AI 意图客户端（层③，未配置 api_key 时为 None）。
    ai: Option<AiClient>,
    state: tokio::sync::Mutex<BridgeState>,
}

impl Bridge {
    pub fn new(paths: StatePaths, cfg: Config, client: IlinkClient) -> Self {
        let bin = cfg.herdr_binary();
        let learn = Arc::new(LearnBook::open(
            paths.learned_rules_file(),
            cfg.ai.max_learned_rules,
        ));
        let ai = if cfg.ai.is_usable() {
            AiClient::new(&cfg.ai).ok()
        } else {
            None
        };
        Self {
            paths,
            cfg,
            client,
            bin,
            learn,
            ai,
            state: tokio::sync::Mutex::new(BridgeState::default()),
        }
    }

    pub async fn menu(&self) -> Vec<AgentEntry> {
        self.state.lock().await.menu.clone()
    }

    pub async fn set_menu(&self, menu: Vec<AgentEntry>) {
        self.state.lock().await.menu = menu;
    }

    /// 记住某 pane 最近推送里展示的选项列表（用户回数字时翻译用）。
    pub async fn set_pending_choices(
        &self,
        pane_id: &str,
        questions: Vec<crate::herdr::PendingQuestion>,
    ) {
        self.state
            .lock()
            .await
            .pending_choices
            .insert(pane_id.to_string(), questions);
    }

    /// 翻译用户回复为发给 agent 的文本。
    /// 单题：纯数字 n → 第 n 个选项 label。
    /// 多题：每段 "题号.选项号"（空格分隔）→ 每题一行选项 label。
    pub async fn translate_reply(&self, pane_id: &str, text: &str) -> Option<String> {
        let st = self.state.lock().await;
        let questions = st.pending_choices.get(pane_id)?;
        if questions.is_empty() {
            return None;
        }
        let t = text.trim();
        // 多题格式：含 '.' 且各段能解析
        if t.contains('.') && !t.starts_with("w") {
            let mut answers = Vec::new();
            let mut all_valid = true;
            for seg in t.split_whitespace() {
                let Some((qi, oi)) = seg.split_once('.') else {
                    all_valid = false;
                    break;
                };
                let (Ok(q), Ok(o)) = (qi.parse::<usize>(), oi.parse::<usize>()) else {
                    all_valid = false;
                    break;
                };
                match questions
                    .get(q.saturating_sub(1))
                    .and_then(|q| q.options.get(o.saturating_sub(1)))
                {
                    Some(opt) => answers.push(opt.label.clone()),
                    None => {
                        all_valid = false;
                        break;
                    }
                }
            }
            if all_valid && !answers.is_empty() {
                return Some(answers.join("\n"));
            }
            return None; // 解析失败 → 走其他流程
        }
        // 单题：纯数字
        if let Ok(n) = t.parse::<usize>() {
            if questions.len() == 1 {
                return questions[0]
                    .options
                    .get(n.saturating_sub(1))
                    .map(|o| o.label.clone());
            }
        }
        None
    }

    /// 拉取最新 pane 列表并回填菜单（编号 = blocked 优先排序）。
    pub async fn refresh_menu(&self) -> Result<Vec<AgentEntry>, String> {
        let panes = herdr::pane_list(&self.bin)
            .await
            .map_err(|e| format!("无法获取 herdr pane 列表：{e:#}"))?;
        let menu = finalize_menu(collect_agents(&panes));
        self.set_menu(menu.clone()).await;
        Ok(menu)
    }

    async fn resolve(&self, target: Option<String>) -> Result<AgentEntry, String> {
        let menu = self.menu().await;
        match target {
            Some(t) => resolve_target(&menu, &t),
            None => {
                let last = self.state.lock().await.last_target.clone();
                default_target(&menu, last.as_deref())
            }
        }
    }

    async fn set_last_target(&self, pane_id: &str) {
        self.state.lock().await.last_target = Some(pane_id.to_string());
    }

    /// 入站消息处理：只响应主人账号。返回应回复的文本（None = 不回复）。
    pub async fn handle_message(&self, msg: &Message) -> Option<String> {
        if msg.from_user_id.is_none()
            || msg.from_user_id.as_deref() != Some(self.client.credentials.owner_user_id.as_str())
        {
            return None;
        }
        // 文本命令或语音转写 → 同一管道
        let text = msg
            .text_content()
            .or_else(|| msg.voice_transcript().map(str::to_string));
        if let Some(text) = text {
            if text.len() > self.cfg.wechat.max_command_bytes {
                return Some(format!(
                    "✗ 命令过长（{} 字节 > 上限 {}）",
                    text.len(),
                    self.cfg.wechat.max_command_bytes
                ));
            }
            return Some(self.handle_command_text(&text).await);
        }
        if msg.image_item().is_some() || msg.file_item().is_some() || msg.video_item().is_some() {
            return Some(self.handle_media(msg).await);
        }
        if msg.voice_item().is_some() {
            return Some("这条语音没转出文字，请打字或重说。".into());
        }
        None
    }

    /// 文本命令处理：五级流水线
    /// 层①精确命令 → 数字快捷回复 → 设置命令 → 层②/④本地意图（内置规则→学习规则）
    /// → 层③ AI 意图 → 兜底提示。
    pub async fn handle_command_text(&self, text: &str) -> String {
        if let Some(cmd) = parse_command(text) {
            self.state.lock().await.pending_clear = false;
            return self.execute_command(cmd).await;
        }
        if let Some(qr) = parse_quick_reply(text) {
            self.state.lock().await.pending_clear = false;
            return self.execute_quick(qr).await;
        }
        if let Some(sc) = parse_settings_command(text) {
            // 「确认/取消」只在有待确认操作时拦截，否则可能是在回复 agent 的提问
            let confirmable = matches!(sc, SettingsCommand::ConfirmClear | SettingsCommand::Cancel);
            let pending = self.state.lock().await.pending_clear;
            if !confirmable || pending {
                return self.execute_settings(sc).await;
            }
        }
        // 待选选项：agent 推送里展示了编号选项（单题或多题问卷）。
        // 单题回「2」、多题回「1.2 2.1」→ 翻译成选项原文发送；解析不了走正常流程。
        {
            let last = self.state.lock().await.last_target.clone();
            if let Some(pid) = last {
                if let Some(choice) = self.translate_reply(&pid, text).await {
                    return self.cmd_send(Some(pid), choice).await;
                }
            }
        }
        let threshold = self.cfg.ai.rule_match_threshold;
        if let Some(intent) = intent::parse_local(text, &self.learn, threshold) {
            if intent.is_actionable() {
                return self.execute_intent(intent).await;
            }
        }
        if let Some(ai) = &self.ai {
            // AI 端点可能耗时数十秒：先回一条即时提示，让用户知道消息已收到、正在解析，
            // 而不是干等。失败也不阻断后续结果回复（只是少了提示）。
            self.notify_thinking().await;
            return self.ai_handle(ai, text).await;
        }
        // 兜底：非命令文本 → 转发给最近交互的 agent（多轮对话用）
        if let Some(pid) = self.state.lock().await.last_target.clone() {
            if let Some(entry) = self.menu().await.iter().find(|e| e.pane_id == pid) {
                return self
                    .cmd_send(Some(entry.pane_id.clone()), text.to_string())
                    .await;
            }
        }
        "❓ 没看懂这条消息，发送 /help 查看命令。".to_string()
    }

    /// 触发 AI 前给微信发「正在解析」即时回执（最近会话令牌，失败静默）。
    async fn notify_thinking(&self) {
        if !self.cfg.ai.thinking_notice {
            return;
        }
        let to = self.client.credentials.owner_user_id.clone();
        if let Err(e) = self
            .client
            .send_text(&to, "⏳ 正在理解你的意思，请稍候…", None, None)
            .await
        {
            tracing::debug!("发送「正在解析」提示失败：{}", redact_text(&format!("{e}")));
        }
    }

    /// 命名目标补全：项目名/agent 名/标题 → pane ID（#序号、last、pane ID 原样保留）。
    async fn complete_target(&self, intent: &mut Intent) {
        let Some(t) = intent.target().map(str::to_string) else {
            return;
        };
        if t.starts_with('#') || t.eq_ignore_ascii_case("last") || t == "最后" {
            return;
        }
        let menu = self.menu().await;
        if menu.iter().any(|e| e.pane_id == t) {
            return;
        }
        if let Ok(entry) = resolve_target(&menu, &t) {
            intent.set_target(Some(entry.pane_id));
            return;
        }
        // 精确解析失败 → 模糊包含补全（agent 名 / 终端标题 / pane ID，取最长命中）
        let cands: Vec<String> = menu
            .iter()
            .flat_map(|e| [e.agent.clone(), e.title.clone(), e.pane_id.clone()])
            .collect();
        let crefs: Vec<&str> = cands.iter().map(String::as_str).collect();
        if let Some(hit) = extract_named_target(&t, &crefs) {
            if let Some(e) = menu
                .iter()
                .find(|e| e.agent == hit || e.title == hit || e.pane_id == hit)
            {
                intent.set_target(Some(e.pane_id.clone()));
            }
        }
    }

    /// 意图 → 层①命令执行通道（AI 只做翻译，执行/回显/纠错与手敲命令同路）。
    async fn execute_intent(&self, mut intent: Intent) -> String {
        self.complete_target(&mut intent).await;
        let cmd = match intent {
            Intent::Approve { target } => Command::Approve { target },
            Intent::Trust { target } => Command::Trust { target },
            Intent::Deny { target } => Command::Deny { target },
            Intent::Interrupt { target } => Command::Interrupt { target },
            Intent::Read { target } => Command::Read {
                target,
                lines: None,
            },
            Intent::Status => Command::Status,
            Intent::Send { target, text } => Command::Send { target, text },
            Intent::Unknown => return "❓ 没看懂这条消息，发送 /help 查看命令。".into(),
        };
        self.execute_command(cmd).await
    }

    async fn execute_command(&self, cmd: Command) -> String {
        match cmd {
            Command::Help => HELP_TEXT.to_string(),
            Command::Rules => self.learn.format_list(),
            Command::Forget { index } => match self.learn.remove(index) {
                Some(r) => format!("🗑 已忘掉规则：「{}」", r.example),
                None => format!("没有 #{} 这条规则，发送 /rules 查看序号", index),
            },
            Command::Agents => match self.refresh_menu().await {
                Ok(menu) => fmt_agents_menu(&menu),
                Err(e) => format!("✗ {e}"),
            },
            Command::Status => match self.refresh_menu().await {
                Ok(menu) => fmt_status_summary(&menu),
                Err(e) => format!("✗ {e}"),
            },
            Command::Debug => self.fmt_debug().await,
            Command::Read { target, lines } => self.cmd_read(target, lines).await,
            Command::Send { target, text } => self.cmd_send(target, text).await,
            Command::Approve { target } => self.cmd_answer(target, "y", "✅ 已批准").await,
            Command::Trust { target } => {
                self.cmd_answer(target, "trust, always allow", "✅ 已发送信任指令")
                    .await
            }
            Command::Deny { target } => self.cmd_answer(target, "n", "✅ 已拒绝").await,
            Command::Interrupt { target } => self.cmd_interrupt(target).await,
            Command::ReadFull { target, .. } => {
                let entry = match self.resolve(target).await {
                    Ok(e) => e,
                    Err(msg) => return msg,
                };
                self.set_last_target(&entry.pane_id).await;
                self.cmd_read_full(&entry).await
            }
        }
    }

    /// 层③：AI 意图解析（带 agent 快照上下文），失败回确认菜单，成功后沉淀学习规则。
    async fn ai_handle(&self, ai: &AiClient, text: &str) -> String {
        let snapshot = self.ai_snapshot().await;
        let last = self.state.lock().await.last_target.clone();
        let user = AiClient::user_prompt(&snapshot, last.as_deref(), text);
        let reply = match ai.complete(AiClient::system_prompt(), &user).await {
            Ok(raw) => {
                tracing::info!("AI 原始响应：{}", raw.chars().take(400).collect::<String>());
                AiClient::parse_reply(&raw)
                    .and_then(|r| AiClient::validate(r, self.cfg.ai.min_confidence))
            }
            Err(e) => Err(e),
        };
        match reply {
            Ok(r) => self.execute_ai_reply(r, text).await,
            Err(e) => {
                self.ai_failure_menu(format!("AI 解析失败：{}", e.hint()))
                    .await
            }
        }
    }

    /// agent 列表快照（AI 上下文用；刷新失败回退当前菜单）。
    async fn ai_snapshot(&self) -> String {
        let menu = match self.refresh_menu().await {
            Ok(m) => m,
            Err(_) => self.menu().await,
        };
        menu.iter()
            .map(|e| {
                format!(
                    "#{} {} {} pane={}（{}）",
                    e.index,
                    e.display_name(),
                    e.status,
                    e.pane_id,
                    status_zh(&e.status)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 执行 AI 解析出的动作序列；全部成功后把 learn 字段沉淀进学习规则簿。
    async fn execute_ai_reply(&self, reply: AiReply, original: &str) -> String {
        let mut outs: Vec<String> = Vec::new();
        for a in &reply.actions {
            match ai::action_to_intent(a) {
                Ok(intent) => outs.push(self.execute_intent(intent).await),
                Err(e) => outs.push(format!("✗ {e}")),
            }
        }
        if outs.is_empty() {
            return self
                .ai_failure_menu("AI 没能理解这条消息".to_string())
                .await;
        }
        if !outs.iter().any(|o| o.starts_with("✗")) {
            if let Some(l) = reply.learn {
                let example = if l.example.trim().is_empty() {
                    original.to_string()
                } else {
                    l.example
                };
                if !self.learn.add(l.keywords, l.template, example) {
                    tracing::debug!("学习规则沉淀被拒绝（模板非法或示例为空）");
                }
            }
        }
        outs.join("\n\n")
    }

    /// AI 失败兜底：回精简确认菜单，绝不瞎猜。
    async fn ai_failure_menu(&self, reason: String) -> String {
        let menu = match self.refresh_menu().await {
            Ok(m) => m,
            Err(_) => self.menu().await,
        };
        let mut out = format!("🤔 {reason}\n\n请改用精确命令：\n\n");
        if menu.is_empty() {
            out.push_str("（当前没有活跃 agent，发送 /agents 刷新）");
        } else {
            out.push_str(&fmt_agents_menu(&menu));
        }
        out
    }

    async fn cmd_read(&self, target: Option<String>, lines: Option<u32>) -> String {
        let entry = match self.resolve(target).await {
            Ok(e) => e,
            Err(e) => return format!("✗ {e}"),
        };
        let n = lines.unwrap_or(self.cfg.read_lines).clamp(1, 200);
        self.set_last_target(&entry.pane_id).await;
        let output = self.read_cleaned(&entry.pane_id, n, n as usize).await;
        format!(
            "🤖 #{} {}（最近 {} 行）\n\n```\n{}\n```",
            entry.index,
            entry.display_name(),
            n,
            output
        )
    }

    /// 全文模式：读 pi 会话文件最近几条 assistant 消息（完整原文，不受终端快照限制）。
    /// 非 pi agent / 文件缺失时回退画面清洗。
    async fn cmd_read_full(&self, entry: &AgentEntry) -> String {
        let Some(path) = &entry.session_file else {
            let output = self.read_cleaned(&entry.pane_id, 40, 20).await;
            return format!(
                "⚠ {} 没有 pi 会话文件（可能不是 pi agent），返回终端画面：
{}",
                entry.display_name(),
                output
            );
        };
        // 只要最后一轮回复（原文完整，不受终端快照限制）
        let Some(last) = crate::herdr::recent_assistant_texts(path, 1)
            .into_iter()
            .next()
        else {
            let output = self.read_cleaned(&entry.pane_id, 40, 20).await;
            return format!(
                "⚠ 会话文件里没有找到回复，返回终端画面：
{}",
                output
            );
        };
        format!(
            "📖 {} 最后一轮回复：
{}",
            entry.display_name(),
            last
        )
    }

    async fn cmd_send(&self, target: Option<String>, text: String) -> String {
        if text.trim().is_empty() {
            return "用法：/send <目标> <文本>，如：/send 1 把测试跑完".into();
        }
        if text.len() > self.cfg.wechat.max_send_bytes {
            return format!(
                "✗ 文本过长（{} 字节 > 上限 {}）",
                text.len(),
                self.cfg.wechat.max_send_bytes
            );
        }
        let entry = match self.resolve(target).await {
            Ok(e) => e,
            Err(e) => return format!("✗ {e}"),
        };
        match herdr::pane_send_line(&self.bin, &entry.pane_id, &text).await {
            Ok(()) => {
                self.set_last_target(&entry.pane_id).await;
                format!("✅ 已发送给 #{} {}", entry.index, entry.display_name())
            }
            Err(e) => format!("✗ 发送失败：{e:#}"),
        }
    }

    /// 批准/拒绝/信任：向 pane 注入对应应答并回车。
    async fn cmd_answer(&self, target: Option<String>, answer: &str, done_prefix: &str) -> String {
        let entry = match self.resolve(target).await {
            Ok(e) => e,
            Err(e) => return format!("✗ {e}"),
        };
        match herdr::pane_send_line(&self.bin, &entry.pane_id, answer).await {
            Ok(()) => {
                self.set_last_target(&entry.pane_id).await;
                format!("{done_prefix} #{} {}", entry.index, entry.display_name())
            }
            Err(e) => format!("✗ 发送失败：{e:#}"),
        }
    }

    async fn cmd_interrupt(&self, target: Option<String>) -> String {
        let entry = match self.resolve(target).await {
            Ok(e) => e,
            Err(e) => return format!("✗ {e}"),
        };
        match herdr::pane_send_keys(&self.bin, &entry.pane_id, &["Ctrl+c"]).await {
            Ok(()) => {
                self.set_last_target(&entry.pane_id).await;
                format!("✅ 已中断 #{} {}", entry.index, entry.display_name())
            }
            Err(e) => format!("✗ 中断失败：{e:#}"),
        }
    }

    async fn execute_quick(&self, qr: QuickReply) -> String {
        let menu = self.menu().await;
        if menu.is_empty() {
            return "请先发 /agents 查看 agent 列表".into();
        }
        // 序号或 pane ID 统一解析到 entry；序号歧义时提示用 pane ID
        let resolve = |target: &TargetRef| -> Result<AgentEntry, String> {
            match target {
                TargetRef::Index(n) => menu
                    .get(n.saturating_sub(1))
                    .cloned()
                    .ok_or_else(|| format!(
                        "序号 {n} 超出范围：当前共 {} 个 agent。菜单可能已刷新，发送 /agents 查看；\n也可直接用 pane ID，如 `w8:p3 批准`",
                        menu.len()
                    )),
                TargetRef::PaneId(pid) => menu
                    .iter()
                    .find(|e| e.pane_id.eq_ignore_ascii_case(pid))
                    .cloned()
                    .ok_or_else(|| format!(
                        "未找到 pane `{pid}`，发送 /agents 查看当前列表"
                    )),
            }
        };
        match qr {
            QuickReply::Detail(n) => match menu.get(n.saturating_sub(1)) {
                Some(entry) => {
                    self.set_last_target(&entry.pane_id).await;
                    let output = self.read_cleaned(&entry.pane_id, 20, 5).await;
                    fmt_detail(entry, &output)
                }
                None => format!(
                    "序号 {n} 超出范围：当前共 {} 个 agent。菜单可能已刷新，发送 /agents 查看；\n也可直接用 pane ID，如发 `w8:p3` 看详情",
                    menu.len()
                ),
            },

            QuickReply::DetailFull(n) => match menu.get(n.saturating_sub(1)) {
                Some(entry) => {
                    self.set_last_target(&entry.pane_id).await;
                    self.cmd_read_full(entry).await
                }
                None => format!("序号 {n} 超出范围：当前共 {} 个 agent", menu.len()),
            },
            QuickReply::Act(target, action) => {
                let entry = match resolve(&target) {
                    Ok(e) => e,
                    Err(msg) => return msg,
                };
                match action {
                    QuickAction::Approve => {
                        self.cmd_answer(Some(entry.pane_id.clone()), "y", "✅ 已批准").await
                    }
                    QuickAction::Trust => {
                        self.cmd_answer(Some(entry.pane_id.clone()), "trust, always allow", "✅ 已发送信任指令")
                            .await
                    }
                    QuickAction::Deny => {
                        self.cmd_answer(Some(entry.pane_id.clone()), "n", "✅ 已拒绝").await
                    }
                    QuickAction::Interrupt => self.cmd_interrupt(Some(entry.pane_id.clone())).await,
                    QuickAction::Send(text) => {
                        self.set_last_target(&entry.pane_id).await;
                        self.cmd_send(Some(entry.pane_id.clone()), text).await
                    }
                    QuickAction::Read => {
                        self.set_last_target(&entry.pane_id).await;
                        let output = self.read_cleaned(&entry.pane_id, 20, 5).await;
                        fmt_detail(&entry, &output)
                    }
                }
            }
        }
    }

    /// 设置命令执行。
    async fn execute_settings(&self, sc: SettingsCommand) -> String {
        match sc {
            SettingsCommand::ConfirmClear => {
                let pending = { self.state.lock().await.pending_clear };
                if !pending {
                    return "没有待确认的清空操作。发送「规则清空」发起。".into();
                }
                self.state.lock().await.pending_clear = false;
                let n = self.learn.clear();
                if n == 0 {
                    "学习规则本来就为空。".into()
                } else {
                    format!("🗑 已清空 {n} 条学习规则。")
                }
            }
            SettingsCommand::Cancel => {
                let had = {
                    let mut s = self.state.lock().await;
                    std::mem::replace(&mut s.pending_clear, false)
                };
                if had {
                    "已取消，学习规则保持不变。".into()
                } else {
                    "没有待确认的操作。".into()
                }
            }
            SettingsCommand::NotifyOn => {
                self.state.lock().await.pending_clear = false;
                self.set_notify_flags(true);
                "🔔 已开启全部推送。".into()
            }
            SettingsCommand::NotifyOff => {
                self.state.lock().await.pending_clear = false;
                self.set_notify_flags(false);
                "🔕 已关闭全部推送，发送「通知 on」恢复。".into()
            }
            SettingsCommand::Mute { until_ms } => {
                self.state.lock().await.pending_clear = false;
                self.set_mute_until(Some(until_ms));
                use chrono::TimeZone;
                let until = chrono::Local
                    .timestamp_millis_opt(until_ms as i64)
                    .single()
                    .map(|dt| dt.format("%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                format!("🔕 已静音至 {until}，期间阻塞/完成通知暂停推送（取消静音随时可发）。")
            }
            SettingsCommand::Unmute => {
                self.state.lock().await.pending_clear = false;
                self.set_mute_until(None);
                "🔔 已解除静音。".into()
            }
            SettingsCommand::RulesClear => {
                let n = self.learn.len();
                if n == 0 {
                    "学习规则本来就为空。".into()
                } else {
                    {
                        let mut s = self.state.lock().await;
                        s.pending_clear = true;
                    }
                    format!("⚠️ 即将清空全部 {n} 条学习规则。回复「确认」执行，回复「取消」放弃。")
                }
            }
        }
    }

    /// 通知开关落盘 config.toml（notify_loop 每轮重读，即时生效）。
    fn set_notify_flags(&self, on: bool) {
        let mut cfg = Config::load(&self.paths).unwrap_or_else(|_| self.cfg.clone());
        cfg.notify.blocked = on;
        cfg.notify.done = on;
        if let Err(e) = cfg.save(&self.paths) {
            tracing::warn!("保存通知设置失败：{e}");
        }
    }

    /// 临时静音落盘 runtime.json（notify_loop 每轮检查）。
    fn set_mute_until(&self, until_ms: Option<u64>) {
        let mut rt = RuntimeState::load(&self.paths);
        rt.mute_until_ms = until_ms;
        rt.save(&self.paths).ok();
    }

    /// 读 pane 输出并清洗（失败不阻断，返回占位文本）。
    async fn read_cleaned(&self, pane_id: &str, lines: u32, keep: usize) -> String {
        match herdr::pane_read(&self.bin, pane_id, lines).await {
            Ok(out) => {
                let cleaned = clean_terminal_output(&out, keep);
                if cleaned.is_empty() {
                    "（暂无输出）".to_string()
                } else {
                    cleaned
                }
            }
            Err(e) => format!("（读取失败：{e:#}）"),
        }
    }

    /// 入站媒体落盘并回复路径。
    async fn handle_media(&self, msg: &Message) -> String {
        let spec: Option<(Option<&str>, Option<&crate::ilink::types::CdnMedia>, &str)> =
            if let Some(img) = msg.image_item() {
                // 图片优先 image_item.aeskey，其次 media.aes_key（download_media 内部处理）
                Some((img.aeskey.as_deref(), img.media.as_ref(), "image.jpg"))
            } else if let Some(f) = msg.file_item() {
                Some((
                    None,
                    f.media.as_ref(),
                    f.file_name.as_deref().unwrap_or("file"),
                ))
            } else if let Some(v) = msg.video_item() {
                Some((None, v.media.as_ref(), "video.mp4"))
            } else {
                None
            };
        let Some((aeskey, media, name)) = spec else {
            return "暂不支持这类消息。".into();
        };
        let bytes = match self.client.download_media(aeskey, media).await {
            Ok(b) => b,
            Err(e) => return format!("✗ 媒体下载失败：{e}"),
        };
        if bytes.is_empty() {
            return "✗ 媒体下载为空，未保存。".into();
        }
        let dir = self.paths.media_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return format!("✗ 创建媒体目录失败：{e}");
        }
        let now = chrono::Local::now();
        let path = dir.join(media_file_name(&now, Some(name)));
        match std::fs::write(&path, &bytes) {
            Ok(()) => format!(
                "已保存: {}\n喂给 agent：/send <目标> {}",
                path.display(),
                path.display()
            ),
            Err(e) => format!("✗ 保存失败：{e}"),
        }
    }

    /// /debug 链路诊断。
    async fn fmt_debug(&self) -> String {
        let runtime = RuntimeState::load(&self.paths);
        let sync = SyncState::load(&self.paths);
        let pid = std::fs::read_to_string(self.paths.daemon_pid_file())
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .or(runtime.pid);
        let mut out = String::from("🔍 链路诊断\n\n");
        match pid {
            Some(p) if herdr::pid_alive(p) => {
                out.push_str(&format!("守护进程：● 运行中（pid {p}）\n"))
            }
            Some(p) => out.push_str(&format!("守护进程：✗ 未运行（pid {p} 已退出）\n")),
            None => out.push_str("守护进程：✗ 未运行\n"),
        }
        match herdr::pane_list(&self.bin).await {
            Ok(panes) => {
                let agents = collect_agents(&panes);
                out.push_str(&format!(
                    "herdr：✓ 可达（{} 个 pane，{} 个 agent）\n",
                    panes.len(),
                    agents.len()
                ));
            }
            Err(e) => out.push_str(&format!("herdr：✗ 不可达（{e:#}）\n")),
        }
        out.push_str(&format!(
            "微信账号尾号：{}\n长轮询游标：{} 字节\n失败计数：{}\n",
            self.client.credentials.token_suffix(),
            sync.get_updates_buf.len(),
            runtime.fail_count
        ));
        let ai = &self.cfg.ai;
        out.push_str(&format!(
            "AI 意图层：{}\n",
            if ai.is_usable() {
                format!("已启用（{}）", ai.model)
            } else {
                "未启用（仅本地规则层）".to_string()
            }
        ));
        let last = self.state.lock().await.last_target.clone();
        out.push_str(&format!(
            "最近交互目标：{}\n",
            last.unwrap_or_else(|| "无".into())
        ));
        out
    }
}

/// 便捷构造：Arc 包装（daemon 双循环共享）。
pub fn shared_bridge(paths: StatePaths, cfg: Config, client: IlinkClient) -> Arc<Bridge> {
    Arc::new(Bridge::new(paths, cfg, client))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(pane_id: &str, agent: &str, status: &str, cwd: &str, focused: bool) -> PaneSnapshot {
        PaneSnapshot {
            pane_id: pane_id.into(),
            workspace_id: "w8".into(),
            agent: Some(agent.into()),
            agent_status: Some(status.into()),
            terminal_title_stripped: Some(format!("π - {agent}")),
            cwd: Some(cwd.into()),
            focused,
            agent_session: None,
        }
    }

    fn sample_menu() -> Vec<AgentEntry> {
        let panes = vec![
            pane("w8:p1", "pi", "idle", r"E:\model\FluidBar", false),
            pane("w8:p3", "pi", "blocked", r"E:\wxherd", true),
            pane("w8:p2", "pi", "working", r"E:\model\FluidBar2", false),
        ];
        finalize_menu(collect_agents(&panes))
    }

    // ── 终端清洗 ──────────────────────────────────────────────────────

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("纯文本"), "纯文本");
        assert_eq!(strip_ansi("\x1b[31m红\x1b[0m"), "红");
        assert_eq!(strip_ansi("\x1b[1;32mok\x1b[m tail"), "ok tail");
        assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
        assert_eq!(strip_ansi("\x1b]8;;http://x\x1b\\link"), "link");
        assert_eq!(strip_ansi("\x1b(B"), ""); // 两字符转义
        assert_eq!(strip_ansi("结尾悬\x1b"), "结尾悬");
    }

    #[test]
    fn test_clean_terminal_output_borders_and_blanks() {
        let raw = "\x1b[36m╭──────────────────────────╮\x1b[0m\n\
                  │                          │\n\
                  ╰──────────────────────────╯\n\
                  E:\\wxherd  working 25m\n\
                  \n\
                  · Magpie · zcode/GLM-5.3-Flash · high\n";
        // 分支栏是装饰被滤掉；路径+计时行无 % 不误伤，保留
        let cleaned = clean_terminal_output(raw, 10);
        assert_eq!(cleaned, "E:\\wxherd  working 25m");
    }

    /// 回归：真实 pi TUI 画面——状态栏在末尾，真实输出在中间。
    /// 「末次输出」必须取到命令结果，而不是 MCP/进度条。
    #[test]
    fn test_clean_terminal_output_skips_tui_chrome_tail() {
        let raw = "✓ tests passed 128/128\n\
                   ● Bash: 1 running • 2 done\n\
                   ╭─ ⠹ Working ─╮\n\
                   │             │\n\
                   ╰─────────────╯\n\
                   E:\\wxherd 1.99.1 working 1m 4s 37s  [██硓░░] 18.5% · 194k/1.0M\n\
                   · Magpie · group/never-stuck · high\n\
                    🔌 MCP: 2 servers enabled | trace ● 48\n";
        let cleaned = clean_terminal_output(raw, 5);
        let lines: Vec<&str> = cleaned.lines().collect();
        assert_eq!(*lines.last().unwrap(), "● Bash: 1 running • 2 done");
        assert!(cleaned.contains("tests passed"));
        assert!(!cleaned.contains("MCP:"), "状态栏不得出现在清洗结果");
        assert!(!cleaned.contains("█"), "进度条不得出现");
        assert!(!cleaned.contains("Magpie"), "分支栏不得出现");
    }

    #[test]
    fn test_is_tui_chrome_line() {
        assert!(is_tui_chrome_line("🔌 MCP: 2 servers enabled | trace ● 48"));
        assert!(is_tui_chrome_line(
            "TPS 25.2 tok/s |  TTFT 10.9s |  6m 47s |  23k |  6.2k"
        ));
        assert!(is_tui_chrome_line("· Magpie · group/never-stuck · high"));
        // 不同模型/分支组合的结构匹配（字面量不同但结构相同）
        assert!(is_tui_chrome_line(
            "· Doulor · workbuddy-deepseek-v4.1-flash · high"
        ));
        assert!(is_tui_chrome_line("· 任何项目 · 任何分支/模型名 · low"));
        assert!(
            !is_tui_chrome_line("选一个：品牌 A · 品牌 B · 品牌 C"),
            "用户可见的正文中点分隔文本不是状态栏（不以 · 开头且不 以优先级词结尾）"
        );
        assert!(is_tui_chrome_line("[██░░░░] 18.5% · 194k/1.0M"));
        assert!(is_tui_chrome_line(
            "E:\\dir 1.99.1 working 1m 4s 37s  18.5% · 194k/1.0M"
        ));
        assert!(is_tui_chrome_line(""));
        assert!(!is_tui_chrome_line("✓ tests passed 128/128"));
        assert!(!is_tui_chrome_line("编译完成，耗时 42s"));
    }

    #[test]
    fn test_clean_terminal_output_keeps_tail() {
        let lines: Vec<String> = (1..=20).map(|i| format!("line-{i}")).collect();
        let cleaned = clean_terminal_output(&lines.join("\n"), 5);
        let expected: Vec<String> = (16..=20).map(|i| format!("line-{i}")).collect();
        assert_eq!(cleaned, expected.join("\n"));
    }

    #[test]
    fn test_clean_terminal_output_inner_border_chars_preserved() {
        // 行内有边框字符但主体是内容 → 保留（只剥两端）
        let cleaned = clean_terminal_output("│ status: blocked │", 10);
        assert_eq!(cleaned, "status: blocked");
    }

    #[test]
    fn test_clean_terminal_output_empty() {
        assert_eq!(clean_terminal_output("", 10), "");
        assert_eq!(clean_terminal_output("│ │\n╰─╯\n", 10), "");
    }

    // ── 菜单与目标解析 ────────────────────────────────────────────────

    #[test]
    fn test_collect_agents_skips_plain_panes() {
        let panes = vec![
            pane("w8:p1", "pi", "idle", r"E:\model\FluidBar", false),
            PaneSnapshot {
                pane_id: "w8:p9".into(),
                workspace_id: "w8".into(),
                agent: None,
                agent_status: Some("unknown".into()),
                terminal_title_stripped: Some("pwsh".into()),
                cwd: None,
                focused: false,
                agent_session: None,
            },
        ];
        let agents = collect_agents(&panes);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].project, "FluidBar");
    }

    #[test]
    fn test_finalize_menu_spaces_order_matches_herdr_list() {
        // herdr 默认 spaces 模式：序号 = pane list 返回序，不重排。
        // 用户配置未设置 agent_panel_sort，本机测试环境应为 spaces。
        let menu = sample_menu();
        assert_eq!(
            menu[0].pane_id, "w8:p1",
            "spaces 模式保持 list 原序：idle 在前"
        );
        assert_eq!(menu[1].pane_id, "w8:p3");
        assert_eq!(menu[2].pane_id, "w8:p2");
        assert_eq!(menu[0].index, 1);
        assert_eq!(menu[2].index, 3);
    }

    #[test]
    fn test_resolve_target_by_number_and_pane() {
        let menu = sample_menu();
        assert_eq!(
            resolve_target(&menu, "1").unwrap().pane_id,
            "w8:p1",
            "spaces 模式 #1 = list 首个"
        );
        assert_eq!(resolve_target(&menu, "#2").unwrap().pane_id, "w8:p3");
        assert_eq!(resolve_target(&menu, "w8:p1").unwrap().status, "idle");
        assert!(resolve_target(&menu, "0").is_err());
        assert!(resolve_target(&menu, "99")
            .unwrap_err()
            .contains("超出范围"));
    }

    #[test]
    fn test_resolve_target_name_and_fuzzy() {
        let menu = sample_menu();
        // 项目名精确（大小写不敏感）
        assert_eq!(resolve_target(&menu, "wxherd").unwrap().pane_id, "w8:p3");
        assert_eq!(resolve_target(&menu, "WXHERD").unwrap().pane_id, "w8:p3");
        // agent 名多命中 → 歧义
        let err = resolve_target(&menu, "pi").unwrap_err();
        assert!(err.contains("匹配到多个"), "got: {err}");
        // 子串多命中 → 歧义并列出候选
        let err = resolve_target(&menu, "fluid").unwrap_err();
        assert!(err.contains("匹配到多个"), "got: {err}");
        assert!(
            err.contains("FluidBar2") && err.contains("FluidBar"),
            "got: {err}"
        );
        // 子串唯一命中
        assert_eq!(resolve_target(&menu, "herd").unwrap().pane_id, "w8:p3");
        // 无命中
        assert!(resolve_target(&menu, "不存在")
            .unwrap_err()
            .contains("未找到"));
        assert!(resolve_target(&menu, "")
            .unwrap_err()
            .contains("请指定目标"));
    }

    #[test]
    fn test_default_target_prefers_last_then_blocked() {
        let menu = sample_menu();
        assert_eq!(
            default_target(&menu, Some("w8:p1")).unwrap().pane_id,
            "w8:p1"
        );
        // last 已失效 → 回菜单首个（spaces 模式下 = herdr 列表第一个）
        assert_eq!(
            default_target(&menu, Some("gone:p9")).unwrap().pane_id,
            "w8:p1"
        );
        assert_eq!(default_target(&menu, None).unwrap().pane_id, "w8:p1");
        assert!(default_target(&[], None).is_err());
    }

    // ── 命令解析 ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_commands() {
        assert_eq!(parse_command("/help"), Some(Command::Help));
        assert_eq!(parse_command("帮助"), Some(Command::Help));
        assert_eq!(parse_command("/agents"), Some(Command::Agents));
        assert_eq!(parse_command("列表"), Some(Command::Agents));
        assert_eq!(parse_command("/LIST"), Some(Command::Agents));
        assert_eq!(parse_command("/status"), Some(Command::Status));
        assert_eq!(parse_command("状态"), Some(Command::Status));
        assert_eq!(parse_command("/rules"), Some(Command::Rules));
        assert_eq!(parse_command("忘掉 3"), Some(Command::Forget { index: 3 }));
        assert_eq!(
            parse_command("/forget #3"),
            Some(Command::Forget { index: 3 })
        );
        assert_eq!(parse_command("/debug"), Some(Command::Debug));
        assert_eq!(parse_command("/unknown"), None);
        assert_eq!(parse_command("   "), None);
    }

    #[test]
    fn test_parse_read_send_answers() {
        assert_eq!(
            parse_command("/read 2 10"),
            Some(Command::Read {
                target: Some("2".into()),
                lines: Some(10)
            })
        );
        assert_eq!(
            parse_command("/read w8:p3"),
            Some(Command::Read {
                target: Some("w8:p3".into()),
                lines: None
            })
        );
        assert_eq!(
            parse_command("读取"),
            Some(Command::Read {
                target: None,
                lines: None
            })
        );
        assert_eq!(
            parse_command("/send #2 把测试跑完"),
            Some(Command::Send {
                target: Some("#2".into()),
                text: "把测试跑完".into()
            })
        );
        assert_eq!(
            parse_command("/reply 1  hello world "),
            Some(Command::Send {
                target: Some("1".into()),
                text: "hello world".into()
            })
        );
        assert_eq!(
            parse_command("/approve 2"),
            Some(Command::Approve {
                target: Some("2".into())
            })
        );
        assert_eq!(
            parse_command("批准"),
            Some(Command::Approve { target: None })
        );
        assert_eq!(parse_command("/deny"), Some(Command::Deny { target: None }));
        assert_eq!(
            parse_command("/interrupt w8:p3"),
            Some(Command::Interrupt {
                target: Some("w8:p3".into())
            })
        );
        assert_eq!(
            parse_command("/stop 1"),
            Some(Command::Interrupt {
                target: Some("1".into())
            })
        );
        assert_eq!(
            parse_command("/send"),
            Some(Command::Send {
                target: None,
                text: String::new()
            })
        );
    }

    #[test]
    fn test_parse_quick_reply() {
        assert_eq!(parse_quick_reply("1"), Some(QuickReply::Detail(1)));
        assert_eq!(parse_quick_reply(" 2 "), Some(QuickReply::Detail(2)));
        assert_eq!(
            parse_quick_reply("1 批准"),
            Some(QuickReply::Act(TargetRef::Index(1), QuickAction::Approve))
        );
        assert_eq!(
            parse_quick_reply("#2 拒绝"),
            Some(QuickReply::Act(TargetRef::Index(2), QuickAction::Deny))
        );
        assert_eq!(
            parse_quick_reply("3 中断"),
            Some(QuickReply::Act(TargetRef::Index(3), QuickAction::Interrupt))
        );
        assert_eq!(
            parse_quick_reply("4 看"),
            Some(QuickReply::Act(TargetRef::Index(4), QuickAction::Read))
        );
        assert_eq!(
            parse_quick_reply("2 ok"),
            Some(QuickReply::Act(TargetRef::Index(2), QuickAction::Approve))
        );
        // 非动作词 → 不命中；「1 说 …」是合法的发送动作（Send）
        assert_eq!(parse_quick_reply("1 你好"), None);
        assert_eq!(
            parse_quick_reply("1 说 跑测试"),
            Some(QuickReply::Act(
                TargetRef::Index(1),
                QuickAction::Send("跑测试".into())
            ))
        );
        assert_eq!(parse_quick_reply("abc"), None);
        assert_eq!(parse_quick_reply(""), None);
    }

    /// 回归：「N 发送 文本」必须解析为 Send 快捷动作。
    /// 之前 classify_action 没有 send 词，这条消息石沉大海（落到 AI 层再回菜单）。
    /// 回归：从 agent 输出中提取编号选项（选项转数字）。
    #[test]
    fn test_extract_numbered_choices() {
        // 常见编号格式
        let out = "要哪个品牌？\n1. 一点点\n2. CoCo\n3. 茶百道";
        let c = extract_numbered_choices(out);
        assert_eq!(c, vec!["一点点", "CoCo", "茶百道"]);

        // 中文顿号与括号形式
        let c2 = extract_numbered_choices("选一个：\n1、三分糖\n2）五分糖\n[3] 全糖");
        assert_eq!(c2, vec!["三分糖", "五分糖", "全糖"]);

        // 单行不算选项（避免误伤普通输出）
        assert!(extract_numbered_choices("1. 修好了").is_empty());
        // 无编号 → 空
        assert!(extract_numbered_choices("就选一点点吧").is_empty());
    }

    /// 回归：单行判定与多行提取一致；全角/半角分隔符都能识别。
    #[test]
    fn test_is_numbered_choice_line() {
        assert_eq!(
            is_numbered_choice_line("1. 一点点").as_deref(),
            Some("一点点")
        );
        assert_eq!(
            is_numbered_choice_line("2、三分糖").as_deref(),
            Some("三分糖")
        );
        assert_eq!(
            is_numbered_choice_line("3）五分糖").as_deref(),
            Some("五分糖")
        );
        assert_eq!(is_numbered_choice_line("[4] 全糖").as_deref(), Some("全糖"));
        assert_eq!(is_numbered_choice_line("5) 少冰").as_deref(), Some("少冰"));
        assert!(is_numbered_choice_line("✓ tests passed").is_none());
        assert_eq!(
            is_numbered_choice_line("1. 5 分钟后重试").as_deref(),
            Some("5 分钟后重试")
        );
        assert!(is_numbered_choice_line("").is_none());
    }

    fn one_q(options: &[&str]) -> Vec<crate::herdr::PendingQuestion> {
        vec![crate::herdr::PendingQuestion {
            header: String::new(),
            question: String::new(),
            options: options
                .iter()
                .map(|o| crate::herdr::PendingOption {
                    label: o.to_string(),
                    description: String::new(),
                })
                .collect(),
        }]
    }

    #[tokio::test]
    async fn test_translate_reply_single_and_multi() {
        let mut st = BridgeState::default();
        // 单题
        st.pending_choices
            .insert("w8:p3".into(), one_q(&["一点点", "CoCo"]));
        assert_eq!(
            st.pending_choices.get("w8:p3").unwrap()[0].options[0].label,
            "一点点"
        );
        // 多题
        st.pending_choices.insert(
            "w8:p1".into(),
            vec![
                crate::herdr::PendingQuestion {
                    header: "品牌".into(),
                    question: "q1".into(),
                    options: one_q(&["一点点", "CoCo", "茶百道"])[0].options.clone(),
                },
                crate::herdr::PendingQuestion {
                    header: "糖度".into(),
                    question: "q2".into(),
                    options: one_q(&["全糖", "七分糖"])[0].options.clone(),
                },
            ],
        );
        // 多题翻译：1.2 2.1 → 两行 label
        let q2 = st.pending_choices.get("w8:p1").unwrap().clone();
        let picked: Vec<String> = vec![
            q2[0].options[1].label.clone(),
            q2[1].options[0].label.clone(),
        ];
        assert_eq!(
            picked.join(
                "
"
            ),
            "CoCo
全糖"
        );
        // 越界 / 其他 pane 无选项
        assert!(q2[0].options.get(5).is_none());
        assert!(!st.pending_choices.contains_key("other:p1"));
    }

    #[test]
    fn test_parse_quick_reply_send() {
        assert_eq!(
            parse_quick_reply("4 发送 把测试跑完"),
            Some(QuickReply::Act(
                TargetRef::Index(4),
                QuickAction::Send("把测试跑完".into())
            ))
        );
        assert_eq!(
            parse_quick_reply("2 send run tests"),
            Some(QuickReply::Act(
                TargetRef::Index(2),
                QuickAction::Send("run tests".into())
            ))
        );
        // 发送无内容 → 不命中（落到其他层给用法提示）
        assert_eq!(parse_quick_reply("4 发送"), None);
    }

    /// 回归：pane ID + 动作词必须解析为 PaneId 目标（不受菜单重排影响）。
    #[test]
    fn test_parse_quick_reply_pane_id() {
        assert_eq!(
            parse_quick_reply("w8:p3 批准"),
            Some(QuickReply::Act(
                TargetRef::PaneId("w8:p3".into()),
                QuickAction::Approve
            ))
        );
        assert_eq!(
            parse_quick_reply("W8:PM 看"),
            Some(QuickReply::Act(
                TargetRef::PaneId("W8:PM".into()),
                QuickAction::Read
            ))
        );
        assert_eq!(
            parse_quick_reply("w8:p1 发送 你好"),
            Some(QuickReply::Act(
                TargetRef::PaneId("w8:p1".into()),
                QuickAction::Send("你好".into())
            ))
        );
        // 非 pane ID 形式的字母开头 → 不命中
        assert_eq!(parse_quick_reply("world 批准"), None);
    }

    /// 回归：连写发送「发送N+内容」/「发送w8:p3:内容」必须解析。
    #[test]
    fn test_parse_compact_send() {
        assert_eq!(
            parse_quick_reply("发送1问我中午吃什么"),
            Some(QuickReply::Act(
                TargetRef::Index(1),
                QuickAction::Send("问我中午吃什么".into())
            ))
        );
        assert_eq!(
            parse_quick_reply("发送2：把测试跑完"),
            Some(QuickReply::Act(
                TargetRef::Index(2),
                QuickAction::Send("把测试跑完".into())
            ))
        );
        assert_eq!(
            parse_quick_reply("说3检查一下"),
            Some(QuickReply::Act(
                TargetRef::Index(3),
                QuickAction::Send("检查一下".into())
            ))
        );
        assert_eq!(
            parse_quick_reply("发送w8:p3:你好"),
            Some(QuickReply::Act(
                TargetRef::PaneId("w8:p3".into()),
                QuickAction::Send("你好".into())
            ))
        );
        // 无内容 → 不命中
        assert_eq!(parse_quick_reply("发送1"), None);
        // 纯数字无动作词 → 详情卡，不是发送
        assert_eq!(parse_quick_reply("1"), Some(QuickReply::Detail(1)));
    }

    // ── 文案 ──────────────────────────────────────────────────────────

    #[test]
    fn test_fmt_agents_menu() {
        let menu = sample_menu();
        let text = fmt_agents_menu(&menu);
        assert!(text.contains("Herdr Agent 状态（3）"));
        assert!(
            text.contains("1️⃣ ✅ FluidBar · pi"),
            "spaces 模式 #1 是 list 首个（idle FluidBar）"
        );
        assert!(
            text.contains("2️⃣ 🚨 wxherd · pi"),
            "#2 是 blocked 的 wxherd"
        );
        assert!(text.contains("等待批准"));
        assert!(text.contains("3️⃣ 🟢 FluidBar2 · pi"));
        assert!(text.contains("回复编号看详情"));
    }

    #[test]
    fn test_fmt_detail_and_status() {
        let menu = sample_menu();
        let detail = fmt_detail(&menu[1], "line1\nline2");
        assert!(detail.contains("#2 wxherd · pi"));
        assert!(detail.contains("pane：`w8:p3`"));
        assert!(detail.contains("等待批准"));
        assert!(detail.contains("```\nline1\nline2\n```"));

        let summary = fmt_status_summary(&menu);
        assert!(summary.contains("🚨 等待批准：1"));
        assert!(summary.contains("🟢 工作中：1"));
        assert!(summary.contains("✅ 空闲：1"));
        assert!(summary.contains("需要处理"));
    }

    #[test]
    fn test_fmt_blocked_and_done_push() {
        let menu = sample_menu();
        // menu[1] 是 blocked 的 wxherd（spaces 模式下序号 2）
        let (push, choices) = fmt_blocked_push(&menu[1], "output line", Some(2), None);
        assert!(choices.is_empty(), "无编号选项时 choices 为空");
        assert!(push.contains("🚨 **Herdr 需要你**"));
        assert!(push.contains("状态：等待批准"));
        assert!(push.contains("`2 批准` 批准"));
        assert!(push.contains("`/approve w8:p3` 批准"));
        assert!(push.contains("output line"));

        let (no_idx, _) = fmt_blocked_push(&menu[1], "x", None, None);
        assert!(!no_idx.contains(" 批准` 批准\n") || no_idx.matches("`/approve").count() == 1);

        let done = fmt_done_push(&menu[2], 153_000, "all tests passed");
        assert!(done.contains("✅ **任务完成**"));
        assert!(done.contains("耗时：2m 33s"));
        assert!(done.contains("all tests passed"));
        // 菜单序号必须出现在完成推送里，且与 /agents 菜单同号
        assert!(
            done.contains("3️⃣"),
            "完成推送首行应带菜单序号 emoji：{done}"
        );
        assert!(!done.contains("看详情"), "不应再有重复的看详情提示");
        assert!(
            done.starts_with("✅"),
            "首行保持完成标题，序号由第二行 emoji 体现"
        );
        assert!(
            done.contains("3\u{fe0f}\u{20e3}"),
            "完成推送第二行应带菜单序号 emoji：{done}"
        );
    }

    #[test]
    fn test_fmt_duration() {
        assert_eq!(fmt_duration(42_000), "42s");
        assert_eq!(fmt_duration(153_000), "2m 33s");
        assert_eq!(fmt_duration(7_500_000), "2h 05m");
        assert_eq!(fmt_duration(93_600_000), "1d 02h");
    }

    #[test]
    fn test_num_emoji() {
        assert_eq!(num_emoji(1), "1️⃣");
        assert_eq!(num_emoji(9), "9️⃣");
        assert_eq!(num_emoji(10), "🔟");
        assert_eq!(num_emoji(11), "11.");
    }

    // ── 通知去重引擎 ──────────────────────────────────────────────────

    fn notify_cfg() -> NotifyConfig {
        NotifyConfig {
            blocked: true,
            done: true,
            notify_when_focused: false,
            cooldown_seconds: 5,
            min_done_duration_seconds: 0,
            quiet_hours: vec![],
            blocked_delay_seconds: 10,
        }
    }

    #[test]
    fn test_quiet_hours_parse_and_match() {
        assert_eq!(parse_quiet_range("22:00-08:00"), Some((1320, 480)));
        assert_eq!(parse_quiet_range("09:30-12:00"), Some((570, 720)));
        assert_eq!(parse_quiet_range("bad"), None);
        assert_eq!(parse_quiet_range("25:00-08:00"), None);

        assert!(in_quiet_hours(23 * 60, &[(1320, 480)]));
        assert!(in_quiet_hours(5 * 60, &[(1320, 480)]));
        assert!(!in_quiet_hours(12 * 60, &[(1320, 480)]));
        assert!(in_quiet_hours(600, &[(570, 720)]));
        assert!(!in_quiet_hours(500, &[(570, 720)]));
        assert!(in_quiet_hours(0, &[(1320, 480)]), "跨天区间含 0 点");
    }

    // ── 长 Markdown 安全分片 ──────────────────────────────────────────

    #[test]
    fn test_split_markdown_chunks_short_text_unchanged() {
        assert_eq!(
            split_markdown_chunks("短文本", MAX_MARKDOWN_BYTES),
            vec!["短文本".to_string()]
        );
        assert_eq!(
            split_markdown_chunks("", MAX_MARKDOWN_BYTES),
            vec![String::new()]
        );
    }

    #[test]
    fn test_split_markdown_chunks_respects_byte_limit() {
        let text: String = (0..500)
            .map(|i| format!("第{i}行 some ascii padding content here\n"))
            .collect();
        let chunks = split_markdown_chunks(&text, MAX_MARKDOWN_BYTES);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= MAX_MARKDOWN_BYTES, "片超限：{} 字节", c.len());
        }
        // 无代码块时重组应还原原文（换片引入的换行除外，逐行比对）
        let rejoined = chunks.join("\n");
        assert_eq!(rejoined, text.trim_end_matches('\n'));
    }

    #[test]
    fn test_split_markdown_chunks_utf8_boundary() {
        // 单行超预算的多字节文本：切点不得落在字符中间
        let line = "中文内容测试".repeat(120); // 每字符 3 字节，单行 2160 字节
        let text = format!("{line}\n{line}\n{line}");
        let chunks = split_markdown_chunks(&text, MAX_MARKDOWN_BYTES);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= MAX_MARKDOWN_BYTES, "片超限：{} 字节", c.len());
            // String 类型本身保证合法 UTF-8；另验证切点字符完整：每片首行/末行均为完整字符序列
            assert!(c.is_char_boundary(0));
            assert!(c.is_char_boundary(c.len()));
        }
        // 所有字符完好：重组后字符集合与原文一致
        let rejoined = chunks.join("\n");
        assert_eq!(
            rejoined.chars().filter(|c| *c != '\n').count(),
            text.chars().filter(|c| *c != '\n').count()
        );
    }

    #[test]
    fn test_split_markdown_chunks_utf8_multibyte_at_cut_point() {
        // 构造：ASCII 填充到恰好把多字节字符骑在预算边界上
        let budget = MAX_MARKDOWN_BYTES - 8;
        let padding = "a".repeat(budget - 1); // 边界前 1 字节：中文字符必然骑在边界上
        let text = format!("{padding}中文\n第二行中文内容\n");
        let chunks = split_markdown_chunks(&text, MAX_MARKDOWN_BYTES);
        for c in &chunks {
            assert!(c.len() <= MAX_MARKDOWN_BYTES);
        }
        let rejoined = chunks.join("\n");
        assert!(rejoined.contains("中文"));
        assert!(rejoined.contains("第二行中文内容"));
    }

    #[test]
    fn test_split_markdown_chunks_code_fence_closed_and_reopened() {
        let body: String = (0..200)
            .map(|i| format!("code line {i:04} padding padding\n"))
            .collect();
        let text = format!("```\n{body}```\n尾部\n");
        let chunks = split_markdown_chunks(&text, MAX_MARKDOWN_BYTES);
        assert!(chunks.len() > 1);
        // 第一片以闭合围栏结尾；中间片首尾都有围栏；末片以重开围栏开头且含尾部内容
        assert!(chunks[0].ends_with("```"));
        for c in &chunks[1..chunks.len() - 1] {
            assert!(c.starts_with("```"), "中间片未重开围栏");
            assert!(c.ends_with("```"));
        }
        let last = chunks.last().unwrap();
        assert!(last.starts_with("```") && last.contains("尾部"));
        for c in &chunks {
            assert!(c.len() <= MAX_MARKDOWN_BYTES, "片超限：{} 字节", c.len());
        }
    }

    #[test]
    fn test_split_markdown_chunks_small_budget() {
        let text = "你好世界".repeat(20); // 160 字节
        let chunks = split_markdown_chunks(&text, 50);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.len() <= 50, "片超限：{} 字节", c.len());
        }
        let rejoined = chunks.join("\n");
        assert_eq!(rejoined.chars().filter(|c| *c != '\n').count(), 80);
    }

    #[test]
    fn test_notify_blocked_delay_then_once() {
        let cfg = notify_cfg();
        // 首次见到 blocked：进入时间 = now，未到延迟 → 不推
        let (st, push) = evaluate_notification(None, "blocked", false, 10_000, 600, &cfg);
        assert_eq!(push, None);
        // 延迟已过（10s）→ 推 Blocked
        let (st, push) = evaluate_notification(Some(&st), "blocked", false, 21_000, 600, &cfg);
        assert_eq!(push, Some(NotifyKind::Blocked));
        assert_eq!(st.last_blocked_push_ms, 21_000);
        // 同周期再评估 → 不再推（去重）
        let (st, push) = evaluate_notification(Some(&st), "blocked", false, 120_000, 600, &cfg);
        assert_eq!(push, None, "同 blocked 周期只推一次");
        let _ = st;
    }

    #[test]
    fn test_notify_done_transition_and_not_from_unknown() {
        let cfg = notify_cfg();
        // blocked → idle：推完成并带时长
        let prev = PaneNotifyState {
            last_status: "blocked".into(),
            entered_status_ms: 100_000,
            last_blocked_push_ms: 105_000,
            last_done_push_ms: 0,
        };
        let (st, push) = evaluate_notification(Some(&prev), "idle", false, 250_000, 600, &cfg);
        assert_eq!(
            push,
            Some(NotifyKind::Done {
                duration_ms: 150_000
            })
        );
        assert_eq!(st.last_status, "idle");
        // unknown → idle：不推完成
        let prev = PaneNotifyState {
            last_status: "unknown".into(),
            entered_status_ms: 100_000,
            ..prev.clone()
        };
        let (_, push) = evaluate_notification(Some(&prev), "idle", false, 120_000, 600, &cfg);
        assert_eq!(push, None);
        // idle → working：无推送
        let prev = PaneNotifyState {
            last_status: "idle".into(),
            entered_status_ms: 100_000,
            ..prev.clone()
        };
        let (_, push) = evaluate_notification(Some(&prev), "working", false, 120_000, 600, &cfg);
        assert_eq!(push, None);
    }

    #[test]
    fn test_notify_done_status_ends_turn() {
        let cfg = notify_cfg();
        // herdr 0.9.5 结束态为 done：working → done 必须推完成
        let prev = PaneNotifyState {
            last_status: "working".into(),
            entered_status_ms: 100_000,
            last_blocked_push_ms: 0,
            last_done_push_ms: 0,
        };
        let (st, push) = evaluate_notification(Some(&prev), "done", false, 360_000, 600, &cfg);
        assert_eq!(
            push,
            Some(NotifyKind::Done {
                duration_ms: 260_000
            })
        );
        assert_eq!(st.last_status, "done");
        assert_eq!(st.last_done_push_ms, 360_000);
        // done → idle：不重复推（prev 已不是 working/blocked）
        let (_, push) = evaluate_notification(Some(&st), "idle", false, 200_000, 600, &cfg);
        assert_eq!(push, None, "done → idle 不得二次推送");
        // blocked → done：同样推完成（批准后一轮内跑完的场景）
        let prev = PaneNotifyState {
            last_status: "blocked".into(),
            entered_status_ms: 100_000,
            last_blocked_push_ms: 105_000,
            last_done_push_ms: 0,
        };
        let (_, push) = evaluate_notification(Some(&prev), "done", false, 380_000, 600, &cfg);
        assert_eq!(
            push,
            Some(NotifyKind::Done {
                duration_ms: 280_000
            })
        );
    }

    #[test]
    fn test_notify_quiet_hours_and_focused() {
        let cfg = notify_cfg();
        // 免打扰时段：不推但记录推送时间（避免时段结束补推）
        let (st, push) = evaluate_notification(None, "blocked", false, 0, 22 * 60 + 30, &cfg);
        assert_eq!(push, None);
        let prev = PaneNotifyState {
            last_status: "blocked".into(),
            entered_status_ms: 0,
            last_blocked_push_ms: 11_000, // 已在免打扰时段记录
            last_done_push_ms: 0,
        };
        let (_, push) =
            evaluate_notification(Some(&prev), "blocked", false, 60_000, 22 * 60 + 30, &cfg);
        assert_eq!(push, None, "免打扰内不重复推");
        let _ = st;

        // 前台 pane 且 notify_when_focused=false → 不推
        let (st, push) = evaluate_notification(None, "blocked", true, 0, 600, &cfg);
        assert_eq!(push, None);
        let (st2, push) = evaluate_notification(Some(&st), "blocked", true, 30_000, 600, &cfg);
        assert_eq!(push, None);
        let _ = st2;
    }

    #[test]
    fn test_notify_state_roundtrip() {
        let mut st = NotifyState::default();
        st.panes.insert(
            "w8:p3".into(),
            PaneNotifyState {
                last_status: "blocked".into(),
                entered_status_ms: 1,
                last_blocked_push_ms: 2,
                last_done_push_ms: 3,
            },
        );
        let json = serde_json::to_string(&st).unwrap();
        let parsed: NotifyState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, st);
        let empty: NotifyState = serde_json::from_str("{}").unwrap();
        assert!(empty.panes.is_empty());
    }

    // ── 媒体命名 ──────────────────────────────────────────────────────

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("report final.pdf"), "report final.pdf");
        assert_eq!(
            sanitize_filename(r"..\evil\path\name.txt"),
            "evilpathname.txt"
        );
        assert_eq!(sanitize_filename("a/b:c*d?.txt"), "abcd.txt");
        assert_eq!(sanitize_filename("  ..  "), "file");
        assert_eq!(sanitize_filename(""), "file");
    }

    // ── 设置命令解析 ──────────────────────────────────────────────

    #[test]
    fn test_parse_settings_notify_and_clear() {
        assert_eq!(
            parse_settings_command("通知 on"),
            Some(SettingsCommand::NotifyOn)
        );
        assert_eq!(
            parse_settings_command("通知 OFF"),
            Some(SettingsCommand::NotifyOff)
        );
        assert_eq!(
            parse_settings_command("通知开"),
            Some(SettingsCommand::NotifyOn)
        );
        assert_eq!(
            parse_settings_command("通知 关闭"),
            Some(SettingsCommand::NotifyOff)
        );
        assert_eq!(parse_settings_command("通知"), None);
        assert_eq!(parse_settings_command("通知 unknown"), None);

        assert_eq!(
            parse_settings_command("规则清空"),
            Some(SettingsCommand::RulesClear)
        );
        assert_eq!(
            parse_settings_command("清空学习规则"),
            Some(SettingsCommand::RulesClear)
        );
        assert_eq!(
            parse_settings_command("确认"),
            Some(SettingsCommand::ConfirmClear)
        );
        assert_eq!(
            parse_settings_command("取消"),
            Some(SettingsCommand::Cancel)
        );
        assert_eq!(
            parse_settings_command("取消静音"),
            Some(SettingsCommand::Unmute)
        );
        // 非设置命令不拦截
        assert_eq!(parse_settings_command("/approve 1"), None);
        assert_eq!(parse_settings_command("看看第二个在干什么"), None);
    }

    #[test]
    fn test_parse_mute_hours() {
        let now = chrono::Local::now().timestamp_millis().max(0) as u64;
        match parse_settings_command("静音1小时") {
            Some(SettingsCommand::Mute { until_ms }) => {
                let delta = until_ms as i64 - now as i64;
                assert!(
                    (3_600_000 - delta).abs() < 5_000,
                    "静音 1 小时偏差过大：{delta}ms"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_settings_command("静音两小时"),
            Some(SettingsCommand::Mute { .. })
        ));
        assert!(matches!(
            parse_settings_command("静音 2 小时"),
            Some(SettingsCommand::Mute { .. })
        ));
        assert!(matches!(
            parse_settings_command("静音"),
            Some(SettingsCommand::Mute { .. })
        ));
        assert_eq!(parse_settings_command("静音0小时"), None);
        assert_eq!(parse_settings_command("静音到25点"), None);
        assert_eq!(parse_settings_command("静音到bad"), None);
    }

    #[test]
    fn test_parse_mute_until_hour() {
        use chrono::{Local, TimeZone, Timelike};
        for (text, expect) in [
            ("静音到9点", (9u32, 0u32)),
            ("静音到 9 点半", (9, 30)),
            ("静音到二十三点", (23, 0)),
        ] {
            match parse_settings_command(text) {
                Some(SettingsCommand::Mute { until_ms }) => {
                    let dt = Local
                        .timestamp_millis_opt(until_ms as i64)
                        .single()
                        .unwrap();
                    assert_eq!((dt.hour(), dt.minute()), expect, "{text} 应落在目标时刻");
                    assert!(until_ms as i64 > Local::now().timestamp_millis());
                }
                other => panic!("{other:?}"),
            }
        }
        // 半点单独验证
        match parse_settings_command("静音到9点半") {
            Some(SettingsCommand::Mute { until_ms }) => {
                let dt = Local
                    .timestamp_millis_opt(until_ms as i64)
                    .single()
                    .unwrap();
                assert_eq!((dt.hour(), dt.minute()), (9, 30));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn test_intent_is_actionable() {
        assert!(Intent::Approve { target: None }.is_actionable());
        assert!(Intent::Status.is_actionable());
        assert!(!Intent::Unknown.is_actionable());
        assert!(!Intent::Send {
            target: None,
            text: "  ".into()
        }
        .is_actionable());
        assert!(Intent::Send {
            target: None,
            text: "继续".into()
        }
        .is_actionable());
    }

    #[test]
    fn test_media_file_name() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 24, 9, 30, 5)
            .unwrap();
        assert_eq!(
            media_file_name(&now, Some("log.txt")),
            "20261024-093005_log.txt"
        );
        assert_eq!(media_file_name(&now, None), "20261024-093005_file");
        assert_eq!(media_file_name(&now, Some("??")), "20261024-093005_file");
    }
}
