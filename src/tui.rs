//! 设置弹框 TUI（M4）：ratatui + crossterm，herdr `prefix+u` 弹出，仅弹出时存在。
//!
//! 交互：↑↓ 选择、空格切换开关、t 发测试消息、l 查看日志（已脱敏）、
//! s 保存并应用、Esc 关闭。文案直接中文硬编码（无 i18n）。

use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::config::{Config, Credentials, RuntimeState, StatePaths};
use crate::herdr;
use crate::ilink::client::IlinkClient;
use crate::intent::learn::LearnBook;
use crate::redact::redact_text;

/// 可布尔切换的开关数量（数值行不计入）。
const TOGGLE_COUNT: usize = 5;
/// 弹框内可选中的行数（5 个开关 + 1 个数值行 + 1 个 AI 服务商行）。
const ROW_COUNT: usize = 7;
/// AI 服务商行在列表中的下标（非开关，Enter/`a` 打开编辑视图）。
const AI_PROVIDER_ROW: usize = 6;
/// 自动退出分钟数的可调上限（12 小时）。
const GONE_EXIT_MINUTES_MAX: u64 = 720;
/// 日志视图展示的末尾行数。
const LOG_TAIL_LINES: usize = 40;

// ---------------------------------------------------------------------------
// 纯逻辑模型（可单测）
// ---------------------------------------------------------------------------

/// AI 服务商编辑状态（纯数据，可单测）。字段序：0=base_url 1=model 2=api_key。
pub struct AiEdit {
    pub field: usize,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl AiEdit {
    pub fn new(base_url: &str, model: &str, api_key: &str) -> Self {
        Self {
            field: 0,
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
        }
    }

    /// 当前编辑中的字段（可变）。
    pub fn current_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.base_url,
            1 => &mut self.model,
            _ => &mut self.api_key,
        }
    }

    #[allow(dead_code)]
    pub fn current(&self) -> &str {
        match self.field {
            0 => &self.base_url,
            1 => &self.model,
            _ => &self.api_key,
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.current_mut().push(c);
    }

    pub fn backspace(&mut self) {
        self.current_mut().pop();
    }

    pub fn next_field(&mut self) {
        self.field = (self.field + 1) % 3;
    }

    pub fn prev_field(&mut self) {
        self.field = (self.field + 2) % 3;
    }

    /// 字段标签（中文）。
    pub fn label(&self, i: usize) -> &'static str {
        match i {
            0 => "接口地址 base_url",
            1 => "模型 model",
            _ => "密钥 api_key",
        }
    }

    /// 值行（空值显示占位提示）。
    pub fn value_of(&self, i: usize) -> &str {
        match i {
            0 => &self.base_url,
            1 => &self.model,
            _ => &self.api_key,
        }
    }
}

/// 设置弹框状态模型：加载/切换/应用均为纯文件操作，不依赖终端。
pub struct SettingsModel {
    pub daemon_running: bool,
    pub daemon_pid: Option<u32>,
    pub bound: bool,
    pub token_suffix: String,
    pub learned_rules: usize,
    pub auto_start: bool,
    pub auto_restart: bool,
    pub herdr_gone_exit_minutes: u64,
    pub notify_blocked: bool,
    pub notify_done: bool,
    pub ai_enabled: bool,
    pub ai_base_url: String,
    pub ai_model: String,
    pub ai_api_key: String,
    pub editing: Option<AiEdit>,
    pub selected: usize,
    pub status: String,
    pub show_log: bool,
    pub log_text: String,
}

impl SettingsModel {
    /// 从状态目录加载当前设置快照。
    pub fn load(paths: &StatePaths) -> Self {
        let cfg = Config::load(paths).unwrap_or_else(|_| Config::defaults());
        let runtime = RuntimeState::load(paths);
        let pid_file = std::fs::read_to_string(paths.daemon_pid_file())
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        let pid = pid_file.or(runtime.pid);
        let bound = Credentials::load(paths).ok().flatten();
        let learn = LearnBook::open(paths.learned_rules_file(), cfg.ai.max_learned_rules);
        Self {
            daemon_running: pid.is_some_and(herdr::pid_alive),
            daemon_pid: pid.filter(|p| herdr::pid_alive(*p)),
            bound: bound.is_some(),
            token_suffix: bound.map(|c| c.token_suffix()).unwrap_or_default(),
            learned_rules: learn.len(),
            auto_start: cfg.auto_start,
            auto_restart: cfg.auto_restart,
            herdr_gone_exit_minutes: cfg.herdr_gone_exit_minutes,
            notify_blocked: cfg.notify.blocked,
            notify_done: cfg.notify.done,
            ai_enabled: cfg.ai.enabled,
            ai_base_url: cfg.ai.base_url.clone(),
            ai_model: cfg.ai.model.clone(),
            ai_api_key: cfg.ai.api_key.clone(),
            editing: None,
            selected: 0,
            status: String::new(),
            show_log: false,
            log_text: String::new(),
        }
    }

    fn toggles(&self) -> [&bool; TOGGLE_COUNT] {
        [
            &self.auto_start,
            &self.auto_restart,
            &self.notify_blocked,
            &self.notify_done,
            &self.ai_enabled,
        ]
    }

    fn toggle_mut(&mut self) -> Option<&mut bool> {
        // 行序：0=auto_start 1=auto_restart 2=自动退出数值 3=blocked 4=done 5=ai
        match self.selected {
            0 => Some(&mut self.auto_start),
            1 => Some(&mut self.auto_restart),
            3 => Some(&mut self.notify_blocked),
            4 => Some(&mut self.notify_done),
            5 => Some(&mut self.ai_enabled),
            _ => None,
        }
    }

    pub fn move_selection(&mut self, delta: i32) {
        let n = ROW_COUNT as i32;
        self.selected = ((self.selected as i32 + delta).rem_euclid(n)) as usize;
    }

    pub fn toggle_selected(&mut self) {
        if self.selected == 2 {
            // 数值行：空格在「不自动退出(0)」与默认 5 分钟间切换
            self.herdr_gone_exit_minutes = if self.herdr_gone_exit_minutes == 0 {
                5
            } else {
                0
            };
            return;
        }
        if let Some(v) = self.toggle_mut() {
            *v = !*v;
        }
    }

    /// ←/→ 微调数值行（其余行无操作）；0..=GONE_EXIT_MINUTES_MAX。
    pub fn adjust_selected(&mut self, delta: i64) {
        if self.selected != 2 {
            return;
        }
        let next = self.herdr_gone_exit_minutes as i64 + delta;
        self.herdr_gone_exit_minutes = next.clamp(0, GONE_EXIT_MINUTES_MAX as i64) as u64;
    }

    /// 把设置写回 config.toml（先重读磁盘保留 api_key 等未展示字段，再保存）。
    pub fn apply(&self, paths: &StatePaths) -> Result<()> {
        let mut cfg = Config::load(paths).unwrap_or_else(|_| Config::defaults());
        cfg.auto_start = self.auto_start;
        cfg.auto_restart = self.auto_restart;
        cfg.herdr_gone_exit_minutes = self.herdr_gone_exit_minutes;
        cfg.notify.blocked = self.notify_blocked;
        cfg.notify.done = self.notify_done;
        cfg.ai.enabled = self.ai_enabled;
        cfg.ai.base_url = self.ai_base_url.trim().to_string();
        cfg.ai.model = self.ai_model.trim().to_string();
        cfg.ai.api_key = self.ai_api_key.trim().to_string();
        cfg.save(paths).context("写入 config.toml 失败")
    }

    /// 加载守护进程日志末尾（逐行脱敏：token/手机号打码）。
    pub fn load_log(&mut self, paths: &StatePaths) {
        let text = std::fs::read_to_string(paths.daemon_log_file()).unwrap_or_default();
        let tail: Vec<String> = text
            .lines()
            .rev()
            .take(LOG_TAIL_LINES)
            .map(redact_text)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        self.log_text = if tail.is_empty() {
            "（暂无日志）".to_string()
        } else {
            tail.join("\n")
        };
        self.show_log = true;
    }

    /// 弹框主体行（中文文案）。
    pub fn rows(&self) -> Vec<String> {
        let daemon = match (self.daemon_running, self.daemon_pid) {
            (true, Some(pid)) => format!("状态：● 运行中 (pid {pid})"),
            _ => "状态：✗ 未运行（可运行 wxherd start）".to_string(),
        };
        let bind = if self.bound {
            format!("微信绑定：已绑定 (尾号 {})", self.token_suffix)
        } else {
            "微信绑定：未绑定（请运行 wxherd login 扫码）".to_string()
        };
        let check = |on: bool| if on { "[✓]" } else { "[ ]" };
        let toggles = self.toggles();
        let arrow = |i: usize| if self.selected == i { "▸" } else { " " };
        let gone_desc = if self.herdr_gone_exit_minutes == 0 {
            "不自动退出".to_string()
        } else {
            format!("{} 分钟", self.herdr_gone_exit_minutes)
        };
        // AI 服务商摘要：地址/模型/密钥是否已配置（密钥只显示占位，不显明文）
        let key_state = if self.ai_api_key.trim().is_empty() {
            "密钥未配置"
        } else {
            "密钥已配置"
        };
        let provider = if self.ai_base_url.trim().is_empty() && self.ai_model.trim().is_empty() {
            format!(
                "{}AI 服务商：未配置（Enter 编辑，{key_state}）",
                arrow(AI_PROVIDER_ROW)
            )
        } else {
            let url = if self.ai_base_url.trim().is_empty() {
                "(未设地址)"
            } else {
                self.ai_base_url.trim()
            };
            let model = if self.ai_model.trim().is_empty() {
                "(未设模型)"
            } else {
                self.ai_model.trim()
            };
            format!(
                "{}AI 服务商：{model} @ {url}（{key_state}）",
                arrow(AI_PROVIDER_ROW)
            )
        };
        vec![
            "┌─ WxHerd 微信桥设置 ─────────────────────┐".to_string(),
            daemon,
            bind,
            format!("AI 意图层：已学 {} 条规则", self.learned_rules),
            String::new(),
            format!(
                "{}{} 跟随 herdr：auto_start",
                arrow(0),
                check(self.auto_start)
            ),
            format!("{}{} 崩溃自动重启", arrow(1), check(self.auto_restart)),
            format!(
                "{}herdr 关闭后自动退出：{}（←→ 调整，0=不退出）",
                arrow(2),
                gone_desc
            ),
            format!(
                "{}{} 通知类型：阻塞推送 blocked",
                arrow(3),
                check(*toggles[2])
            ),
            format!("{}{} 通知类型：完成推送 done", arrow(4), check(*toggles[3])),
            format!("{}{} AI 意图层启用", arrow(5), check(*toggles[4])),
            provider,
        ]
    }

    /// AI 编辑视图的行文本（▸ 标记当前编辑字段）。
    pub fn ai_edit_rows(&self) -> Vec<String> {
        let Some(edit) = &self.editing else {
            return Vec::new();
        };
        let mut out = vec![String::from("┌─ AI 意图层配置 ────────────────────┐")];
        for i in 0..3 {
            let mark = if edit.field == i { "▸" } else { " " };
            let val = edit.value_of(i);
            let shown = if val.is_empty() { "(空)" } else { val };
            out.push(format!("{mark} {}：{shown}", edit.label(i)));
        }
        out.push(String::new());
        out.push("提示：可填任意 OpenAI 兼容端点；".to_string());
        out.push("密钥建议写 ${WXHERD_AI_KEY} 引用环境变量。".to_string());
        out
    }

    pub fn begin_ai_edit(&mut self) {
        self.editing = Some(AiEdit::new(
            &self.ai_base_url,
            &self.ai_model,
            &self.ai_api_key,
        ));
        self.status.clear();
    }

    /// 提交编辑结果到模型并落盘；返回状态文案。
    pub fn commit_ai_edit(&mut self, paths: &StatePaths) -> String {
        if let Some(edit) = self.editing.take() {
            self.ai_base_url = edit.base_url.clone();
            self.ai_model = edit.model.clone();
            self.ai_api_key = edit.api_key.clone();
        }
        match self.apply(paths) {
            Ok(()) => {
                let ai_cfg = Config::load(paths).unwrap_or_else(|_| Config::defaults());
                if ai_cfg.ai.api_key.trim().is_empty() {
                    "✓ AI 配置已保存（未填密钥 → AI 层不启用，仅本地规则）".to_string()
                } else {
                    "✓ AI 配置已保存（守护进程下轮生效）".to_string()
                }
            }
            Err(e) => format!("✗ 保存失败：{e:#}"),
        }
    }

    pub const AI_FOOTER: &'static str = "[↑↓/Tab] 切换字段  [Enter] 保存  [Esc] 取消";

    pub const FOOTER: &'static str = "[↑↓] 选择  [空格] 切换  [←→] 调整  [a] AI 服务商  [t] 测试  [l] 日志  [s] 保存  [Esc] 关闭";
}

// ---------------------------------------------------------------------------
// 动作：发送测试消息（脱离 TUI 的独立 tokio 运行时）
// ---------------------------------------------------------------------------

fn send_test_message(paths: &StatePaths) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("创建运行时失败")?;
    rt.block_on(async {
        let credentials = Credentials::load(paths)?.with_context(|| {
            format!(
                "微信未绑定（{} 不存在），请先运行 wxherd login",
                paths.credentials_file().display()
            )
        })?;
        let cfg = Config::load(paths)
            .unwrap_or_else(|_| Config::defaults())
            .expanded();
        let client = IlinkClient::new(credentials, &cfg.wechat.bot_agent)?;
        let to = client.credentials.owner_user_id.clone();
        client
            .send_text(
                &to,
                "🤖 WxHerd 测试消息：来自设置弹框，通道正常。",
                None,
                None,
            )
            .await
            .map_err(anyhow::Error::from)?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// 渲染与事件循环
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, model: &SettingsModel) {
    let area = centered_rect(f.area(), 80, 24);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" WxHerd 微信桥设置 ")
        .border_style(Style::default().cyan());

    if model.show_log {
        let para = Paragraph::new(model.log_text.as_str())
            .block(block.title(" 守护进程日志（已脱敏，末 40 行） "));
        f.render_widget(para, area);
        return;
    }

    if model.editing.is_some() {
        let rows = model.ai_edit_rows();
        let mut lines: Vec<Line> = Vec::new();
        for r in rows.iter() {
            let highlight = r.starts_with('▸');
            lines.push(Line::from(r.clone()).style(if highlight {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            }));
        }
        f.render_widget(block.clone(), area);
        let inner = block.inner(area);
        let split = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).split(inner);
        f.render_widget(Paragraph::new(lines), split[0]);
        f.render_widget(
            Paragraph::new(Line::from(SettingsModel::AI_FOOTER).yellow()),
            split[1],
        );
        return;
    }

    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let body: Vec<Line> = model
        .rows()
        .iter()
        .map(|r| {
            let highlight = r.contains('▸');
            Line::from(r.clone()).style(if highlight {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            })
        })
        .collect();
    f.render_widget(Paragraph::new(body), rows[0]);
    if !model.status.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(model.status.as_str()).yellow()),
            rows[1],
        );
    }
    let footer = Line::from(SettingsModel::FOOTER);
    f.render_widget(Paragraph::new(footer), rows[2]);
}

fn centered_rect(area: ratatui::layout::Rect, pct_w: u16, height: u16) -> ratatui::layout::Rect {
    let w = area.width.saturating_mul(pct_w) / 100;
    let h = height.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    ratatui::layout::Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

/// 运行设置弹框（阻塞，直到 Esc 关闭）。返回进程退出码。
pub fn run_settings_tui(paths: StatePaths) -> i32 {
    if let Err(e) = std::io::stdout().execute(crossterm::terminal::EnterAlternateScreen) {
        eprintln!("✗ 进入终端界面失败：{e}");
        return 1;
    }
    if let Err(e) = crossterm::terminal::enable_raw_mode() {
        let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
        eprintln!("✗ 开启终端原始模式失败：{e}");
        return 1;
    }
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            restore_terminal();
            eprintln!("✗ 初始化界面失败：{e}");
            return 1;
        }
    };

    let mut model = SettingsModel::load(&paths);
    loop {
        if terminal.draw(|f| draw(f, &model)).is_err() {
            break;
        }
        let ev = if event::poll(Duration::from_millis(250)).unwrap_or(false) {
            event::read().ok()
        } else {
            None
        };
        let Some(Event::Key(key)) = ev else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // AI 编辑视图：捕获所有按键（文字输入），Esc 取消、Enter 保存
        if model.editing.is_some() {
            match key.code {
                KeyCode::Esc => {
                    model.editing = None;
                    model.status = "已取消 AI 配置修改".to_string();
                }
                KeyCode::Enter => {
                    model.status = model.commit_ai_edit(&paths);
                }
                KeyCode::Tab | KeyCode::Down => {
                    if let Some(e) = model.editing.as_mut() {
                        e.next_field();
                    }
                }
                KeyCode::Up | KeyCode::BackTab => {
                    if let Some(e) = model.editing.as_mut() {
                        e.prev_field();
                    }
                }
                KeyCode::Backspace => {
                    if let Some(e) = model.editing.as_mut() {
                        e.backspace();
                    }
                }
                KeyCode::Char(c) if !c.is_control() => {
                    if let Some(e) = model.editing.as_mut() {
                        e.push_char(c);
                    }
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Esc => {
                if model.show_log {
                    model.show_log = false;
                } else {
                    break;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => model.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => model.move_selection(1),
            KeyCode::Left | KeyCode::Char('-') => model.adjust_selected(-1),
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => model.adjust_selected(1),
            KeyCode::Char(' ') | KeyCode::Enter if !model.show_log => {
                if model.selected == AI_PROVIDER_ROW {
                    model.begin_ai_edit();
                } else {
                    model.toggle_selected();
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => model.begin_ai_edit(),
            KeyCode::Char('t') | KeyCode::Char('T') => {
                model.status = match send_test_message(&paths) {
                    Ok(()) => "✓ 测试消息已发送，请在微信查看".to_string(),
                    Err(e) => format!("✗ {e:#}"),
                };
            }
            KeyCode::Char('l') | KeyCode::Char('L') => {
                if model.show_log {
                    model.show_log = false;
                } else {
                    model.load_log(&paths);
                }
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                model.status = match model.apply(&paths) {
                    Ok(()) => "✓ 已保存并应用（守护进程下轮生效）".to_string(),
                    Err(e) => format!("✗ 保存失败：{e:#}"),
                };
            }
            _ => {}
        }
    }

    restore_terminal();
    0
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> StatePaths {
        let dir =
            std::env::temp_dir().join(format!("wxherd-tui-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        StatePaths { root: dir }
    }

    #[test]
    fn test_toggle_and_selection() {
        let paths = temp_paths("toggle");
        let mut model = SettingsModel::load(&paths);
        assert!(
            model.auto_start,
            "无配置文件时走 Config::defaults（auto_start=true）"
        );
        assert_eq!(model.selected, 0);
        model.move_selection(1);
        assert_eq!(model.selected, 1);
        model.move_selection(-2);
        assert_eq!(model.selected, ROW_COUNT - 1, "循环滚动");
        model.move_selection(1);
        assert_eq!(model.selected, 0);

        let before = model.auto_restart;
        model.selected = 1;
        model.toggle_selected();
        assert_ne!(model.auto_restart, before);
        model.toggle_selected();
        assert_eq!(model.auto_restart, before);
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn test_herdr_gone_exit_value_row() {
        let paths = temp_paths("gone-exit");
        let mut model = SettingsModel::load(&paths);
        assert_eq!(model.herdr_gone_exit_minutes, 5, "默认 5 分钟");

        // 选中数值行：空格在 0（不退出）与 5 分钟间切换
        model.selected = 2;
        model.toggle_selected();
        assert_eq!(model.herdr_gone_exit_minutes, 0, "空格切到不自动退出");
        model.toggle_selected();
        assert_eq!(model.herdr_gone_exit_minutes, 5, "空格切回 5 分钟");

        // ←→ 微调，边界 0 与 720
        model.adjust_selected(-1);
        assert_eq!(model.herdr_gone_exit_minutes, 4);
        for _ in 0..10 {
            model.adjust_selected(-1);
        }
        assert_eq!(model.herdr_gone_exit_minutes, 0, "下界 0");
        model.adjust_selected(-1);
        assert_eq!(model.herdr_gone_exit_minutes, 0, "不低于 0");
        model.adjust_selected(GONE_EXIT_MINUTES_MAX as i64 + 100);
        assert_eq!(
            model.herdr_gone_exit_minutes, GONE_EXIT_MINUTES_MAX,
            "上界 720"
        );

        // 数值行空格切换：非零 → 0 → 5
        model.toggle_selected();
        assert_eq!(model.herdr_gone_exit_minutes, 0, "非零值空格切到不自动退出");
        model.toggle_selected();
        assert_eq!(model.herdr_gone_exit_minutes, 5, "0 空格切回默认 5 分钟");

        // 开关行不受 ←→ 影响
        model.selected = 0;
        let auto_start_before = model.auto_start;
        model.adjust_selected(1);
        assert_eq!(model.auto_start, auto_start_before, "非数值行 ←→ 无操作");

        // apply 持久化
        model.selected = 2;
        model.adjust_selected(3); // 5 + 3 = 8
        model.apply(&paths).unwrap();
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.herdr_gone_exit_minutes, 8, "数值行修改写入 config.toml");

        // 显示文案
        model.herdr_gone_exit_minutes = 0;
        assert!(model.rows().join("\n").contains("不自动退出"));
        model.herdr_gone_exit_minutes = 8;
        assert!(model
            .rows()
            .join("\n")
            .contains("herdr 关闭后自动退出：8 分钟"));
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn test_rows_and_footer_chinese() {
        let paths = temp_paths("rows");
        let mut model = SettingsModel::load(&paths);
        model.bound = true;
        model.token_suffix = "4cd7".into();
        let rows = model.rows().join("\n");
        assert!(rows.contains("WxHerd 微信桥设置"));
        assert!(rows.contains("跟随 herdr"));
        assert!(rows.contains("[✓]"));
        assert!(rows.contains("微信绑定：已绑定 (尾号 4cd7)"));
        assert!(rows.contains("AI 意图层"));
        assert!(rows.contains("AI 服务商"), "弹框应有 AI 服务商配置行");
        assert!(SettingsModel::FOOTER.contains("保存"));
        assert!(SettingsModel::FOOTER.contains("测试"));
        assert!(
            SettingsModel::FOOTER.contains("AI 服务商"),
            "页脚应提示 AI 配置入口"
        );
        // 选中行有箭头指示
        assert!(model.rows().iter().any(|r| r.starts_with("▸")));
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn test_apply_persists_config() {
        let paths = temp_paths("apply");
        Config::defaults().save(&paths).unwrap();
        let mut model = SettingsModel::load(&paths);
        model.selected = 3;
        model.toggle_selected(); // notify_blocked on → off
        model.ai_enabled = false;
        model.apply(&paths).unwrap();
        let cfg = Config::load(&paths).unwrap();
        assert!(!cfg.notify.blocked);
        assert!(cfg.notify.done, "未改动的开关保持原值");
        assert!(!cfg.ai.enabled);
        assert!(cfg.auto_start);
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn test_apply_preserves_unshown_fields() {
        let paths = temp_paths("preserve");
        let mut cfg = Config::defaults();
        cfg.ai.api_key = "${WXHERD_AI_KEY}".into();
        cfg.ai.model = "glm-4-flash".into();
        cfg.save(&paths).unwrap();
        let model = SettingsModel::load(&paths);
        model.apply(&paths).unwrap();
        let cfg2 = Config::load(&paths).unwrap();
        assert_eq!(
            cfg2.ai.api_key, "${WXHERD_AI_KEY}",
            "apply 不得丢失未展示字段"
        );
        assert_eq!(cfg2.ai.model, "glm-4-flash");
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn test_ai_edit_field_switch_and_typing() {
        let mut edit = AiEdit::new("", "", "");
        for c in "https://api.test/v1".chars() {
            edit.push_char(c);
        }
        assert_eq!(edit.base_url, "https://api.test/v1");
        edit.next_field();
        for c in "glm-4-flash".chars() {
            edit.push_char(c);
        }
        assert_eq!(edit.model, "glm-4-flash");
        edit.next_field();
        assert_eq!(edit.field, 2);
        edit.push_char('x');
        edit.backspace();
        assert_eq!(edit.api_key, "", "Backspace 应删掉最后一个字符");
        edit.next_field();
        assert_eq!(edit.field, 0, "字段循环回到 base_url");
        edit.prev_field();
        assert_eq!(edit.field, 2, "后退环绕到 api_key");
        assert_eq!(edit.label(0), "接口地址 base_url");
        assert_eq!(edit.value_of(1), "glm-4-flash");
    }

    #[test]
    fn test_commit_ai_edit_persists_provider() {
        let paths = temp_paths("aiedit");
        Config::defaults().save(&paths).unwrap();
        let mut model = SettingsModel::load(&paths);
        model.begin_ai_edit();
        let edit = model.editing.as_mut().unwrap();
        for c in "https://open.bigmodel.cn/api/paas/v4".chars() {
            edit.push_char(c);
        }
        edit.next_field();
        for c in "glm-4-flash".chars() {
            edit.push_char(c);
        }
        edit.next_field();
        for c in "${WXHERD_AI_KEY}".chars() {
            edit.push_char(c);
        }
        let status = model.commit_ai_edit(&paths);
        assert!(status.starts_with('✓'), "应报告保存成功：{status}");
        assert!(model.editing.is_none(), "提交后退出编辑视图");
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.ai.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(cfg.ai.model, "glm-4-flash");
        assert_eq!(cfg.ai.api_key, "${WXHERD_AI_KEY}", "密钥原样写入，不展开");
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn test_ai_provider_row_shows_config_state() {
        let paths = temp_paths("airow");
        let mut cfg = Config::defaults();
        cfg.ai.base_url = "https://api.test/v1".into();
        cfg.ai.model = "m1".into();
        cfg.ai.api_key = "k".into();
        cfg.save(&paths).unwrap();
        let model = SettingsModel::load(&paths);
        let rows = model.rows();
        let provider_row = &rows[rows.len() - 1];
        assert!(
            provider_row.contains("AI 服务商"),
            "最后一行应为 AI 服务商：{provider_row}"
        );
        assert!(provider_row.contains("m1"), "应显示模型名");
        assert!(
            provider_row.contains("密钥已配置"),
            "应显示密钥状态；不得显明文"
        );
        assert!(!provider_row.contains("k @"), "不应泄露密钥内容");
        // AI 编辑视图行应包含三个字段标签
        let mut m2 = SettingsModel::load(&paths);
        m2.begin_ai_edit();
        let edit_rows = m2.ai_edit_rows();
        assert!(edit_rows.iter().any(|r| r.contains("接口地址")));
        assert!(edit_rows.iter().any(|r| r.contains("模型")));
        assert!(edit_rows.iter().any(|r| r.contains("密钥")));
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn test_load_log_redacted_and_tail() {
        let paths = temp_paths("log");
        let secret = format!("{}{}", "fdecba", "9876543210");
        let mut log = String::new();
        for i in 0..60 {
            log.push_str(&format!("line {i} token=Bearer {secret}\n"));
        }
        std::fs::write(paths.daemon_log_file(), log).unwrap();
        let mut model = SettingsModel::load(&paths);
        model.load_log(&paths);
        assert!(model.show_log);
        assert!(!model.log_text.contains(&secret), "日志必须脱敏");
        assert!(model.log_text.contains("****3210"));
        assert_eq!(
            model.log_text.lines().count(),
            LOG_TAIL_LINES,
            "只保留末 40 行"
        );
        assert!(model.log_text.contains("line 59"), "保留的是末尾行");
        let _ = std::fs::remove_dir_all(&paths.root);
    }
}
