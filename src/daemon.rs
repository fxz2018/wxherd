//! 守护进程：收消息循环（长轮询/游标/退避/-14 静默）、通知扫描循环（状态快照 diff）、
//! start/stop 进程管理（Windows: DETACHED_PROCESS + tasklist/taskkill；Unix: spawn + kill）。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Timelike;
use fs2::FileExt;

use crate::bridge::{
    self, clean_terminal_output, collect_agents, evaluate_notification, finalize_menu,
    fmt_blocked_push, fmt_done_push, split_markdown_chunks, NotifyKind, NotifyState,
};
use crate::config::{Config, Credentials, RuntimeState, StatePaths, SyncState};
use crate::herdr;
use crate::ilink::client::{IlinkClient, IlinkError};
use crate::ilink::types::{message_state, Message};
use crate::redact::redact_text;

/// 错误码 -14：会话暂停。协议建议 1 小时，但实测多为分钟级限流，
/// 静默太久会丢失中间推送——缩到 5 分钟，到期探活失败再自动加长。
const SILENCE_MS: u64 = 5 * 60 * 1000;
/// 静默期间的醒来看间隔。
const SILENCE_CHECK_SECS: u64 = 30;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 当前本地时间的"一天中的分钟数"（免打扰时段判断用）。
fn minute_of_day() -> u32 {
    let t = chrono::Local::now().time();
    t.hour() * 60 + t.minute()
}

// ---------------------------------------------------------------------------
// serve：双循环
// ---------------------------------------------------------------------------

/// 前台运行守护进程（`wxherd serve`）。单实例锁 + 收消息循环 + 通知扫描循环。
pub async fn run_serve(paths: StatePaths) -> Result<()> {
    paths.ensure_dirs().context("创建状态目录失败")?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.daemon_lock_file())
        .context("打开 daemon.lock 失败")?;
    lock.try_lock_exclusive()
        .context("已有守护进程在运行（daemon.lock 被占用）")?;

    let credentials = Credentials::load(&paths)
        .context("读取凭据失败")?
        .context("微信未绑定，请先运行 wxherd login")?;
    let cfg = Config::load(&paths)
        .unwrap_or_else(|_| Config::defaults())
        .expanded();
    let client = IlinkClient::new(credentials, &cfg.wechat.bot_agent)?;

    let pid = std::process::id();
    std::fs::write(paths.daemon_pid_file(), pid.to_string()).context("写入 daemon.pid 失败")?;
    let mut runtime = RuntimeState::load(&paths);
    runtime.pid = Some(pid);
    runtime.fail_count = 0;
    runtime.save(&paths).ok();
    tracing::info!(
        "守护进程启动（pid {pid}，状态目录 {}）",
        paths.root.display()
    );

    let bridge = bridge::shared_bridge(paths.clone(), cfg.clone(), client);
    // 上线通知后端（失败不阻断）
    bridge.client.notify_start().await.ok();

    let recv = tokio::spawn(receive_loop(bridge.clone()));
    let scan = tokio::spawn(notify_loop(bridge.clone()));

    let result = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("收到 Ctrl+C，守护进程退出");
            Ok(())
        }
        r = recv => r.context("收消息循环异常退出")?,
        r = scan => r.context("状态扫描循环异常退出")?,
    };

    bridge.client.notify_stop().await.ok();
    let _ = std::fs::remove_file(paths.daemon_pid_file());
    result
}

/// 收消息循环：getUpdates 长轮询 → bridge 处理 → 回复。游标持久化，失败退避，-14 短静默+探活重试。
async fn receive_loop(bridge: Arc<bridge::Bridge>) -> Result<()> {
    let paths = &bridge.paths;
    let cfg = &bridge.cfg;
    let mut sync = SyncState::load(paths);
    let mut backoff_secs: u64 = 1;
    let mut silence_until: Option<u64> = None;

    loop {
        let now = now_ms();
        if let Some(until) = silence_until {
            if now < until {
                tracing::debug!("-14 静默中，{}s 后复查", SILENCE_CHECK_SECS);
                tokio::time::sleep(Duration::from_secs(
                    SILENCE_CHECK_SECS.min((until - now) / 1000 + 1),
                ))
                .await;
                continue;
            }
            // 静默到期：先 getConfig 探活再恢复轮询
            match bridge
                .client
                .get_config(&bridge.client.credentials.owner_user_id, None)
                .await
            {
                Ok(_) => {
                    tracing::info!("静默结束，会话恢复");
                    silence_until = None;
                }
                Err(IlinkError::SessionPaused) => silence_until = Some(now_ms() + SILENCE_MS),
                Err(e) => {
                    tracing::warn!("静默到期探活失败，稍后重试：{e}");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
            continue;
        }

        match bridge
            .client
            .get_updates(&sync.get_updates_buf, cfg.wechat.long_poll_seconds)
            .await
        {
            Ok(resp) => {
                backoff_secs = 1;
                let mut runtime = RuntimeState::load(paths);
                runtime.last_poll_ms = Some(now_ms());
                runtime.save(paths).ok();
                if let Some(buf) = resp.get_updates_buf.filter(|b| !b.is_empty()) {
                    sync.get_updates_buf = buf;
                }
                let msgs = resp.msgs.unwrap_or_default();
                if !msgs.is_empty() {
                    tracing::info!("getUpdates 收到 {} 条消息", msgs.len());
                }
                for msg in msgs {
                    // 只处理用户新消息（流式更新 message_state 1/2 属 v0.2 范围）
                    if msg.message_type != Some(crate::ilink::types::message_type::USER) {
                        tracing::info!(
                            "跳过消息：type={:?} state={:?}（非 USER）",
                            msg.message_type,
                            msg.message_state
                        );
                        continue;
                    }
                    if msg.message_state == Some(message_state::GENERATING) {
                        tracing::info!("跳过消息：state=GENERATING（流式中间态）");
                        continue;
                    }
                    tracing::info!(
                        "处理用户消息：text={:?}",
                        msg.text_content().unwrap_or_default()
                    );
                    // 入站 token → 滚动缓存（官方 Octop 策略）：主动推送据此保持通道常新
                    if let Some(tok) = msg
                        .context_token
                        .as_deref()
                        .filter(|t| !t.trim().is_empty())
                    {
                        bridge.client.remember_context_token(tok).await;
                    }
                    // 单条消息处理整体 60s 超时：任何下游挂起都不许拖死收消息循环
                    // （曾发生：某次处理后循环静默 37 分钟，微信完全无响应）
                    let handled = tokio::time::timeout(
                        Duration::from_secs(60),
                        handle_with_typing(&bridge, &msg),
                    )
                    .await;
                    match handled {
                        Ok(Some(reply)) => {
                            let to = bridge.client.credentials.owner_user_id.clone();
                            let sent = tokio::time::timeout(
                                Duration::from_secs(30),
                                send_chunks(
                                    &bridge,
                                    &to,
                                    &reply,
                                    msg.context_token.as_deref(),
                                    msg.run_id.as_deref(),
                                ),
                            )
                            .await;
                            if sent.is_err() {
                                tracing::warn!("回复发送超时（30s），放弃本条");
                            }
                        }
                        Ok(None) => {}
                        Err(_) => tracing::warn!("单条消息处理超时（60s），跳过"),
                    }
                }
                sync.save(paths).ok();
            }
            Err(IlinkError::SessionPaused) => {
                tracing::warn!("收到 -14（会话暂停），静默 {}s", SILENCE_MS / 1000);
                silence_until = Some(now_ms() + SILENCE_MS);
                let mut runtime = RuntimeState::load(paths);
                runtime.fail_count += 1;
                runtime.save(paths).ok();
            }
            Err(e) => {
                tracing::warn!("getUpdates 失败（{backoff_secs}s 后重试）：{e}");
                let mut runtime = RuntimeState::load(paths);
                runtime.fail_count += 1;
                runtime.save(paths).ok();
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(60);
            }
        }
    }
}

/// 回复/推送统一走安全分片（M5：3500 字节 UTF-8 边界），单片失败即停止后续片。
async fn send_chunks(
    bridge: &bridge::Bridge,
    to: &str,
    text: &str,
    context_token: Option<&str>,
    run_id: Option<&str>,
) {
    for chunk in split_markdown_chunks(text, bridge::MAX_MARKDOWN_BYTES) {
        if let Err(e) = bridge
            .client
            .send_text(to, &chunk, context_token, run_id)
            .await
        {
            tracing::warn!("回复发送失败：{}", redact_text(&format!("{e}")));
            break;
        }
    }
}

/// 处理一条消息，期间发「正在输入…」指示（typing.enabled=false 或取票失败则跳过）。
async fn handle_with_typing(bridge: &bridge::Bridge, msg: &Message) -> Option<String> {
    let to = bridge.client.credentials.owner_user_id.clone();
    let ticket = if bridge.cfg.typing.enabled {
        match bridge
            .client
            .get_config(&to, msg.context_token.as_deref())
            .await
        {
            Ok(cfg) => cfg.typing_ticket,
            Err(e) => {
                tracing::debug!("getConfig 取 typing_ticket 失败：{e}");
                None
            }
        }
    } else {
        None
    };
    if let Some(t) = ticket.as_deref() {
        bridge.client.send_typing(&to, t, 1).await.ok();
    }
    let reply = bridge.handle_message(msg).await;
    if let Some(t) = ticket.as_deref() {
        bridge.client.send_typing(&to, t, 2).await.ok();
    }
    reply
}

/// 通知扫描循环：每 poll_interval_seconds 扫 herdr agent 状态 → diff → 推送。
/// 去重状态与 event 子命令共享（claim_notification 文件锁原子 claim，双路径无重复推送）。
/// herdr 连续不可达超过 herdr_gone_exit_minutes（0=不退出）→ 守护进程自动退出，
/// 由下次 herdr 启动的 startup hook 补拉。
async fn notify_loop(bridge: Arc<bridge::Bridge>) -> Result<()> {
    let interval = Duration::from_secs(bridge.cfg.poll_interval_seconds.max(1));
    // herdr 不可达计时起点：任何一次成功扫描即复位
    let mut herdr_gone_since_ms: Option<u64> = None;
    // 发送节流（微信服务端对 sendMessage 突发/受罚期的保护）：
    // ret=-2 后完全停发，静默期每次尝试都会重置/升级服务端惩罚窗口。
    let mut send_blocked_until_ms: u64 = 0;
    const SEND_THROTTLE_MS: u64 = 10 * 60 * 1000; // -2 后停发 10 分钟
    const SEND_MIN_INTERVAL_MS: u64 = 4 * 1000; // 正常推送最小间隔，杜绝亚秒级突发
    let mut last_send_ms: u64 = 0;

    loop {
        tokio::time::sleep(interval).await;
        // 微信侧设置即时生效：通知/自动退出配置每轮重读，静音每轮检查
        let cfg_now = Config::load(&bridge.paths)
            .unwrap_or_else(|_| bridge.cfg.clone())
            .expanded();
        let notify_cfg = cfg_now.notify;
        let runtime = RuntimeState::load(&bridge.paths);
        let muted = runtime.mute_until_ms.is_some_and(|until| until > now_ms());
        let panes = match herdr::pane_list(&bridge.bin).await {
            Ok(p) => {
                herdr_gone_since_ms = None;
                p
            }
            Err(e) => {
                let since = herdr_gone_since_ms.get_or_insert(now_ms());
                if herdr_gone_exit_due(*since, now_ms(), cfg_now.herdr_gone_exit_minutes) {
                    tracing::info!(
                        "herdr 已持续不可达 {} 分钟，守护进程自动退出（下次 herdr 启动自动补拉）",
                        cfg_now.herdr_gone_exit_minutes
                    );
                    return Ok(());
                }
                tracing::debug!("状态扫描失败（herdr 未启动或暂不可达）：{e}");
                continue;
            }
        };
        let menu = finalize_menu(collect_agents(&panes));
        bridge.set_menu(menu).await;

        let now = now_ms();
        let minute = minute_of_day();
        for pane in &panes {
            let Some(_agent) = pane.agent.as_ref() else {
                continue;
            };
            let status = pane
                .agent_status
                .clone()
                .unwrap_or_else(|| "unknown".into());
            let Some(kind) = claim_notification(
                &bridge.paths,
                &pane.pane_id,
                &status,
                pane.focused,
                now,
                minute,
                &notify_cfg,
            ) else {
                continue;
            };
            if muted {
                tracing::debug!("静音中，跳过推送（pane {}，状态 {}）", pane.pane_id, status);
                continue;
            }

            let Some(entry) = bridge
                .menu()
                .await
                .iter()
                .find(|e| e.pane_id == pane.pane_id)
                .cloned()
            else {
                continue;
            };
            let text = match kind {
                NotifyKind::Blocked => {
                    // 结构化问卷：会话文件里有未回答的 ask_user_question → 全部题目一次列出
                    let questionnaire = entry
                        .session_file
                        .as_deref()
                        .and_then(herdr::pending_questionnaire);
                    let recent = read_recent_clean(&bridge.bin, &pane.pane_id, 40, 14).await;
                    let (text, questions) =
                        fmt_blocked_push(&entry, &recent, Some(entry.index), questionnaire);
                    // 记住问卷/选项：用户回数字（单题）或「题号.选项号」（多题）时翻译
                    bridge.set_pending_choices(&pane.pane_id, questions).await;
                    text
                }
                NotifyKind::Done { duration_ms } => {
                    // 末次输出：优先读 pi 会话文件（assistant 原文，零 TUI 装饰，与模型/插件无关）；
                    // 读不到（非 pi / 文件缺失）时回退 TUI 画面清洗 + 状态栏过滤
                    let last_line = match &entry.session_file {
                        Some(path) => herdr::last_assistant_text(path, 160),
                        None => None,
                    };
                    let last_line = match last_line {
                        Some(t) => t,
                        None => {
                            let recent = read_recent_clean(&bridge.bin, &pane.pane_id, 30, 8).await;
                            recent
                                .lines()
                                .rev()
                                .find(|l| !l.trim().is_empty() && !looks_like_status_frame(l))
                                .unwrap_or_default()
                                .to_string()
                        }
                    };
                    fmt_done_push(&entry, duration_ms, &last_line)
                }
            };
            let to = bridge.client.credentials.owner_user_id.clone();
            let now = now_ms();
            if now < send_blocked_until_ms {
                tracing::debug!(
                    "发送节流中（还剩 {}s），跳过 {} 通知（pane {}）",
                    (send_blocked_until_ms - now) / 1000,
                    status,
                    pane.pane_id
                );
                continue;
            }
            if now - last_send_ms < SEND_MIN_INTERVAL_MS {
                continue; // 距上次推送太近，下轮再发
            }
            match bridge.client.send_text(&to, &text, None, None).await {
                Ok(_) => {
                    tracing::info!("已推送 {} 通知（pane {}）", status, pane.pane_id);
                    last_send_ms = now;
                }
                Err(crate::ilink::client::IlinkError::Api { code: -2, .. }) => {
                    // 服务端节流/会话保护：立即停发一段时间，任何重试都会延长惩罚
                    send_blocked_until_ms = now + SEND_THROTTLE_MS;
                    tracing::warn!(
                        "推送被拒（ret=-2 节流/会话保护），停发 {} 分钟；期间通知进入待发队列，到期自动恢复",
                        SEND_THROTTLE_MS / 60000
                    );
                }
                Err(e) => tracing::warn!("通知推送失败：{}", redact_text(&format!("{e}"))),
            }
        }
    }
}

/// herdr 不可达自动退出判定（纯函数）：minutes=0 永不退出；时钟回拨不误退。
fn herdr_gone_exit_due(gone_since_ms: u64, now_ms: u64, minutes: u64) -> bool {
    minutes > 0 && now_ms.saturating_sub(gone_since_ms) >= minutes.saturating_mul(60_000)
}

/// 工作指示框标题行残留（清洗剥边后只剩 "⠹ Working" / "Working" 等），非真实输出。
fn looks_like_status_frame(line: &str) -> bool {
    let t = line.trim();
    t == "Working"
        || t == "Waiting"
        || t == "Blocked"
        || t.ends_with("Working") && t.chars().count() <= 12
        || t.ends_with("Waiting") && t.chars().count() <= 12
        // 编号选项行（1. xx / 2、xx…）：末次输出取它没有上下文，视为噪声。
        // 完整选项列表已由 extract_numbered_choices 在 blocked 推送里展示。
        || crate::bridge::is_numbered_choice_line(t).is_some()
}

/// 通知去重共享状态（M4）：文件锁保护的原子 claim，event 子命令与轮询循环共用。
///
/// 同一 pane 的状态评估与落盘在锁内完成；锁被占用说明另一路径正在处理同一转变，
/// 本次直接放弃（返回 None），保证双路径无重复推送。
/// 免打扰时段内照常记录推送时间（静默但不补推），与轮询原行为一致。
pub fn claim_notification(
    paths: &StatePaths,
    pane_id: &str,
    new_status: &str,
    focused: bool,
    now_ms: u64,
    minute_of_day: u32,
    cfg: &crate::config::NotifyConfig,
) -> Option<NotifyKind> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.notify_lock_file())
        .ok()?;
    // 非阻塞独占锁：占用中 = 另一路径正在处理，放弃本次
    lock.try_lock_exclusive().ok()?;
    let mut state = NotifyState::load(paths);
    let prev = state.panes.get(pane_id).cloned();
    let (new_st, kind) = evaluate_notification(
        prev.as_ref(),
        new_status,
        focused,
        now_ms,
        minute_of_day,
        cfg,
    );
    state.panes.insert(pane_id.to_string(), new_st);
    state.save(paths).ok();
    kind // 锁随 lock 句柄离开作用域自动释放
}

async fn read_recent_clean(bin: &str, pane_id: &str, lines: u32, keep: usize) -> String {
    match herdr::pane_read(bin, pane_id, lines).await {
        Ok(out) => {
            let cleaned = clean_terminal_output(&out, keep);
            if cleaned.is_empty() {
                "（暂无输出）".to_string()
            } else {
                cleaned
            }
        }
        Err(_) => "（暂无输出）".to_string(),
    }
}

// ---------------------------------------------------------------------------
// event / startup-hook（M4：herdr 插件集成）
// ---------------------------------------------------------------------------

/// herdr 事件载荷中提取 (pane_id, 新 agent_status)。
/// 载荷形状按防御式解析：顶层或 pane/agent 对象内的
/// pane_id/paneId/pane 与 agent_status/agentStatus/new_status/status 字段。
pub fn parse_agent_status_event(json: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let find_str = |v: &serde_json::Value, keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        None
    };
    let pane_id = find_str(&value, &["pane_id", "paneId"])
        .or_else(|| {
            value
                .get("pane")
                .and_then(|p| find_str(p, &["pane_id", "paneId"]))
        })
        // pane 字段直接是字符串 ID 的形状
        .or_else(|| {
            value
                .get("pane")
                .and_then(|p| p.as_str())
                .map(str::to_string)
        })?;
    let status = find_str(
        &value,
        &["agent_status", "agentStatus", "new_status", "status"],
    )
    .or_else(|| {
        value
            .get("agent")
            .and_then(|a| find_str(a, &["status", "agent_status"]))
    })?;
    Some((pane_id, status))
}

/// herdr 事件入口（M4）：pane.agent_status_changed → 与轮询共享去重状态后推送。
/// 独立进程运行（herdr 注入 HERDR_PLUGIN_EVENT_JSON / HERDR_PLUGIN_CONFIG_DIR）。
pub async fn run_event(paths: &StatePaths) -> i32 {
    let payload = std::env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default();
    if payload.trim().is_empty() {
        println!("未收到事件载荷（HERDR_PLUGIN_EVENT_JSON 为空）");
        return 0;
    }
    let Some((pane_id, status)) = parse_agent_status_event(&payload) else {
        tracing::debug!("忽略无法解析的事件：{}", redact_text(&payload));
        return 0;
    };

    let cfg = Config::load(paths)
        .unwrap_or_else(|_| Config::defaults())
        .expanded();
    let credentials = match Credentials::load(paths) {
        Ok(Some(c)) => c,
        Ok(None) => {
            tracing::debug!("微信未绑定，事件静默退出");
            return 0;
        }
        Err(e) => {
            tracing::warn!("凭据损坏：{e}");
            return 1;
        }
    };
    let bin = cfg.herdr_binary();
    let panes = match herdr::pane_list(&bin).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("事件处理时 herdr 不可达：{e}");
            return 0;
        }
    };
    let Some(pane) = panes.iter().find(|p| p.pane_id == pane_id).cloned() else {
        tracing::debug!("事件指向的 pane 已不存在：{pane_id}");
        return 0;
    };

    // 与轮询路径共享去重（文件锁原子 claim）；推不推由共享状态决定
    let kind = claim_notification(
        paths,
        &pane_id,
        &status,
        pane.focused,
        now_ms(),
        minute_of_day(),
        &cfg.notify,
    );
    let Some(kind) = kind else {
        tracing::debug!("事件与轮询路径去重命中或无需推送（pane {pane_id}，状态 {status}）");
        return 0;
    };
    let runtime = RuntimeState::load(paths);
    if runtime.mute_until_ms.is_some_and(|until| until > now_ms()) {
        tracing::debug!("静音中，事件推送跳过");
        return 0;
    }

    let menu = finalize_menu(collect_agents(&panes));
    let Some(entry) = menu.iter().find(|e| e.pane_id == pane_id).cloned() else {
        return 0;
    };
    let text = match kind {
        NotifyKind::Blocked => {
            let recent = read_recent_clean(&bin, &pane_id, 40, 14).await;
            fmt_blocked_push(&entry, &recent, Some(entry.index), None).0
        }
        NotifyKind::Done { duration_ms } => {
            let recent = read_recent_clean(&bin, &pane_id, 30, 8).await;
            let last_line = recent
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty() && !looks_like_status_frame(l))
                .unwrap_or_default()
                .to_string();
            fmt_done_push(&entry, duration_ms, &last_line)
        }
    };
    let client = match IlinkClient::new(credentials, &cfg.wechat.bot_agent) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("事件推送初始化客户端失败：{e}");
            return 1;
        }
    };
    let to = client.credentials.owner_user_id.clone();
    match client.send_text(&to, &text, None, None).await {
        Ok(_) => {
            tracing::info!("事件路径已推送 {status} 通知（pane {pane_id}）");
            0
        }
        Err(e) => {
            tracing::warn!("事件推送失败：{}", redact_text(&format!("{e}")));
            1
        }
    }
}

/// 跟随 herdr 启动的拉起钩子（M4）：auto_start 且未运行且已绑定 →
/// 分离拉起 serve 后立即退出（startup hook 是一次性初始化命令，不能驻留）。
pub fn run_startup_hook(paths: &StatePaths) -> i32 {
    let cfg = Config::load(paths).unwrap_or_else(|_| Config::defaults());
    if !cfg.auto_start {
        println!("auto_start=false，跳过自动拉起");
        return 0;
    }
    if let Some(pid) = read_pid(paths) {
        if herdr::pid_alive(pid) {
            println!("✓ 守护进程已在运行（pid {pid}），无需拉起");
            return 0;
        }
    }
    match Credentials::load(paths) {
        Ok(Some(_)) => {}
        Ok(None) => {
            println!("微信未绑定（先运行 wxherd login），跳过自动拉起");
            return 0;
        }
        Err(e) => {
            println!("凭据损坏（{e}），跳过自动拉起");
            return 0;
        }
    }
    match start_detached(paths) {
        Ok(pid) => {
            println!("✓ 已跟随 herdr 启动守护进程（pid {pid}）");
            0
        }
        Err(e) => {
            eprintln!("✗ 自动拉起失败：{e:#}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// start / stop（后台进程管理）
// ---------------------------------------------------------------------------

fn read_pid(paths: &StatePaths) -> Option<u32> {
    std::fs::read_to_string(paths.daemon_pid_file())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// 分离进程拉起 `wxherd serve`（日志重定向到 daemon.log），返回 pid。
#[cfg(windows)]
pub fn start_detached(paths: &StatePaths) -> Result<u32> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let exe = std::env::current_exe().context("无法定位 wxherd 可执行文件")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.daemon_log_file())
        .context("打开 daemon.log 失败")?;
    let log_err = log.try_clone().context("复制日志句柄失败")?;

    let child = std::process::Command::new(&exe)
        .arg("--state-dir")
        .arg(&paths.root)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .context("拉起守护进程失败")?;
    let pid = child.id();
    std::fs::write(paths.daemon_pid_file(), pid.to_string()).ok();
    Ok(pid)
}

/// 分离进程拉起 `wxherd serve`（日志重定向到 daemon.log），返回 pid。
///
/// Unix：`process_group(0)` 让子进程脱离父进程组，父进程退出后继续运行；
/// 标准输入置空、stdout/stderr 都指向日志文件。
#[cfg(unix)]
pub fn start_detached(paths: &StatePaths) -> Result<u32> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().context("无法定位 wxherd 可执行文件")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.daemon_log_file())
        .context("打开 daemon.log 失败")?;
    let log_err = log.try_clone().context("复制日志句柄失败")?;

    let child = std::process::Command::new(&exe)
        .arg("--state-dir")
        .arg(&paths.root)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .process_group(0)
        .spawn()
        .context("拉起守护进程失败")?;
    let pid = child.id();
    std::fs::write(paths.daemon_pid_file(), pid.to_string()).ok();
    Ok(pid)
}

/// start：已运行则幂等返回；未绑定凭据直接报错。
pub fn cmd_start(paths: &StatePaths) -> i32 {
    if let Some(pid) = read_pid(paths) {
        if herdr::pid_alive(pid) {
            println!("✓ 守护进程已在运行（pid {pid}）");
            return 0;
        }
        println!("pid {pid} 已退出，重新拉起…");
    }
    match Credentials::load(paths) {
        Ok(Some(_)) => {}
        Ok(None) => {
            eprintln!("✗ 微信未绑定，请先运行 `wxherd login`");
            return 1;
        }
        Err(e) => {
            eprintln!("✗ 凭据损坏：{e}");
            return 1;
        }
    }
    match start_detached(paths) {
        Ok(pid) => {
            println!("✓ 守护进程已启动（pid {pid}）");
            println!("  日志：{}", paths.daemon_log_file().display());
            0
        }
        Err(e) => {
            eprintln!("✗ 启动失败：{e:#}");
            1
        }
    }
}

/// stop：终止守护进程（Windows taskkill /T /F；Unix SIGTERM→SIGKILL）并清理 pid 文件。
pub fn cmd_stop(paths: &StatePaths) -> i32 {
    let Some(pid) = read_pid(paths) else {
        println!("守护进程未在运行（无 pid 记录）");
        return 0;
    };
    if !herdr::pid_alive(pid) {
        let _ = std::fs::remove_file(paths.daemon_pid_file());
        println!("守护进程未在运行（pid {pid} 已退出）");
        return 0;
    }
    match herdr::pid_kill(pid) {
        Ok(()) => {
            // 终止信号返回后进程可能仍在收尾，轮询确认退出
            for _ in 0..10 {
                if !herdr::pid_alive(pid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = std::fs::remove_file(paths.daemon_pid_file());
            println!("✓ 守护进程已停止（pid {pid}）");
            0
        }
        Err(e) => {
            eprintln!("✗ 停止失败：{e:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NotifyConfig;

    #[test]
    fn test_herdr_gone_exit_due() {
        assert!(!herdr_gone_exit_due(0, 299_999, 5), "未满 5 分钟不退出");
        assert!(herdr_gone_exit_due(0, 300_000, 5), "满 5 分钟退出");
        assert!(!herdr_gone_exit_due(0, u64::MAX / 2, 0), "0 = 永不自动退出");
        assert!(!herdr_gone_exit_due(10_000, 5_000, 5), "时钟回拨不误退");
    }

    #[test]
    fn test_parse_agent_status_event() {
        // 顶层扁平形状
        let json =
            r#"{"event":"pane.agent_status_changed","pane_id":"w8:p3","agent_status":"blocked"}"#;
        assert_eq!(
            parse_agent_status_event(json),
            Some(("w8:p3".into(), "blocked".into()))
        );
        // 嵌套 pane 对象 + camelCase
        let json = r#"{"pane":{"paneId":"w8:p5"},"agentStatus":"idle"}"#;
        assert_eq!(
            parse_agent_status_event(json),
            Some(("w8:p5".into(), "idle".into()))
        );
        // pane 为字符串 ID + status 别名
        let json = r#"{"pane":"w8:p1","new_status":"working"}"#;
        assert_eq!(
            parse_agent_status_event(json),
            Some(("w8:p1".into(), "working".into()))
        );
        // agent 对象内的 status
        let json = r#"{"pane_id":"w8:p2","agent":{"status":"blocked"}}"#;
        assert_eq!(
            parse_agent_status_event(json),
            Some(("w8:p2".into(), "blocked".into()))
        );
        // 无法解析 → None
        assert_eq!(parse_agent_status_event("not json"), None);
        assert_eq!(
            parse_agent_status_event(r#"{"event":"worktree.created"}"#),
            None
        );
        assert_eq!(
            parse_agent_status_event(r#"{"pane_id":"w8:p3"}"#),
            None,
            "缺状态字段"
        );
    }

    #[test]
    fn test_claim_notification_shared_state_and_lock() {
        let dir = std::env::temp_dir().join(format!("wxherd-claim-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = StatePaths { root: dir.clone() };
        let cfg = NotifyConfig::default();

        // blocked 未到延迟 → 不推但状态已落盘
        let kind = claim_notification(&paths, "w8:p3", "blocked", false, 1_000, 600, &cfg);
        assert_eq!(kind, None);
        let state = NotifyState::load(&paths);
        assert_eq!(state.panes.get("w8:p3").unwrap().last_status, "blocked");

        // 延迟已过 → 推 Blocked（与轮询共享同一状态文件）
        let kind = claim_notification(&paths, "w8:p3", "blocked", false, 12_000, 600, &cfg);
        assert_eq!(kind, Some(NotifyKind::Blocked));

        // 同周期再 claim → 去重命中
        let kind = claim_notification(&paths, "w8:p3", "blocked", false, 20_000, 600, &cfg);
        assert_eq!(kind, None);

        // blocked → idle：持续 ≥ min_done_duration_seconds（默认 10s）才推完成
        let kind = claim_notification(&paths, "w8:p3", "idle", false, 30_000, 600, &cfg);
        assert!(
            matches!(kind, Some(NotifyKind::Done { .. })),
            "任务 29s ≥ 10s 阈值，应推送"
        );

        // 独占锁生效：手工占用锁文件后 claim 失败返回 None
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(paths.notify_lock_file())
            .unwrap();
        lock.try_lock_exclusive().unwrap();
        let kind = claim_notification(&paths, "w8:p9", "blocked", false, 40_000, 600, &cfg);
        assert_eq!(kind, None, "锁被占用时必须放弃而不是阻塞");
        drop(lock);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
