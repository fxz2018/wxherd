//! 配置与状态文件管理：状态目录解析、`${ENV}` 展开、config.toml / credentials.json / sync.json 读写。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 入站媒体默认落盘子目录（相对状态目录）。
pub const DEFAULT_MEDIA_DIR: &str = "media";

/// 状态目录。解析顺序：`--state-dir` → `WXHERD_STATE_DIR` → `HERDR_STATE_DIR` → `~/.wxherd`。
///
/// 刻意不读 `HERDR_PLUGIN_CONFIG_DIR`（herdr 会向所有插件运行时命令注入它）：
/// 若插件上下文与手动启动解析出不同状态目录，daemon.lock 会各自独立，
/// 双实例同时长轮询同一个微信 bot。单实例保证要求状态目录与启动上下文无关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    pub root: PathBuf,
}

impl StatePaths {
    pub fn resolve(override_dir: Option<&Path>) -> Self {
        let root = override_dir
            .map(|p| p.to_path_buf())
            .or_else(|| Self::env_dir("WXHERD_STATE_DIR"))
            .or_else(|| Self::env_dir("HERDR_STATE_DIR"))
            .or_else(|| Self::home_dir().map(|h| h.join(".wxherd")))
            .unwrap_or_else(|| PathBuf::from(".wxherd"));
        Self { root }
    }

    /// 取非空环境变量为路径（空串视为未设置）。
    fn env_dir(name: &str) -> Option<PathBuf> {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }

    fn home_dir() -> Option<PathBuf> {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(self.media_dir())?;
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn credentials_file(&self) -> PathBuf {
        self.root.join("credentials.json")
    }

    pub fn sync_file(&self) -> PathBuf {
        self.root.join("sync.json")
    }

    pub fn runtime_file(&self) -> PathBuf {
        self.root.join("runtime.json")
    }

    pub fn notify_state_file(&self) -> PathBuf {
        self.root.join("notify-state.json")
    }

    /// 通知去重跨进程锁（event 与轮询双路径原子 claim 用）。
    pub fn notify_lock_file(&self) -> PathBuf {
        self.root.join("notify-state.lock")
    }

    pub fn learned_rules_file(&self) -> PathBuf {
        self.root.join("learned-rules.json")
    }

    pub fn daemon_pid_file(&self) -> PathBuf {
        self.root.join("daemon.pid")
    }

    pub fn daemon_log_file(&self) -> PathBuf {
        self.root.join("daemon.log")
    }

    pub fn daemon_lock_file(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }

    pub fn media_dir(&self) -> PathBuf {
        self.root.join(DEFAULT_MEDIA_DIR)
    }
}

/// 展开字符串中的 `${ENV_NAME}` 引用。未设置的环境变量展开为空串；
/// 名称不合法（空、含空白、格式非 [A-Za-z_][A-Za-z0-9_]*）时保持原文。
pub fn expand_env(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{') {
            if let Some(end) = input[i + 2..].find('}') {
                let name = &input[i + 2..i + 2 + end];
                if is_valid_env_name(name) {
                    out.push_str(&std::env::var(name).unwrap_or_default());
                } else {
                    // 名称不合法（含嵌套 `${`）：整体原样保留，避免内层被二次展开。
                    out.push_str(&input[i..i + 2 + end + 1]);
                }
                i += 2 + end + 1;
                continue;
            }
        }
        let ch_len = input[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&input[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// serde(default)：旧版 config.toml 缺新字段时按字段默认补齐，不整份解析失败。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub blocked: bool,
    pub done: bool,
    pub notify_when_focused: bool,
    pub cooldown_seconds: u64,
    /// 完成推送的最低任务时长：短于此的任务不推（秒）。0 = 全部推送。
    pub min_done_duration_seconds: u64,
    /// 免打扰时段，如 `["22:00-08:00"]`，空 = 不启用。
    pub quiet_hours: Vec<String>,
    pub blocked_delay_seconds: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            blocked: true,
            done: true,
            notify_when_focused: true,
            cooldown_seconds: 60,
            min_done_duration_seconds: 10,
            quiet_hours: Vec::new(),
            blocked_delay_seconds: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WechatConfig {
    /// 观测标识，仅日志归因用。
    pub bot_agent: String,
    pub long_poll_seconds: u64,
    pub max_command_bytes: usize,
    pub max_send_bytes: usize,
    pub media_dir: String,
}

impl Default for WechatConfig {
    fn default() -> Self {
        Self {
            bot_agent: "wxherd".to_string(),
            long_poll_seconds: 35,
            max_command_bytes: 4000,
            max_send_bytes: 2000,
            media_dir: DEFAULT_MEDIA_DIR.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub timeout_seconds: u64,
    pub min_confidence: f64,
    pub max_learned_rules: usize,
    pub rule_match_threshold: f64,
    /// 调用 AI 前先回一条「正在理解你的意思」即时提示。
    #[serde(default = "default_true")]
    pub thinking_notice: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            // 慢端点（中转站/推理模型）很常见，默认给足 3 分钟
            timeout_seconds: 180,
            min_confidence: 0.7,
            max_learned_rules: 500,
            rule_match_threshold: 0.9,
            thinking_notice: true,
        }
    }
}

impl AiConfig {
    /// api_key 展开后为空即视为未启用（设计约束：未配置密钥只跑本地规则层）。
    pub fn is_usable(&self) -> bool {
        // 四个条件缺一不可：否则会拿着空 model / 空地址去请求，每次都 400，
        // 白白把消息送去 AI 又回退菜单。
        self.enabled
            && !self.api_key.trim().is_empty()
            && !self.base_url.trim().is_empty()
            && !self.model.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypingConfig {
    pub enabled: bool,
}

impl Default for TypingConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub herdr_bin: String,
    pub read_lines: u32,
    pub poll_interval_seconds: u64,
    pub auto_start: bool,
    pub auto_restart: bool,
    /// herdr 连续不可达超过该分钟数后守护进程自动退出（下次 herdr 启动时
    /// startup hook 自动补拉）；0 = 永不自动退出。
    pub herdr_gone_exit_minutes: u64,
    pub notify: NotifyConfig,
    pub wechat: WechatConfig,
    pub ai: AiConfig,
    pub typing: TypingConfig,
}

/// 缺省值统一走 defaults()：serde 缺字段（旧 config.toml 增量升级）与
/// Config::default() 都得到合理初值，而不是数值 0 / 空串。
impl Default for Config {
    fn default() -> Self {
        Self::defaults()
    }
}

/// 手写默认值（显式列出，避免依赖 struct 默认推导次序）。
impl Config {
    pub fn defaults() -> Self {
        Self {
            herdr_bin: "herdr".to_string(),
            read_lines: 40,
            poll_interval_seconds: 2,
            auto_start: true,
            auto_restart: true,
            herdr_gone_exit_minutes: 5,
            notify: NotifyConfig::default(),
            wechat: WechatConfig::default(),
            ai: AiConfig::default(),
            typing: TypingConfig::default(),
        }
    }

    /// herdr 可执行文件：插件运行时优先 HERDR_BIN_PATH。
    pub fn herdr_binary(&self) -> String {
        std::env::var("HERDR_BIN_PATH")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.herdr_bin.clone())
    }

    pub fn load(paths: &StatePaths) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(paths.config_file())?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }

    pub fn save(&self, paths: &StatePaths) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(paths.config_file(), text)?;
        Ok(())
    }

    /// 对所有字符串值做 `${ENV}` 展开，返回展开后的新配置。
    pub fn expanded(&self) -> Self {
        let mut cfg = self.clone();
        cfg.ai.base_url = expand_env(&cfg.ai.base_url);
        cfg.ai.api_key = expand_env(&cfg.ai.api_key);
        cfg.herdr_bin = expand_env(&cfg.herdr_bin);
        cfg
    }
}

pub const DEFAULT_CONFIG_TOML: &str = r#"# wxherd 配置文件（完整字段说明见 docs/config-reference.md）

# herdr 可执行文件；默认从 PATH 找 "herdr"，插件运行时优先用 HERDR_BIN_PATH
herdr_bin = "herdr"

# /read 未指定行数时的默认读取行数（1-200）
read_lines = 40

# agent 状态扫描间隔（秒，最小 1）
poll_interval_seconds = 2

# 跟随 herdr 启动时是否自动拉起守护进程
auto_start = true

# 守护进程崩溃后自动重启
auto_restart = true

# herdr 连续不可达超过该分钟数后守护进程自动退出（0 = 永不自动退出）。
# 下次 herdr 启动时 startup hook 会自动补拉，无需手动操作
herdr_gone_exit_minutes = 5

[notify]
# 阻塞推送（agent 等你批准/回答）
blocked = true
# 完成推送（working|blocked → idle 也视为完成）
done = true
# 正盯着该 pane 时也推送（true = 无论前台后台都推；false = 只推后台 pane）
notify_when_focused = true
# 同一 pane 同类型通知去重间隔（秒）
cooldown_seconds = 60
# 完成推送的最低任务时长（秒）：短于此不推，0 = 全部推送
min_done_duration_seconds = 10
# 免打扰时段（本地时间，支持跨天），空 = 不启用
quiet_hours = []
# 阻塞推送延迟（秒），等 agent 自动批准机制先跑
blocked_delay_seconds = 10

[wechat]
# 观测标识，仅日志归因用
bot_agent = "wxherd"
# 长轮询超时（秒）
long_poll_seconds = 35
# 命令长度上限（字节）
max_command_bytes = 4000
# /send 文本上限（字节）
max_send_bytes = 2000
# 入站媒体保存目录（相对状态目录，也可绝对路径）
media_dir = "media"

[ai]
# AI 意图层总开关；未配置 api_key 时自动停用，仅本地规则层工作
enabled = true
# 任意 OpenAI Chat Completions 兼容端点
base_url = "https://open.bigmodel.cn/api/paas/v4"
model = "glm-4-flash"
# 建议走环境变量，不落盘
api_key = "${WXHERD_AI_KEY}"
# 请求超时（秒）。慢端点（中转站/推理模型）建议 120-300
timeout_seconds = 180
# 低于此置信度不执行，回确认菜单
min_confidence = 0.7
# 调用 AI 前先回一条「正在理解你的意思」即时提示
thinking_notice = true
# 学习规则上限（超出按 hit_count 淘汰）
max_learned_rules = 500
# 学习规则命中相似度阈值（归一化后）
rule_match_threshold = 0.9

[typing]
# 处理命令期间显示"正在输入…"（需要 getConfig 拿 typing_ticket）
enabled = true
"#;

/// 扫码登录凭据（credentials.json）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    pub bot_token: String,
    #[serde(default)]
    pub bot_id: String,
    pub owner_user_id: String,
    #[serde(default)]
    pub base_url: String,
}

impl Credentials {
    pub fn load(paths: &StatePaths) -> anyhow::Result<Option<Self>> {
        let path = paths.credentials_file();
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    pub fn save(&self, paths: &StatePaths) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(paths.credentials_file(), text)?;
        Ok(())
    }

    /// token 尾号，用于界面展示（脱敏）。
    pub fn token_suffix(&self) -> String {
        self.bot_token
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

/// getupdates 游标（sync.json）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default)]
    pub get_updates_buf: String,
}

impl SyncState {
    pub fn load(paths: &StatePaths) -> Self {
        std::fs::read_to_string(paths.sync_file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &StatePaths) -> anyhow::Result<()> {
        std::fs::write(paths.sync_file(), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// 守护进程心跳（runtime.json）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeState {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub last_poll_ms: Option<u64>,
    #[serde(default)]
    pub fail_count: u64,
    /// 临时静音截止时间（ms，微信命令「静音1小时」设置）。
    #[serde(default)]
    pub mute_until_ms: Option<u64>,
}

impl RuntimeState {
    pub fn load(paths: &StatePaths) -> Self {
        std::fs::read_to_string(paths.runtime_file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &StatePaths) -> anyhow::Result<()> {
        std::fs::write(paths.runtime_file(), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// 测试辅助：从字符串加载（供测试与未来复用）。
#[cfg(test)]
impl SyncState {
    pub fn load_from_str(text: &str) -> SyncState {
        serde_json::from_str(text).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 环境变量测试不能并行跑（会互相污染进程级 env）。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_state_dir_resolution_priority() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("WXHERD_STATE_DIR");
        std::env::remove_var("HERDR_STATE_DIR");
        std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR");

        // 默认：~/.wxherd
        let paths = StatePaths::resolve(None);
        let home = StatePaths::home_dir().expect("测试机应有 USERPROFILE/HOME");
        assert_eq!(paths.root, home.join(".wxherd"));

        // herdr 注入的插件目录必须被忽略（单实例保证：状态目录与启动上下文无关）
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", "E:/tmp-wxherd-plugin");
        let paths = StatePaths::resolve(None);
        assert_eq!(
            paths.root,
            home.join(".wxherd"),
            "HERDR_PLUGIN_CONFIG_DIR 不得参与解析"
        );

        // HERDR_STATE_DIR 显式覆盖
        std::env::set_var("HERDR_STATE_DIR", "E:/tmp-wxherd-state");
        let paths = StatePaths::resolve(None);
        assert_eq!(paths.root, PathBuf::from("E:/tmp-wxherd-state"));

        // 显式 --state-dir 最高优先
        let paths = StatePaths::resolve(Some(Path::new("E:/tmp-wxherd-cli")));
        assert_eq!(paths.root, PathBuf::from("E:/tmp-wxherd-cli"));

        // WXHERD_STATE_DIR 优于 HERDR_STATE_DIR
        std::env::set_var("WXHERD_STATE_DIR", "E:/tmp-wxherd-own");
        let paths = StatePaths::resolve(None);
        assert_eq!(paths.root, PathBuf::from("E:/tmp-wxherd-own"));
        // 空串视为未设置
        std::env::set_var("WXHERD_STATE_DIR", "");
        let paths = StatePaths::resolve(None);
        assert_eq!(paths.root, PathBuf::from("E:/tmp-wxherd-state"));

        std::env::remove_var("WXHERD_STATE_DIR");
        std::env::remove_var("HERDR_STATE_DIR");
        std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR");
    }

    #[test]
    fn test_state_paths_layout() {
        let paths = StatePaths {
            root: PathBuf::from("E:/tmp-wxherd-root"),
        };
        assert_eq!(
            paths.config_file(),
            PathBuf::from("E:/tmp-wxherd-root/config.toml")
        );
        assert_eq!(
            paths.credentials_file(),
            PathBuf::from("E:/tmp-wxherd-root/credentials.json")
        );
        assert_eq!(
            paths.sync_file(),
            PathBuf::from("E:/tmp-wxherd-root/sync.json")
        );
        assert_eq!(
            paths.runtime_file(),
            PathBuf::from("E:/tmp-wxherd-root/runtime.json")
        );
        assert_eq!(
            paths.notify_state_file(),
            PathBuf::from("E:/tmp-wxherd-root/notify-state.json")
        );
        assert_eq!(
            paths.notify_lock_file(),
            PathBuf::from("E:/tmp-wxherd-root/notify-state.lock")
        );
        assert_eq!(
            paths.learned_rules_file(),
            PathBuf::from("E:/tmp-wxherd-root/learned-rules.json")
        );
        assert_eq!(
            paths.daemon_pid_file(),
            PathBuf::from("E:/tmp-wxherd-root/daemon.pid")
        );
        assert_eq!(
            paths.daemon_log_file(),
            PathBuf::from("E:/tmp-wxherd-root/daemon.log")
        );
        assert_eq!(
            paths.daemon_lock_file(),
            PathBuf::from("E:/tmp-wxherd-root/daemon.lock")
        );
        assert_eq!(paths.media_dir(), PathBuf::from("E:/tmp-wxherd-root/media"));
    }

    #[test]
    fn test_expand_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("WXHERD_TEST_KEY", "sk-123");

        assert_eq!(expand_env("纯文本不变"), "纯文本不变");
        assert_eq!(expand_env("${WXHERD_TEST_KEY}"), "sk-123");
        assert_eq!(expand_env("Bearer ${WXHERD_TEST_KEY}!"), "Bearer sk-123!");
        assert_eq!(expand_env("${A}${WXHERD_TEST_KEY}"), "sk-123");
        // 未设置的变量展开为空串
        assert_eq!(expand_env("key=${WXHERD_TEST_MISSING}"), "key=");
        // 名称不合法保持原文
        assert_eq!(expand_env("${1BAD}"), "${1BAD}");
        assert_eq!(expand_env("${BAD NAME}"), "${BAD NAME}");
        assert_eq!(expand_env("${ unclosed"), "${ unclosed");
        assert_eq!(expand_env("${}"), "${}");
        // 嵌套花括号内容不匹配合法名，保持原样
        assert_eq!(
            expand_env("${WXHERD_TEST_KEY ${X}}"),
            "${WXHERD_TEST_KEY ${X}}"
        );

        std::env::remove_var("WXHERD_TEST_KEY");
    }

    #[test]
    fn test_expand_env_utf8_preserved() {
        assert_eq!(expand_env("中文✓${WXHERD_TEST_MISSING}保留"), "中文✓保留");
    }

    #[test]
    fn test_default_config_toml_parses() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("默认模板必须可被解析");
        assert_eq!(cfg.herdr_bin, "herdr");
        assert_eq!(cfg.read_lines, 40);
        assert_eq!(cfg.poll_interval_seconds, 2);
        assert!(cfg.auto_start && cfg.auto_restart);
        assert_eq!(cfg.herdr_gone_exit_minutes, 5);
        assert!(cfg.notify.blocked && cfg.notify.done);
        assert_eq!(cfg.notify.cooldown_seconds, 60);
        assert!(cfg.notify.quiet_hours.is_empty());
        assert_eq!(cfg.wechat.long_poll_seconds, 35);
        assert_eq!(cfg.wechat.bot_agent, "wxherd");
        assert_eq!(cfg.wechat.max_send_bytes, 2000);
        assert_eq!(cfg.ai.model, "glm-4-flash");
        assert_eq!(cfg.ai.api_key, "${WXHERD_AI_KEY}");
        assert_eq!(cfg.ai.min_confidence, 0.7);
        assert_eq!(cfg.ai.max_learned_rules, 500);
        assert_eq!(cfg.ai.rule_match_threshold, 0.9);
        assert!(cfg.typing.enabled);
    }

    #[test]
    fn test_config_roundtrip_and_defaults() {
        let cfg = Config::defaults();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, cfg);

        // 缺失段落全部走默认值（向后兼容增量加字段）
        let parsed: Config = toml::from_str("herdr_bin = \"x\"").unwrap();
        assert_eq!(parsed.herdr_bin, "x");
        assert_eq!(parsed.notify, NotifyConfig::default());
        assert_eq!(parsed.ai, AiConfig::default());
        assert_eq!(
            parsed.herdr_gone_exit_minutes, 5,
            "旧配置缺新字段时 serde 走 defaults() 而非数值 0"
        );
    }

    #[test]
    fn test_config_expanded() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("WXHERD_TEST_KEY", "abc");
        let mut cfg = Config::defaults();
        cfg.ai.api_key = "${WXHERD_TEST_KEY}".to_string();
        let ex = cfg.expanded();
        assert_eq!(ex.ai.api_key, "abc");
        assert_eq!(cfg.ai.api_key, "${WXHERD_TEST_KEY}"); // 原配置不动
        std::env::remove_var("WXHERD_TEST_KEY");
    }

    /// 回归：旧版 config.toml 缺少新增字段（min_done_duration_seconds 等）时必须能解析，
    /// 否则整份配置回退默认值——曾导致 AI 层静默失效（base_url/model 全空）。
    #[test]
    fn test_legacy_config_missing_new_fields_parses() {
        let legacy = r#"
herdr_bin = "herdr"
read_lines = 40
poll_interval_seconds = 2
auto_start = true
auto_restart = true
herdr_gone_exit_minutes = 0

[notify]
blocked = true
done = true
notify_when_focused = true
cooldown_seconds = 5
quiet_hours = []
blocked_delay_seconds = 10

[wechat]
bot_agent = "wxherd"
long_poll_seconds = 35
max_command_bytes = 4000
max_send_bytes = 2000
media_dir = "media"

[ai]
enabled = true
base_url = "https://open.bigmodel.cn/api/paas/v4"
model = "glm-4-flash"
api_key = "k"
timeout_seconds = 180
min_confidence = 0.7
max_learned_rules = 500
rule_match_threshold = 0.9

[typing]
enabled = true
"#;
        let cfg: Config = toml::from_str(legacy).expect("旧配置必须可解析");
        assert_eq!(cfg.ai.model, "glm-4-flash", "AI 配置不得因缺字段而丢失");
        assert_eq!(cfg.ai.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert!(cfg.ai.is_usable(), "旧配置的 AI 层应可用");
        // 缺失的新字段走字段默认
        assert_eq!(cfg.notify.min_done_duration_seconds, 10);
    }

    #[test]
    fn test_ai_usable_gate() {
        let mut ai = AiConfig::default();
        assert!(!ai.is_usable(), "未配置 api_key 时 AI 层必须停用");
        ai.api_key = "k".to_string();
        assert!(
            !ai.is_usable(),
            "缺 base_url / model 时 AI 层不得启用（否则每请求必 400）"
        );
        ai.base_url = "https://api.test/v1".to_string();
        assert!(!ai.is_usable(), "缺 model 时仍不得启用");
        ai.model = "m".to_string();
        assert!(ai.is_usable());
        ai.enabled = false;
        assert!(!ai.is_usable());
    }

    #[test]
    fn test_credentials_roundtrip_and_suffix() {
        let c = Credentials {
            bot_token: "abcdwxyz1234".to_string(),
            bot_id: "bot-1".to_string(),
            owner_user_id: "u-1".to_string(),
            base_url: "https://ilinkai.weixin.qq.com".to_string(),
        };
        assert_eq!(c.token_suffix(), "1234");

        let json = serde_json::to_string(&c).unwrap();
        let parsed: Credentials = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, c);
    }

    #[test]
    fn test_sync_and_runtime_state() {
        let sync = SyncState {
            get_updates_buf: "cursor-1".to_string(),
        };
        let json = serde_json::to_string(&sync).unwrap();
        let parsed: SyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, sync);
        assert_eq!(SyncState::default(), SyncState::load_from_str("{}"));

        let rt = RuntimeState {
            pid: Some(42),
            last_poll_ms: Some(1000),
            fail_count: 3,
            mute_until_ms: None,
        };
        let json = serde_json::to_string_pretty(&rt).unwrap();
        let parsed: RuntimeState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rt);
        let empty: RuntimeState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, RuntimeState::default());
        let _ = json;
    }
}
