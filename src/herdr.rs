//! herdr CLI 封装：pane list/read/send-text/send-keys 子进程调用与 JSON 解析、进程探活/终止。
//!
//! 本模块只依赖协议无关的工具，不依赖 bridge/daemon/ilink。

use std::process::Command;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// /read 默认快照源：recent-unwrapped 去掉 TUI 边框换行，清洗效果最好。
pub const READ_SOURCE: &str = "recent-unwrapped";

/// herdr pane list 返回的单个 pane 快照（agent 运行状态）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PaneSnapshot {
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focused: bool,
    /// pi 会话文件路径（jsonl）：done 推送取真实回复用，替代 TUI 画面清洗。
    #[serde(default)]
    pub agent_session: Option<AgentSessionRef>,
}

/// agent 会话引用（herdr pane list 返回）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentSessionRef {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

// ---------------------------------------------------------------------------
// JSON 解析（纯函数，便于单测）
// ---------------------------------------------------------------------------

/// herdr CLI 输出形如 `{"id":"cli:pane:list","result":{...}}` 的 JSON-RPC 信封；
/// 提取 result 字段（无信封时原样返回）。
pub fn parse_envelope(stdout: &str) -> Result<serde_json::Value> {
    let text = stdout.trim();
    let start = text.find('{').context("herdr 输出中不包含 JSON")?;
    let value: serde_json::Value = serde_json::from_str(&text[start..]).with_context(|| {
        format!(
            "herdr 输出 JSON 解析失败：{}",
            &text[start..text.len().min(200)]
        )
    })?;
    Ok(value.get("result").cloned().unwrap_or(value))
}

/// 解析 `herdr pane list` 输出为 pane 快照列表。
pub fn parse_pane_list(stdout: &str) -> Result<Vec<PaneSnapshot>> {
    let result = parse_envelope(stdout)?;
    let panes = result
        .get("panes")
        .context("herdr pane list 输出缺少 panes 字段")?;
    Ok(serde_json::from_value(panes.clone())?)
}

// ---------------------------------------------------------------------------
// 子进程调用
// ---------------------------------------------------------------------------

/// Windows 下禁止子进程弹出控制台窗口（守护进程高频调用，否则每 2s 闪一个黑框）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 运行 herdr 子命令，返回 stdout。非零退出码报错（带 stderr）。
pub async fn run(bin: &str, args: &[&str]) -> Result<String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .context("herdr 调用超时（15s）")?
        .with_context(|| format!("无法启动 {bin}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("herdr {} 失败：{}", args.join(" "), stderr.trim());
    }
    Ok(stdout)
}

/// 获取 agent pane 列表快照。
pub async fn pane_list(bin: &str) -> Result<Vec<PaneSnapshot>> {
    let out = run(bin, &["pane", "list"]).await?;
    parse_pane_list(&out)
}

/// 读取 pane 最近输出（--source recent-unwrapped --format text）。
pub async fn pane_read(bin: &str, pane_id: &str, lines: u32) -> Result<String> {
    let lines = lines.clamp(1, 200).to_string();
    run(
        bin,
        &[
            "pane",
            "read",
            pane_id,
            "--source",
            READ_SOURCE,
            "--lines",
            &lines,
            "--format",
            "text",
        ],
    )
    .await
}

/// 向 pane 注入字面文本（不带回车）。
pub async fn pane_send_text(bin: &str, pane_id: &str, text: &str) -> Result<()> {
    run(bin, &["pane", "send-text", pane_id, text])
        .await
        .map(|_| ())
}

/// 向 pane 发送按键序列（如 Enter、Ctrl+c）。
pub async fn pane_send_keys(bin: &str, pane_id: &str, keys: &[&str]) -> Result<()> {
    let mut args: Vec<&str> = vec!["pane", "send-keys", pane_id];
    args.extend_from_slice(keys);
    run(bin, &args).await.map(|_| ())
}

/// 注入文本并回车（herdr pane send-text 后补 Enter）。
pub async fn pane_send_line(bin: &str, pane_id: &str, text: &str) -> Result<()> {
    pane_send_text(bin, pane_id, text).await?;
    pane_send_keys(bin, pane_id, &["Enter"]).await
}

// ---------------------------------------------------------------------------
// 进程探活 / 终止（Windows: tasklist/taskkill；Unix: kill -0 / kill -TERM）
// ---------------------------------------------------------------------------

/// 从 pi 会话文件（jsonl）尾部读取最后一条 assistant 文本回复。
///
/// done 推送的「末次输出」优先用这个，而不是 TUI 画面清洗：
/// 画面里混着状态栏/进度条/模型名等装饰，黑名单过滤追不完（不同
/// agent/插件/主题千变万化）；jsonl 是 pi 自己的稳定格式，
/// assistant 的 text 块就是干净的回复原文。
/// 非 pi agent 或文件不可读时返回 None（调用方回退到画面清洗）。
pub fn last_assistant_text(session_path: &str, max_chars: usize) -> Option<String> {
    use std::io::{BufRead, Seek, SeekFrom};
    let file = std::fs::File::open(session_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    // 从尾部读：会话文件可达数 MB，全量解析浪费；倒序找最近的 assistant text
    let len = reader.seek(SeekFrom::End(0)).ok()?;
    let tail_len = len.min(256 * 1024); // 最多看尾部 256KB
    reader.seek(SeekFrom::Start(len - tail_len)).ok()?;
    let mut found: Option<String> = None;
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(msg) = v.get("message") else {
            continue;
        };
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
            if !text.trim().is_empty() {
                found = Some(text.trim().to_string());
            }
            continue;
        }
        // content 也可能是块数组 [{type:"text", text:"..."}]
        if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
            let text = blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.trim().is_empty() {
                found = Some(text.trim().to_string());
            }
        }
    }
    found.map(|t| {
        if t.chars().count() > max_chars {
            t.chars().take(max_chars).collect::<String>() + "…"
        } else {
            t
        }
    })
}

/// 从 pi 会话文件（jsonl）读最近 `count` 条 assistant 文本（时间倒序，最新在前）。
/// 全文模式的取数来源：不受终端快照行数限制，内容为模型回复原文。
pub fn recent_assistant_texts(session_path: &str, count: usize) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(session_path) else {
        return Vec::new();
    };
    let mut texts: Vec<String> = Vec::new();
    for line in content.lines().rev() {
        if texts.len() >= count {
            break;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // 兼容多种 agent 的 jsonl 格式：
        // - pi:      {"message": {"role": "assistant", "content": ...}}
        // - claude:  {"type": "assistant", "message": {"role": "assistant", "content": ...}}
        // role 不在 message 里时回退看顶层 type 字段
        let Some(msg) = v.get("message") else {
            continue;
        };
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .or_else(|| v.get("type").and_then(|t| t.as_str()))
            .unwrap_or_default();
        if role != "assistant" {
            continue;
        }
        let text = match msg.get("content") {
            Some(c) if c.is_string() => c.as_str().unwrap_or("").to_string(),
            Some(c) if c.is_array() => c
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                                b.get("text").and_then(|t| t.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(
                            "
",
                        )
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        if !text.trim().is_empty() {
            texts.push(text.trim().to_string());
        }
    }
    texts
}

/// 一次多问题问卷（ask_user_question）的解析结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingQuestionnaire {
    /// 每题：题头 + 问题文本 + 选项（label + description）。
    pub questions: Vec<PendingQuestion>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingQuestion {
    pub header: String,
    pub question: String,
    pub options: Vec<PendingOption>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingOption {
    pub label: String,
    pub description: String,
}

/// 从会话文件尾部找最近的 ask_user_question toolCall，解析全部问题与选项。
/// toolCall 之后若已有 toolResult（用户已答），返回 None。
/// 找不到 / 文件不可读 → None（调用方回退画面清洗）。
pub fn pending_questionnaire(session_path: &str) -> Option<PendingQuestionnaire> {
    let content = std::fs::read_to_string(session_path).ok()?;
    let mut last_call: Option<serde_json::Value> = None;
    let mut answered_after_call = false;
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(msg) = v.get("message") else {
            continue;
        };
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .or_else(|| v.get("type").and_then(|t| t.as_str()))
            .unwrap_or_default();
        if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
            for b in blocks {
                match (role, b.get("type").and_then(|t| t.as_str())) {
                    ("assistant", Some("toolCall")) => {
                        if b.get("name").and_then(|n| n.as_str()) == Some("ask_user_question") {
                            last_call = b.get("arguments").cloned();
                            answered_after_call = false; // 新问卷，重置回答标记
                        }
                    }
                    ("toolResult", _) if last_call.is_some() => {
                        let text = b
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                            .to_lowercase();
                        // 回答/取消都算这轮问卷已结束
                        if text.contains("answered")
                            || text.contains("cancel")
                            || text.contains("decline")
                        {
                            answered_after_call = true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // 最后一个 toolCall 必须还没有 toolResult（正等待回答）
    if last_call.is_none() || answered_after_call {
        return None;
    }
    let args = last_call?;
    let qs = args.get("questions")?.as_array()?;
    let mut questions = Vec::new();
    for q in qs {
        let header = q
            .get("header")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        let question = q
            .get("question")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let mut options = Vec::new();
        if let Some(opts) = q.get("options").and_then(|o| o.as_array()) {
            for o in opts {
                options.push(PendingOption {
                    label: o
                        .get("label")
                        .and_then(|l| l.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    description: o
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }
        if !options.is_empty() {
            questions.push(PendingQuestion {
                header,
                question,
                options,
            });
        }
    }
    if questions.is_empty() {
        return None;
    }
    Some(PendingQuestionnaire { questions })
}

/// Windows 探活：`tasklist` 按 PID 过滤。
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    let mut cmd = Command::new("tasklist");
    cmd.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let Ok(output) = cmd.output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // 无匹配时 tasklist 输出提示文本（不含该 PID）；有匹配时 CSV 行含 PID 数字
    stdout.contains(&pid.to_string())
}

/// Unix 探活：`kill -0`（进程存在且有权发信号即视为存活）。
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Windows 终止进程树：`taskkill /PID <pid> /T /F`。
#[cfg(windows)]
pub fn pid_kill(pid: u32) -> Result<()> {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = cmd.output().context("无法启动 taskkill")?;
    if !output.status.success() {
        bail!(
            "taskkill 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Unix 终止：先 SIGTERM 让守护进程自己收尾，2 秒内没退再 SIGKILL。
///
/// 注意：只杀主进程，不递归杀子进程树（守护进程自身不派生子进程，
/// 仅在执行 herdr 命令期间短暂持有短命子进程，随父进程退出即结束）。
#[cfg(unix)]
pub fn pid_kill(pid: u32) -> Result<()> {
    let signal = |sig: &str| {
        Command::new("kill")
            .args([sig, &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !signal("-TERM") {
        bail!("kill -TERM {pid} 失败");
    }
    for _ in 0..20 {
        if !pid_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !signal("-KILL") {
        bail!("kill -KILL {pid} 失败");
    }
    for _ in 0..10 {
        if !pid_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("进程 {pid} 在 SIGKILL 后仍存活")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 `herdr pane list` 输出（截取两个 pane）。
    const SAMPLE: &str = r#"{"id":"cli:pane:list","result":{"panes":[{"agent":"pi","agent_status":"idle","cwd":"E:\\model\\FluidBar","focused":false,"pane_id":"w8:p1","revision":11,"state_labels":{"blocked":"waiting"},"tab_id":"w8:t1","terminal_id":"term_1","terminal_title":"π - FluidBar","terminal_title_stripped":"π - FluidBar","workspace_id":"w8"},{"agent":"pi","agent_status":"blocked","cwd":"E:\\wxherd","focused":true,"pane_id":"w8:p3","revision":20,"state_labels":{"blocked":"waiting"},"tab_id":"w8:t3","terminal_id":"term_2","terminal_title":"π - wxherd","terminal_title_stripped":"π - wxherd","workspace_id":"w8"},{"agent_status":"unknown","cwd":"E:\\model\\FluidBar","focused":false,"pane_id":"w8:p5","revision":1,"terminal_id":"term_3","terminal_title":"pwsh","terminal_title_stripped":"pwsh","workspace_id":"w8"}],"type":"pane_list"}}"#;

    #[test]
    fn test_parse_pane_list_real_sample() {
        let panes = parse_pane_list(SAMPLE).expect("真实样例必须可解析");
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].pane_id, "w8:p1");
        assert_eq!(panes[0].agent.as_deref(), Some("pi"));
        assert_eq!(panes[0].agent_status.as_deref(), Some("idle"));
        assert_eq!(panes[0].cwd.as_deref(), Some(r"E:\model\FluidBar"));
        assert!(!panes[0].focused);
        assert_eq!(panes[1].agent_status.as_deref(), Some("blocked"));
        assert!(panes[1].focused);
        // 无 agent 的普通终端 pane 也要保留（agent 字段缺失）
        assert!(panes[2].agent.is_none());
        assert_eq!(panes[2].agent_status.as_deref(), Some("unknown"));
    }

    #[test]
    fn test_parse_envelope_without_wrapper() {
        let v = parse_envelope(r#"{"panes":[]}"#).unwrap();
        assert!(v.get("panes").is_some());
        let v = parse_envelope("前置日志行\n{\"result\":{\"ok\":1}}").unwrap();
        assert_eq!(v.get("ok").and_then(|x| x.as_i64()), Some(1));
    }

    #[test]
    fn test_parse_pane_list_rejects_garbage() {
        assert!(parse_pane_list("not json").is_err());
        assert!(
            parse_pane_list(r#"{"id":"x"}"#).is_err(),
            "缺少 panes 字段必须报错"
        );
    }

    #[test]
    fn test_pane_read_line_clamp_in_args() {
        // pane_read 的行数经 clamp 后拼进参数；这里只验证逻辑函数本身可用
        let lines = 999u32.clamp(1, 200).to_string();
        assert_eq!(lines, "200");
    }

    /// 回归：会话文件里 user/tool 等非 assistant 行不得让函数提前返回 None。
    /// 之前 `?` 作用在 Option 上，遇到第一条 user 行整个函数就返回 None。
    #[test]
    fn test_last_assistant_text_mixed_lines() {
        let dir = std::env::temp_dir().join(format!("wxherd-session-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"问题\"}}
",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"第一版回复\"}]}}
",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"继续\"}}
",
                "{\"type\":\"message\",\"message\":{\"role\":\"toolResult\",\"content\":\"工具输出\"}}
",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"最终回复\"}]}}
",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"后续\"}}
"
            ),
        )
        .unwrap();
        let got = last_assistant_text(path.to_str().unwrap(), 200);
        assert_eq!(
            got.as_deref(),
            Some("最终回复"),
            "混合行序应取最后一条 assistant 文本"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 字符串 content 形式同样支持；截断生效。
    #[test]
    fn test_last_assistant_text_string_content_and_truncate() {
        let dir = std::env::temp_dir().join(format!("wxherd-session-test2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\"一句话回复\"}}
",
        )
        .unwrap();
        assert_eq!(
            last_assistant_text(path.to_str().unwrap(), 200).as_deref(),
            Some("一句话回复")
        );
        assert_eq!(
            last_assistant_text(path.to_str().unwrap(), 3).as_deref(),
            Some("一句话…"),
            "超长截断加省略号"
        );
        // 不存在的文件 → None（调用方回退画面清洗）
        assert_eq!(last_assistant_text("Z:/不存在/x.jsonl", 10), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 回归：Claude Code 格式（顶层 type 字段 + message.content 块）同样可解析。
    #[test]
    fn test_last_assistant_text_claude_format() {
        let dir = std::env::temp_dir().join(format!("wxherd-session-test3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("claude.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}
",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"claude 回复\"}]}}
"
            ),
        )
        .unwrap();
        assert_eq!(
            last_assistant_text(path.to_str().unwrap(), 200).as_deref(),
            Some("claude 回复")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 回归：真实 pi toolCall 结构的 ask_user_question（两题）完整解析。
    #[test]
    fn test_pending_questionnaire_two_questions() {
        let dir = std::env::temp_dir().join(format!("wxherd-qn-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let tool_call = r#"{"type": "message", "message": {"role": "assistant", "content": [{"type": "toolCall", "id": "c1", "name": "ask_user_question", "arguments": {"questions": [{"header": "本地改动", "question": "本地有未提交改动，拉取时如何处理？", "options": [{"label": "stash 暂存后拉取（推荐）", "description": "git stash -u"}, {"label": "先提交再拉取", "description": "pull --rebase"}]}, {"header": "合并方式", "question": "用哪种方式？", "options": [{"label": "rebase（推荐）", "description": "线性历史"}, {"label": "merge", "description": "合并提交"}]}]}}]}}"#;
        std::fs::write(&path, tool_call).unwrap();
        let qn = pending_questionnaire(path.to_str().unwrap());
        println!("DEBUG qn = {qn:?}");
        let qn = qn.expect("应有待答问卷");
        assert_eq!(qn.questions.len(), 2);
        assert_eq!(qn.questions[0].header, "本地改动");
        assert_eq!(qn.questions[0].options.len(), 2);
        assert_eq!(qn.questions[0].options[0].label, "stash 暂存后拉取（推荐）");
        assert_eq!(qn.questions[1].options[1].label, "merge");

        // 用户已回答（toolResult 带 answered）→ 不再是待答问卷
        std::fs::write(
            &path,
            format!("{tool_call}
{{\"type\":\"message\",\"message\":{{\"role\":\"toolResult\",\"content\":[{{\"type\":\"text\",\"text\":\"User has answered your questions\"}}]}}}}
"),
        )
        .unwrap();
        assert!(
            pending_questionnaire(path.to_str().unwrap()).is_none(),
            "已回答的问卷不算待答"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
