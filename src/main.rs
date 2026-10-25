//! wxherd CLI 入口：参数解析、子命令分发、日志初始化。业务逻辑在各模块。

mod bridge;
mod config;
mod daemon;
mod herdr;
mod ilink;
mod intent;
mod redact;
mod tui;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::{Credentials, StatePaths};
use ilink::client::{IlinkClient, QR_API_BASE};
use ilink::types::QrState;

#[derive(Parser)]
#[command(
    name = "wxherd",
    version,
    about = "微信 ↔ Herdr 全功能桥接插件",
    override_usage = "wxherd [OPTIONS] <COMMAND>"
)]
struct Cli {
    /// 状态目录（默认顺序：本参数 → WXHERD_STATE_DIR → HERDR_STATE_DIR → ~/.wxherd）
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 生成默认 config.toml 与状态目录
    Init,
    /// 终端扫码绑定微信（含配对码 / 过期自动刷新 / 重定向提示）
    Login,
    /// 前台运行守护进程（日志输出到终端）
    Serve,
    /// 后台启动守护进程（分离进程 + 日志写状态目录）
    Start,
    /// 停止后台守护进程
    Stop,
    /// 查看桥接状态
    Status,
    /// 配置 / herdr 连通 / 微信绑定 诊断
    Doctor,
    /// 给绑定账号发测试文本
    SendTest,
    /// 上传并发送图片
    SendImage {
        /// 图片文件路径
        path: PathBuf,
        /// 可选说明文字
        caption: Option<String>,
    },
    /// 上传并发送文件
    SendFile {
        path: PathBuf,
        caption: Option<String>,
    },
    /// herdr 事件入口（由插件清单的 [[events]] 调用）
    Event,
    /// 跟随 herdr 启动的拉起钩子（由插件清单的 [[startup]] 调用）
    StartupHook,
    /// 设置弹框 TUI
    SettingsTui,
    /// 打开设置弹框 pane（快捷键入口：以真实终端 pane 运行 settings-tui）
    OpenSettings,
    /// 直接测试 AI 意图解析（排障用：打印 prompt、原始响应、解析结果）
    AiTest {
        /// 要解析的自然语言文本
        text: String,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let paths = StatePaths::resolve(cli.state_dir.as_deref());

    let code = match cli.command {
        Command::Init => cmd_init(&paths),
        Command::Login => cmd_login(&paths).await,
        Command::Status => cmd_status(&paths),
        Command::Doctor => cmd_doctor(&paths).await,
        Command::SendTest => cmd_send_test(&paths).await,
        Command::SendImage { path, caption } => {
            cmd_send_image(&paths, &path, caption.as_deref()).await
        }
        Command::SendFile { path, caption } => {
            cmd_send_file(&paths, &path, caption.as_deref()).await
        }
        Command::Serve => match daemon::run_serve(paths).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("✗ 守护进程异常退出：{e:#}");
                1
            }
        },
        Command::Start => daemon::cmd_start(&paths),
        Command::Stop => daemon::cmd_stop(&paths),
        Command::Event => daemon::run_event(&paths).await,
        Command::StartupHook => daemon::run_startup_hook(&paths),
        Command::SettingsTui => {
            // TUI 阻塞事件循环 + 内部临时 block_on，需脱离 tokio 运行时线程
            let p = paths.clone();
            match std::thread::spawn(move || tui::run_settings_tui(p)).join() {
                Ok(code) => code,
                Err(_) => {
                    eprintln!("✗ 设置弹框异常退出");
                    1
                }
            }
        }
        Command::OpenSettings => cmd_open_settings(&paths),
        Command::AiTest { text } => cmd_ai_test(&paths, &text).await,
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

/// 快捷键入口：以 herdr 插件 pane 形式打开设置弹框（TUI 需要真实终端）。
/// 直接用 plugin_action 后台运行 settings-tui 无 TTY，必然失败。
fn cmd_open_settings(paths: &StatePaths) -> i32 {
    let herdr_bin = std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "herdr".to_string());
    let state_dir = paths.root.to_string_lossy().into_owned();
    let status = std::process::Command::new(&herdr_bin)
        .args([
            "plugin",
            "pane",
            "open",
            "--plugin",
            "wxherd.bridge",
            "--entrypoint",
            "settings",
            "--placement",
            "overlay",
            "--focus",
            "--env",
            &format!("WXHERD_STATE_DIR={state_dir}"),
        ])
        // 关键：不能继承本进程 stdin。action 的 stdin 是管道，若透传会把杂散
        // 字节当按键泄进新 pane（TUI 收到 Esc/s/l 就会关闭或跳转）。
        .stdin(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => 0,
        Ok(s) => {
            eprintln!("✗ 打开设置弹框失败（herdr 退出码 {:?}）", s.code());
            1
        }
        Err(e) => {
            eprintln!("✗ 无法启动 herdr（{herdr_bin}）：{e}");
            1
        }
    }
}

/// 排障：直接跑一次 AI 意图解析，打印 prompt / 原始响应 / 解析结果。
/// 不依赖微信，不走守护进程，方便定位「解析失败」。
async fn cmd_ai_test(paths: &StatePaths, text: &str) -> i32 {
    let cfg = match config::Config::load(paths) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ config.toml 解析失败：{e:#}（已用默认配置）");
            config::Config::defaults()
        }
    };
    let cfg = cfg.expanded();
    println!(
        "接口：{}/chat/completions",
        cfg.ai.base_url.trim_end_matches('/')
    );
    println!(
        "模型：{}",
        if cfg.ai.model.trim().is_empty() {
            "（空）"
        } else {
            cfg.ai.model.trim()
        }
    );
    println!("启用：{}  可用：{}", cfg.ai.enabled, cfg.ai.is_usable());
    if !cfg.ai.is_usable() {
        eprintln!("✗ AI 层不可用（需 enabled + base_url + model + api_key 均非空）");
        return 1;
    }
    let client = match intent::ai::AiClient::new(&cfg.ai) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ 创建客户端失败：{e}");
            return 1;
        }
    };
    // 用真实 pane 列表做快照，与守护进程行为一致
    let snapshot = match herdr::pane_list(&cfg.herdr_bin).await {
        Ok(panes) => {
            let menu = bridge::finalize_menu(bridge::collect_agents(&panes));
            menu.iter()
                .map(|e| {
                    format!(
                        "#{} {} {} pane={}",
                        e.index,
                        e.display_name(),
                        e.status,
                        e.pane_id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Err(e) => {
            println!("（无法获取 pane 列表，用空快照：{e}）");
            String::new()
        }
    };
    let user = intent::ai::AiClient::user_prompt(&snapshot, None, text);
    println!("\n--- 用户 prompt ---\n{user}\n--- 结束 ---\n");
    let raw = match client
        .complete(intent::ai::AiClient::system_prompt(), &user)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ 调用失败：{e}");
            eprintln!("  提示：{}", e.hint());
            return 1;
        }
    };
    println!("--- 原始响应 ---\n{raw}\n--- 结束 ---");
    match intent::ai::AiClient::parse_reply(&raw)
        .and_then(|r| intent::ai::AiClient::validate(r, cfg.ai.min_confidence))
    {
        Ok(reply) => {
            println!("✓ 解析成功：{reply:?}");
            0
        }
        Err(e) => {
            eprintln!("✗ 解析失败：{e}");
            eprintln!("  提示：{}", e.hint());
            1
        }
    }
}

fn cmd_init(paths: &StatePaths) -> i32 {
    if let Err(e) = paths.ensure_dirs() {
        eprintln!("✗ 创建状态目录失败：{e}（{}）", paths.root.display());
        return 1;
    }
    let config_file = paths.config_file();
    if config_file.exists() {
        println!("✓ config.toml 已存在，不覆盖：{}", config_file.display());
    } else {
        if let Err(e) = std::fs::write(&config_file, config::DEFAULT_CONFIG_TOML) {
            eprintln!("✗ 写入 config.toml 失败：{e}");
            return 1;
        }
        println!("✓ 已生成默认配置：{}", config_file.display());
    }
    println!("✓ 状态目录就绪：{}", paths.root.display());
    println!("下一步：运行 `wxherd login` 扫码绑定微信。");
    0
}

// ---------------------------------------------------------------------------
// login（M1：扫码登录全状态机）
// ---------------------------------------------------------------------------

async fn cmd_login(paths: &StatePaths) -> i32 {
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(40))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ 初始化 HTTP 客户端失败：{e}");
            return 1;
        }
    };

    // 固定地址起步；scaned_but_redirect 时切换到 redirect_host 继续轮询
    let mut base_url = QR_API_BASE.to_string();
    println!("正在获取登录二维码…");

    loop {
        let qr = match ilink::client::fetch_qrcode(&http).await {
            Ok(q) => q,
            Err(e) => {
                eprintln!("✗ 获取二维码失败：{e}");
                return 1;
            }
        };
        let img_content = qr
            .qrcode_img_content
            .clone()
            .or_else(|| qr.qrcode.clone())
            .unwrap_or_default();
        if img_content.is_empty() {
            eprintln!("✗ 服务端未返回二维码内容");
            return 1;
        }
        println!(
            "\n请用微信扫描以下二维码（{} 有效期内完成）：\n",
            QR_TTL_HINT
        );
        match render_qr_terminal(&img_content) {
            Ok(art) => println!("{art}"),
            Err(e) => println!("（二维码渲染失败：{e}；内容：{img_content}）"),
        }

        let qrcode_value = qr.qrcode.clone().unwrap_or_default();
        let mut announced_scaned = false;
        let mut verify_code: Option<String> = None;
        let mut need_refresh = false;

        loop {
            let status = match ilink::client::poll_qrcode_status(
                &http,
                &base_url,
                &qrcode_value,
                verify_code.as_deref(),
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    // 单次轮询失败转 wait 继续等（官方客户端同策略）
                    tracing::warn!("二维码状态轮询失败，继续等待：{e}");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };
            match status.state().unwrap_or(QrState::Wait) {
                QrState::Wait => {}
                QrState::Scaned => {
                    if !announced_scaned {
                        announced_scaned = true;
                        println!("已扫码，请在手机上确认登录…");
                    }
                }
                QrState::Expired => {
                    println!("二维码已过期，重新生成…");
                    need_refresh = true;
                }
                QrState::NeedVerifyCode => {
                    if verify_code.is_some() {
                        println!("配对码不正确或已失效，请重新输入手机上显示的配对码。");
                    } else {
                        println!("需要输入配对码：请查看手机上显示的验证码。");
                    }
                    match read_line("请输入配对码：") {
                        Some(code) if !code.trim().is_empty() => {
                            verify_code = Some(code.trim().to_string())
                        }
                        _ => {
                            println!("未输入配对码，重新生成二维码…");
                            need_refresh = true;
                        }
                    }
                }
                QrState::VerifyCodeBlocked => {
                    eprintln!("✗ 配对码错误次数过多，已被限制。请稍后重试 `wxherd login`。");
                    return 1;
                }
                QrState::ScanedButRedirect => {
                    if let Some(host) = status.redirect_host.as_deref().filter(|h| !h.is_empty()) {
                        base_url = format!("https://{host}");
                        println!("服务端重定向，切换轮询地址：{base_url}");
                    }
                }
                QrState::BindedRedirect => {
                    eprintln!(
                        "✗ 该二维码已绑定到其他实例，请重新运行 `wxherd login` 获取新二维码。"
                    );
                    return 1;
                }
                QrState::Confirmed => {
                    let bot_token = status.bot_token.clone().unwrap_or_default();
                    if bot_token.is_empty() {
                        eprintln!("✗ 登录确认成功但服务端未返回 bot_token");
                        return 1;
                    }
                    let credentials = Credentials {
                        bot_token,
                        bot_id: status.ilink_bot_id.clone().unwrap_or_default(),
                        owner_user_id: status.ilink_user_id.clone().unwrap_or_default(),
                        base_url: status
                            .baseurl
                            .clone()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| base_url.clone()),
                    };
                    if let Err(e) = paths
                        .ensure_dirs()
                        .and_then(|_| credentials.save(paths).map_err(std::io::Error::other))
                    {
                        eprintln!("✗ 保存凭据失败：{e}");
                        return 1;
                    }
                    println!(
                        "✓ 绑定成功！凭据已保存：{}",
                        paths.credentials_file().display()
                    );
                    println!(
                        "  账号尾号：{}，主人 ID：{}",
                        credentials.token_suffix(),
                        credentials.owner_user_id
                    );
                    // 通知后端客户端上线（失败不阻断）
                    if let Ok(client) = make_client(&credentials, "wxherd") {
                        if let Err(e) = client.notify_start().await {
                            tracing::warn!("notifystart 失败（不阻断）：{e}");
                        }
                    }
                    return 0;
                }
            }
            if need_refresh {
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

const QR_TTL_HINT: &str = "约 5 分钟";

fn render_qr_terminal(value: &str) -> Result<String> {
    let code = qrcode::QrCode::new(value.as_bytes())?;
    // 终端深色背景：反转前景/背景色保证对比度
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .module_dimensions(2, 2)
        .build())
}

fn read_line(prompt: &str) -> Option<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush().ok()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf).ok()?;
    Some(buf)
}

fn make_client(credentials: &Credentials, bot_agent: &str) -> Result<IlinkClient> {
    IlinkClient::new(credentials.clone(), bot_agent).context("初始化 iLink 客户端失败")
}

fn load_client(paths: &StatePaths) -> Result<IlinkClient> {
    let credentials = Credentials::load(paths)?.with_context(|| {
        format!(
            "尚未绑定微信（{} 不存在），请先运行 `wxherd login`",
            paths.credentials_file().display()
        )
    })?;
    let cfg = config::Config::load(paths)
        .map(|c| c.expanded())
        .unwrap_or_default();
    make_client(&credentials, &cfg.wechat.bot_agent)
}

// ---------------------------------------------------------------------------
// status / doctor
// ---------------------------------------------------------------------------

fn cmd_status(paths: &StatePaths) -> i32 {
    println!("状态目录：{}", paths.root.display());

    let config_ok = paths.config_file().exists();
    println!(
        "config.toml：{}",
        if config_ok {
            "✓ 存在"
        } else {
            "✗ 不存在（运行 wxherd init）"
        }
    );

    match Credentials::load(paths) {
        Ok(Some(c)) => println!("微信绑定：✓ 已绑定（尾号 {}）", c.token_suffix()),
        Ok(None) => println!("微信绑定：✗ 未绑定（运行 wxherd login）"),
        Err(e) => println!("微信绑定：✗ 凭据损坏：{e}"),
    }

    let runtime = config::RuntimeState::load(paths);
    let pid_file = std::fs::read_to_string(paths.daemon_pid_file())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let pid = pid_file.or(runtime.pid);
    match pid {
        Some(pid) if herdr::pid_alive(pid) => println!("守护进程：● 运行中（pid {pid}）"),
        Some(pid) => println!("守护进程：✗ 未运行（pid {pid} 已退出）"),
        None => println!("守护进程：✗ 未运行"),
    }
    match runtime.last_poll_ms {
        Some(ms) => println!("最近心跳：{ms} ms（失败 {} 次）", runtime.fail_count),
        None => println!("最近心跳：无"),
    }
    0
}

async fn cmd_doctor(paths: &StatePaths) -> i32 {
    println!("wxherd 诊断（{}）\n", paths.root.display());
    let mut failures = 0u32;
    let mut check = |name: &str, ok: bool, detail: &str| {
        let mark = if ok { "✓" } else { "✗" };
        println!(" {mark} {name}：{detail}");
        if !ok {
            failures += 1;
        }
    };

    // 1. 状态目录与配置
    let dir_ok = paths.root.exists();
    check("状态目录", dir_ok, &paths.root.display().to_string());

    if dir_ok && paths.config_file().exists() {
        match config::Config::load(paths) {
            Ok(cfg) => {
                check("config.toml", true, "可解析");
                // 2. herdr 可执行
                let herdr_bin = cfg.herdr_binary();
                let version = run_capture(&herdr_bin, &["--version"]);
                match version {
                    Ok(out) => {
                        check("herdr", true, &format!("{}（{}）", out.trim(), herdr_bin));
                    }
                    Err(e) => {
                        check("herdr", false, &format!("无法执行 `{}`：{e}", herdr_bin));
                    }
                }
                // 3. AI 配置
                let ai = cfg.expanded().ai;
                if ai.is_usable() {
                    check(
                        "AI 意图层",
                        !ai.base_url.is_empty() && !ai.model.is_empty(),
                        &format!("{} / {}", ai.base_url, ai.model),
                    );
                } else {
                    check("AI 意图层", true, "未启用（未配置 api_key，仅本地规则层）");
                }
            }
            Err(e) => {
                check("config.toml", false, &format!("解析失败：{e}"));
            }
        }
    } else {
        check("config.toml", false, "不存在（运行 wxherd init）");
    }

    // 4. 微信凭据
    match Credentials::load(paths) {
        Ok(Some(c)) => {
            check(
                "微信绑定",
                true,
                &format!("已绑定（尾号 {}）", c.token_suffix()),
            );
            // 5. 凭据 API base 可用性（仅格式检查，不真发请求）
            let base = if c.base_url.is_empty() {
                QR_API_BASE
            } else {
                c.base_url.as_str()
            };
            check("API 地址", base.starts_with("https://"), base);
        }
        Ok(None) => {
            check("微信绑定", false, "未绑定（运行 wxherd login）");
        }
        Err(e) => {
            check("微信绑定", false, &format!("凭据损坏：{e}"));
        }
    }

    // 6. 媒体目录可写
    let media_dir = paths.media_dir();
    let media_ok = std::fs::create_dir_all(&media_dir).is_ok();
    check("媒体目录", media_ok, &media_dir.display().to_string());

    println!();
    if failures == 0 {
        println!("全部通过 ✓");
        0
    } else {
        println!("{failures} 项未通过 ✗");
        1
    }
}

fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("启动 {program} 失败"))?;
    if !output.status.success() {
        bail!("{} 退出码 {:?}", program, output.status.code());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// send-file（M2）
// ---------------------------------------------------------------------------

async fn cmd_send_file(paths: &StatePaths, path: &Path, caption: Option<&str>) -> i32 {
    if !path.exists() {
        eprintln!("✗ 文件不存在：{}", path.display());
        return 1;
    }
    let client = match load_client(paths) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ {e:#}");
            return 1;
        }
    };
    let to = client.credentials.owner_user_id.clone();
    match client.send_file(&to, path, caption, None).await {
        Ok(_) => {
            println!("✓ 文件已发送（{}）", path.display());
            0
        }
        Err(e) => {
            eprintln!("✗ 发送失败：{e}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// send-test / send-image（M1）
// ---------------------------------------------------------------------------

async fn cmd_send_test(paths: &StatePaths) -> i32 {
    let client = match load_client(paths) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ {e:#}");
            return 1;
        }
    };
    let to = client.credentials.owner_user_id.clone();
    if client.current_context_token().await.is_none() {
        eprintln!("⚠ 冷启动：尚未收到过你的微信消息，发送可能被服务端拒绝（ret=-2）。");
        eprintln!("  若失败，请在微信里先给 bot 发一条消息再重试。");
    }
    let text = "🤖 wxherd 测试消息：微信↔Herdr 桥接通道正常。";
    match client.send_text(&to, text, None, None).await {
        Ok(_) => {
            println!("✓ 测试消息已发送给主人账号（{to}）");
            0
        }
        Err(e) => {
            eprintln!("✗ 发送失败：{e}");
            1
        }
    }
}

async fn cmd_send_image(paths: &StatePaths, path: &Path, caption: Option<&str>) -> i32 {
    if !path.exists() {
        eprintln!("✗ 文件不存在：{}", path.display());
        return 1;
    }
    let client = match load_client(paths) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ {e:#}");
            return 1;
        }
    };
    let to = client.credentials.owner_user_id.clone();
    match client.send_image(&to, path, caption, None).await {
        Ok(_) => {
            println!("✓ 图片已发送（{}）", path.display());
            0
        }
        Err(e) => {
            eprintln!("✗ 发送失败：{e}");
            1
        }
    }
}
