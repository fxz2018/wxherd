//! AI 意图解析层（层③）：OpenAI 兼容 Chat Completions 客户端、JSON schema 强约束、
//! 动作白名单、置信度门槛。解析失败/超时/网络错误由调用方回确认菜单（绝不瞎猜）。

use std::time::Duration;

use serde::Deserialize;

use super::Intent;
use crate::config::AiConfig;

/// 动作白名单（AI 无权发明新操作）。
pub const ACTION_WHITELIST: [&str; 6] = ["approve", "deny", "interrupt", "read", "send", "status"];

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AiAction {
    pub action: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct AiLearn {
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub template: String,
    #[serde(default)]
    pub example: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AiReply {
    pub actions: Vec<AiAction>,
    pub confidence: f64,
    #[serde(default)]
    pub learn: Option<AiLearn>,
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("网络请求失败：{0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("响应解析失败：{0}")]
    Parse(String),
    #[error("{0}")]
    Invalid(String),
}

impl AiError {
    /// 是否瞬时故障（值得重试一次）：网络错误、429 限流、5xx 上游不可用。
    /// 超时**不**重试：用户已经把 timeout 调得很大，再重一次等于等两倍。
    pub fn is_transient(&self) -> bool {
        match self {
            AiError::Http(e) => !e.is_timeout(),
            AiError::Status(code) => *code == 429 || (500..=599).contains(code),
            // 模型偶发输出截断/不合 schema，重试一次往往就好
            AiError::Parse(_) => true,
            AiError::Invalid(_) => false,
        }
    }

    /// 给用户看的中文提示（含排查建议）。
    pub fn hint(&self) -> String {
        match self {
            AiError::Status(503) | AiError::Status(502) | AiError::Status(504) => {
                "上游服务暂时不可用（已重试一次）。可能中转站过载，稍后再说一次即可；\
                 若持续，请检查 base_url 是否可用"
                    .to_string()
            }
            AiError::Status(429) => "上游限流（已重试一次），请稍后再试或更换端点".to_string(),
            AiError::Status(code) => {
                format!("上游返回 HTTP {code}，请检查 base_url / model / 密钥")
            }
            AiError::Http(e) => {
                if e.is_timeout() {
                    "请求超时，没有重试（已调至较大超时时间）。这个端点很慢，一般要等 1-3 分钟。\
可在设置弹框或 config.toml 里调大 ai.timeout_seconds 以获取更多容忍时间"
                        .to_string()
                } else {
                    format!("网络错误：{e}")
                }
            }
            AiError::Parse(msg) => {
                format!("模型输出无法解析（已重试一次）：{msg}。可换更稳的模型")
            }
            AiError::Invalid(msg) => msg.clone(),
        }
    }
}

pub struct AiClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl AiClient {
    pub fn new(cfg: &AiConfig) -> Result<Self, AiError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_seconds.max(1)))
            .build()?;
        Ok(Self {
            http,
            base_url: normalize_base_url(&cfg.base_url),
            model: cfg.model.trim().to_string(),
            api_key: cfg.api_key.trim().to_string(),
        })
    }

    /// 系统提示词（中文硬编码，含 schema 与白名单强约束）。
    pub fn system_prompt() -> &'static str {
        "你是 Herdr 终端桥的意图解析器。把用户的中文消息翻译成 JSON 指令，只输出 JSON，不要解释，不要执行。\n\
输出格式：\n\
{\"actions\":[{\"action\":\"approve|deny|interrupt|read|send|status\",\"target\":\"#N 或 pane ID 或 项目名\",\"text\":\"仅 send 需要，原样保留用户要发的话\"}],\"confidence\":0.0到1.0,\"learn\":{\"keywords\":[\"关键词\"],\"template\":\"approve {target}\",\"example\":\"原句泛化\"}}\n\
约束：\n\
1. action 只能是 approve/deny/interrupt/read/send/status 六选一\n\
2. target 必须从给定的 agent 列表里选（用 #序号 最好）；无明确目标就省略\n\
3. send 的 text 原样透传，禁止改写\n\
4. confidence 表示你对解析正确性的把握\n\
5. learn 仅当说法有泛化价值时给出：template 用动作+{target}占位符，example 是这句话的通用说法；没有就省略\n\
6. 无法理解时输出 {\"actions\":[],\"confidence\":0}"
    }

    /// 用户消息提示词：agent 列表快照 + 最近交互上下文 + 用户原话。
    pub fn user_prompt(snapshot: &str, last_target: Option<&str>, text: &str) -> String {
        let mut out = String::from("当前 agent 列表：\n");
        out.push_str(snapshot);
        out.push('\n');
        match last_target {
            Some(t) => out.push_str(&format!("最近交互目标：{t}\n")),
            None => out.push_str("最近交互目标：无\n"),
        }
        out.push_str(&format!("\n用户说：「{text}」"));
        out
    }

    /// 调用 Chat Completions，返回模型文本。瞬时故障（5xx/429/超时/解析失败）重试一次。
    pub async fn complete(&self, system: &str, user: &str) -> Result<String, AiError> {
        match self.complete_once(system, user).await {
            Ok(text) => Ok(text),
            Err(e) if e.is_transient() => {
                tracing::warn!("AI 首次失败（{e}），重试一次");
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                let r = self.complete_once(system, user).await;
                if let Err(e2) = &r {
                    tracing::warn!("AI 重试仍失败：{e2}");
                }
                r
            }
            Err(e) => Err(e),
        }
    }

    async fn complete_once(&self, system: &str, user: &str) -> Result<String, AiError> {
        if self.base_url.is_empty() || self.api_key.is_empty() {
            return Err(AiError::Invalid("AI 未配置 base_url / api_key".into()));
        }
        let url = format!("{}/chat/completions", self.base_url);
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            "temperature": 0.1,
            // 意图解析只需一小段 JSON；256 足够，太大反而拖慢响应
            "max_tokens": 256,
            // 关闭深度思考：GLM/o1 类模型思考会把 max_tokens 耗尽在
            // reasoning 上导致 content 为 null（finish_reason=length）。
            // 智谱接受该字段；OpenAI 兼容端点不认识的字段通常忽略。
            "thinking": {"type": "disabled"}
        });
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "AI HTTP {}｜url={} model={}｜响应体（前 500 字符）：{}",
                status.as_u16(),
                url,
                self.model,
                truncate_for_log(&body, 500)
            );
            return Err(AiError::Status(status.as_u16()));
        }
        let v: serde_json::Value = resp.json().await?;
        let msg = v.pointer("/choices/0/message");
        // content 优先；为空时兜底取 reasoning_content（思考型模型偶尔把答案
        // 写进思考段就结束），两处都没有才报错。
        let content = msg
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .filter(|c| !c.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                msg.and_then(|m| m.get("reasoning_content"))
                    .and_then(|c| c.as_str())
                    .filter(|c| !c.trim().is_empty())
                    .map(str::to_string)
            });
        if let Some(c) = content {
            return Ok(c);
        }
        let finish = v
            .pointer("/choices/0/finish_reason")
            .and_then(|f| f.as_str())
            .unwrap_or("unknown");
        tracing::warn!(
            "AI 响应无 content｜finish_reason={finish}｜原始响应：{}",
            truncate_for_log(&v.to_string(), 500)
        );
        Err(AiError::Parse(format!(
            "模型未返回 content（finish_reason={finish}）；推理型模型建议调大 max_tokens 或换非推理模型"
        )))
    }

    /// 解析模型文本为 AiReply：容忍 ```json 围栏与前后废话（取首个 { 到最后一个 }）。
    pub fn parse_reply(raw: &str) -> Result<AiReply, AiError> {
        let s = strip_code_fence(raw);
        let start = s
            .find('{')
            .ok_or_else(|| AiError::Parse("响应中没有 JSON 对象".into()))?;
        let end = s
            .rfind('}')
            .ok_or_else(|| AiError::Parse("响应中没有 JSON 对象".into()))?;
        if end < start {
            return Err(AiError::Parse("JSON 对象区间不合法".into()));
        }
        let json = &s[start..=end];
        serde_json::from_str(json).map_err(|e| AiError::Parse(format!("JSON 不合 schema：{e}")))
    }

    /// 校验：置信度门槛 + 动作白名单 + send 必须带文本。
    pub fn validate(reply: AiReply, min_confidence: f64) -> Result<AiReply, AiError> {
        if !(0.0..=1.0).contains(&reply.confidence) {
            return Err(AiError::Invalid(format!(
                "置信度 {} 不在 0-1 区间",
                reply.confidence
            )));
        }
        if reply.confidence < min_confidence {
            return Err(AiError::Invalid(format!(
                "置信度 {:.2} 低于门槛 {:.2}",
                reply.confidence, min_confidence
            )));
        }
        for a in &reply.actions {
            if !ACTION_WHITELIST.contains(&a.action.as_str()) {
                return Err(AiError::Invalid(format!("未知动作「{}」", a.action)));
            }
            if a.action == "send" && a.text.as_deref().map(str::trim).unwrap_or("").is_empty() {
                return Err(AiError::Invalid("send 动作缺少 text".into()));
            }
        }
        Ok(reply)
    }
}

/// 去掉 ``` 围栏（含语言标记行）。
/// 日志里截断长文本（按字符边界安全截断）。
fn truncate_for_log(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…（已截断）")
}

/// 归一化 base_url：用户可能填根路径（.../v4）或完整 endpoint
/// （.../v4/chat/completions），两种都要能工作，否则会拼成
/// `.../chat/completions/chat/completions` 而 404。
fn normalize_base_url(raw: &str) -> String {
    let t = raw.trim().trim_end_matches('/');
    t.strip_suffix("/chat/completions")
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| t.to_string())
}

fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let end = rest.rfind("```").unwrap_or(rest.len());
    rest[..end].trim()
}

/// AI 动作 → 意图（target 空串归一为 None）。
pub fn action_to_intent(a: &AiAction) -> Result<Intent, String> {
    let target = a
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(match a.action.as_str() {
        "approve" => Intent::Approve { target },
        "deny" => Intent::Deny { target },
        "interrupt" | "stop" => Intent::Interrupt { target },
        "read" => Intent::Read { target },
        "status" => Intent::Status,
        "send" => Intent::Send {
            target,
            text: a.text.clone().unwrap_or_default(),
        },
        other => return Err(format!("未知动作「{other}」")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── base_url 归一化 ─────────────────────────────────────────────

    #[test]
    fn test_normalize_base_url() {
        assert_eq!(
            normalize_base_url("https://open.bigmodel.cn/api/paas/v4"),
            "https://open.bigmodel.cn/api/paas/v4"
        );
        assert_eq!(
            normalize_base_url("https://open.bigmodel.cn/api/paas/v4/"),
            "https://open.bigmodel.cn/api/paas/v4"
        );
        assert_eq!(
            normalize_base_url("https://open.bigmodel.cn/api/paas/v4/chat/completions"),
            "https://open.bigmodel.cn/api/paas/v4",
            "完整 endpoint 应剥掉 /chat/completions，否则重复拼接 404"
        );
        assert_eq!(
            normalize_base_url("  https://api.test/v1/chat/completions/  "),
            "https://api.test/v1"
        );
    }

    #[test]
    fn test_invalid_not_retried() {
        // 超时错误不重试（用户已等了完整超时，再来一次是双倍痛苦）——
        // 真实 reqwest 超时难以直接构造，由集成验证；此处验证 Invalid 分类
        assert!(!AiError::Invalid("x".into()).is_transient());
    }

    // ── 提示词 ────────────────────────────────────────────────────────

    #[test]
    fn test_system_prompt_contains_schema_and_whitelist() {
        let p = AiClient::system_prompt();
        for w in ACTION_WHITELIST {
            assert!(p.contains(w), "系统提示词必须包含动作 {w}");
        }
        assert!(p.contains("confidence"));
        assert!(p.contains("learn"));
        assert!(p.contains("只输出 JSON"));
    }

    #[test]
    fn test_user_prompt_contains_snapshot_context_and_text() {
        let p = AiClient::user_prompt(
            "#1 wxherd · pi blocked pane=w8:p3",
            Some("w8:p3"),
            "帮我把卡住的放行",
        );
        assert!(p.contains("#1 wxherd · pi blocked pane=w8:p3"));
        assert!(p.contains("最近交互目标：w8:p3"));
        assert!(p.contains("帮我把卡住的放行"));

        let p2 = AiClient::user_prompt("#1 x", None, "状态");
        assert!(p2.contains("最近交互目标：无"));
    }

    // ── 响应解析 ──────────────────────────────────────────────────────

    fn sample_reply_json() -> String {
        r##"{
            "actions": [
                {"action": "approve", "target": "#1"},
                {"action": "read", "target": "#4"}
            ],
            "confidence": 0.9,
            "learn": {"keywords": ["卡住", "放行"], "template": "approve {target}", "example": "帮我把卡住的放行"}
        }"##
        .to_string()
    }

    #[test]
    fn test_parse_reply_plain() {
        let reply = AiClient::parse_reply(&sample_reply_json()).unwrap();
        assert_eq!(reply.actions.len(), 2);
        assert!((reply.confidence - 0.9).abs() < 1e-9);
        let learn = reply.learn.unwrap();
        assert_eq!(learn.template, "approve {target}");
        assert_eq!(learn.keywords, vec!["卡住".to_string(), "放行".to_string()]);
    }

    #[test]
    fn test_parse_reply_fenced_and_wrapped() {
        let fenced = format!("```json\n{}\n```", sample_reply_json());
        let reply = AiClient::parse_reply(&fenced).unwrap();
        assert_eq!(reply.actions.len(), 2);

        let wrapped = format!("好的，解析结果如下：\n{}\n以上。", sample_reply_json());
        let reply2 = AiClient::parse_reply(&wrapped).unwrap();
        assert_eq!(reply2.actions.len(), 2);
    }

    #[test]
    fn test_parse_reply_learn_optional_and_defaults() {
        let reply = AiClient::parse_reply(r##"{"actions":[],"confidence":0}"##).unwrap();
        assert!(reply.actions.is_empty());
        assert_eq!(reply.confidence, 0.0);
        assert!(reply.learn.is_none());
    }

    #[test]
    fn test_parse_reply_garbage() {
        assert!(AiClient::parse_reply("我觉得不行").is_err());
        assert!(AiClient::parse_reply("").is_err());
        assert!(AiClient::parse_reply("{\"actions\":\"不是数组\"}").is_err());
        assert!(
            AiClient::parse_reply("{\"confidence\":0.9}").is_err(),
            "缺 actions 字段"
        );
    }

    // ── 校验与门槛 ────────────────────────────────────────────────────

    #[test]
    fn test_validate_confidence_gate() {
        let reply = AiClient::parse_reply(&sample_reply_json()).unwrap();
        assert!(AiClient::validate(reply.clone(), 0.7).is_ok());
        let err = AiClient::validate(reply, 0.95).unwrap_err();
        assert!(err.to_string().contains("置信度"));

        let low = AiClient::parse_reply(r##"{"actions":[{"action":"approve"}],"confidence":0.5}"##)
            .unwrap();
        assert!(AiClient::validate(low, 0.7).is_err());
    }

    #[test]
    fn test_validate_whitelist_and_send_text() {
        let bad = AiClient::parse_reply(
            r##"{"actions":[{"action":"reboot","target":"#1"}],"confidence":0.9}"##,
        )
        .unwrap();
        let err = AiClient::validate(bad, 0.7).unwrap_err();
        assert!(err.to_string().contains("reboot"));

        let no_text = AiClient::parse_reply(
            r##"{"actions":[{"action":"send","target":"#1"}],"confidence":0.9}"##,
        )
        .unwrap();
        assert!(AiClient::validate(no_text, 0.7).is_err());

        let ok = AiClient::parse_reply(
            r##"{"actions":[{"action":"send","target":"#1","text":"把测试跑完"}],"confidence":0.9}"##,
        )
        .unwrap();
        assert!(AiClient::validate(ok, 0.7).is_ok());
    }

    // ── 动作 → 意图 ───────────────────────────────────────────────────

    #[test]
    fn test_action_to_intent() {
        let approve = AiAction {
            action: "approve".into(),
            target: Some("#1".into()),
            text: None,
        };
        assert_eq!(
            action_to_intent(&approve).unwrap(),
            Intent::Approve {
                target: Some("#1".into())
            }
        );

        let send = AiAction {
            action: "send".into(),
            target: Some("#2".into()),
            text: Some("把测试跑完".into()),
        };
        assert_eq!(
            action_to_intent(&send).unwrap(),
            Intent::Send {
                target: Some("#2".into()),
                text: "把测试跑完".into()
            }
        );

        // 空串 target 归一为 None
        let no_target = AiAction {
            action: "read".into(),
            target: Some("  ".into()),
            text: None,
        };
        assert_eq!(
            action_to_intent(&no_target).unwrap(),
            Intent::Read { target: None }
        );

        let unknown = AiAction {
            action: "format".into(),
            target: None,
            text: None,
        };
        assert!(action_to_intent(&unknown).is_err());
    }
}
