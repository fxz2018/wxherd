//! iLink 协议消息模型与各接口的请求/响应结构（serde）。
//!
//! 字段依据官方协议文档（E:/ilink-weixin-bot/docs/protocol.md）与 openclaw-weixin 的
//! types.ts 核对；所有响应字段尽量 Option 化，未知字段忽略以保证前向兼容。

use serde::{Deserialize, Serialize};

/// item_list.type 常量。
pub mod item_type {
    pub const TEXT: u32 = 1;
    pub const IMAGE: u32 = 2;
    pub const VOICE: u32 = 3;
    pub const FILE: u32 = 4;
    pub const VIDEO: u32 = 5;
    /// v0.2 预留：工具调用进度（流式）。
    #[allow(dead_code)]
    pub const TOOL_CALL_START: u32 = 11;
    /// v0.2 预留：工具调用结果（流式）。
    #[allow(dead_code)]
    pub const TOOL_CALL_RESULT: u32 = 12;
}

/// message_type：1 用户 / 2 Bot。
pub mod message_type {
    pub const USER: u32 = 1;
    pub const BOT: u32 = 2;
}

/// message_state：0 新消息 / 1 生成中 / 2 完成。
pub mod message_state {
    #[allow(dead_code)]
    pub const NEW: u32 = 0;
    /// v0.2 预留：流式生成中。
    #[allow(dead_code)]
    pub const GENERATING: u32 = 1;
    pub const DONE: u32 = 2;
}

/// 鉴权 Bot POST 请求公共元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaseInfo {
    #[serde(rename = "channel_version")]
    pub channel_version: String,
    #[serde(rename = "bot_agent")]
    pub bot_agent: String,
}

impl BaseInfo {
    pub fn new(channel_version: impl Into<String>, bot_agent: impl Into<String>) -> Self {
        Self {
            channel_version: channel_version.into(),
            bot_agent: bot_agent.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// 消息模型
// ---------------------------------------------------------------------------

/// 宽松反序列化：接受 JSON 数字或字符串，统一存成 String。
/// 服务端 message_id / seq 返回形如 "v1:2936668428851763483" 的字符串，
/// 而旧实现声明为 u64 会导致整条 getUpdates 响应解析失败、消息全丢。
fn de_opt_id<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        Num(u64),
        Str(String),
    }
    Ok(Option::<NumOrStr>::deserialize(d)?.map(|v| match v {
        NumOrStr::Num(n) => n.to_string(),
        NumOrStr::Str(s) => s,
    }))
}

/// 宽松反序列化：时间戳字段接受数字或数字字符串，非法值按缺失处理。
fn de_opt_u64<'de, D>(d: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        Num(u64),
        Str(String),
    }
    Ok(match Option::<NumOrStr>::deserialize(d)? {
        Some(NumOrStr::Num(n)) => Some(n),
        Some(NumOrStr::Str(s)) => s.trim().parse::<u64>().ok(),
        None => None,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(
        default,
        deserialize_with = "de_opt_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub seq: Option<String>,
    #[serde(
        default,
        deserialize_with = "de_opt_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "de_opt_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub create_time_ms: Option<u64>,
    #[serde(
        default,
        deserialize_with = "de_opt_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub update_time_ms: Option<u64>,
    #[serde(
        default,
        deserialize_with = "de_opt_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub delete_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_type: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_state: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_list: Option<Vec<MessageItem>>,
    /// 会话上下文令牌：回复对应会话时必须回传。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl Message {
    /// 所有 text_item 文本拼接（入站文本命令）。
    pub fn text_content(&self) -> Option<String> {
        let texts: Vec<&str> = self
            .item_list
            .as_ref()?
            .iter()
            .filter_map(|it| it.text_content())
            .collect();
        if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        }
    }

    /// 语音转写文本（voice_item.text，服务端转写）。
    pub fn voice_transcript(&self) -> Option<&str> {
        self.item_list
            .as_ref()?
            .iter()
            .find_map(|it| it.voice_transcript())
    }

    pub fn first_item(&self, item_type: u32) -> Option<&MessageItem> {
        self.item_list
            .as_ref()?
            .iter()
            .find(|it| it.item_type == item_type)
    }

    pub fn image_item(&self) -> Option<&ImageItem> {
        self.first_item(item_type::IMAGE)
            .and_then(|it| it.image_item.as_ref())
    }

    pub fn file_item(&self) -> Option<&FileItem> {
        self.first_item(item_type::FILE)
            .and_then(|it| it.file_item.as_ref())
    }

    pub fn video_item(&self) -> Option<&VideoItem> {
        self.first_item(item_type::VIDEO)
            .and_then(|it| it.video_item.as_ref())
    }

    pub fn voice_item(&self) -> Option<&VoiceItem> {
        self.first_item(item_type::VOICE)
            .and_then(|it| it.voice_item.as_ref())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageItem {
    #[serde(rename = "type", default)]
    pub item_type: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_item: Option<TextItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_item: Option<ImageItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_item: Option<VoiceItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_item: Option<FileItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_item: Option<VideoItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_start_item: Option<ToolCallItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_result_item: Option<ToolCallItem>,
    #[serde(
        default,
        deserialize_with = "de_opt_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub create_time_ms: Option<u64>,
    #[serde(
        default,
        deserialize_with = "de_opt_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub update_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_completed: Option<bool>,
    #[serde(
        default,
        deserialize_with = "de_opt_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub msg_id: Option<String>,
    /// 被引用的消息（引用消息场景，v0.1 仅透传保留）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_msg: Option<Box<Message>>,
}

impl MessageItem {
    /// 纯文本 item 构造器（出站）。
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            item_type: item_type::TEXT,
            text_item: Some(TextItem::new(text)),
            ..Default::default()
        }
    }

    pub fn text_content(&self) -> Option<&str> {
        self.text_item.as_ref().and_then(|t| t.text.as_deref())
    }

    pub fn voice_transcript(&self) -> Option<&str> {
        self.voice_item
            .as_ref()
            .and_then(|v| v.text.as_deref())
            .filter(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TextItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// 手写构造时常用 String 直接给 text；提供 From。
impl TextItem {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageItem {
    /// 入站 AES 密钥（32 位十六进制字符串），优先于 media.aes_key。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aeskey: Option<String>,
    /// 密文字节数（发送端填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<CdnMedia>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb_media: Option<CdnMedia>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VoiceItem {
    /// 服务端语音转写文本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encode_type: Option<u32>,
    /// 采样率（Hz）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u64>,
    /// 时长（毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playtime: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<CdnMedia>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// 明文字节数（十进制字符串编码）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<CdnMedia>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VideoItem {
    /// 密文字节数（发送端填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<CdnMedia>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb_media: Option<CdnMedia>,
}

/// 工具调用 item（type 11/12），v0.1 仅保留字段透传。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallItem {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// CDN 媒体引用。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CdnMedia {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypt_query_param: Option<String>,
    /// base64 编码的 AES 密钥（16 字节原始密钥或 32 位 hex 密钥串的 base64）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aes_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypt_type: Option<u32>,
    /// 服务端直出的完整下载 URL，优先使用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_url: Option<String>,
}

// ---------------------------------------------------------------------------
// getupdates
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetUpdatesRequest {
    pub get_updates_buf: String,
    pub base_info: BaseInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GetUpdatesResponse {
    #[serde(default)]
    pub ret: Option<i64>,
    #[serde(default)]
    pub errcode: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub msgs: Option<Vec<Message>>,
    #[serde(default)]
    pub get_updates_buf: Option<String>,
    /// 服务端建议的下一次长轮询超时（ms）。
    #[serde(default)]
    pub longpolling_timeout_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// sendmessage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub msg: Message,
    pub base_info: BaseInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SendMessageResponse {
    #[serde(default)]
    pub ret: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
    /// 服务端在每次成功发送后回传的新 context_token（滚动续期）。
    /// 参照腾讯官方 Octop gateway 实现：发送后立即缓存，保持推送通道常新。
    #[serde(default, rename = "contextToken")]
    pub context_token: Option<String>,
    /// 官方响应可能把额外字段放在 data 对象里（含 contextToken）。
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// getuploadurl
// ---------------------------------------------------------------------------

/// media_type：1 图片 / 2 视频 / 3 文件 / 4 语音。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Image,
    /// v0.2 预留：视频上传。
    #[allow(dead_code)]
    Video,
    File,
    /// v0.2 预留：语音上传。
    #[allow(dead_code)]
    Voice,
}

impl MediaType {
    pub fn as_u32(self) -> u32 {
        match self {
            MediaType::Image => 1,
            MediaType::Video => 2,
            MediaType::File => 3,
            MediaType::Voice => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetUploadUrlRequest {
    /// v0.2 预留：服务端生成的文件标识（当前上传走 CDN 直传路径）。
    #[allow(dead_code)]
    pub filekey: String,
    pub media_type: u32,
    pub to_user_id: String,
    pub rawsize: u64,
    pub rawfilemd5: String,
    /// AES-128-ECB + PKCS#7 填充后的密文大小。
    pub filesize: u64,
    pub no_need_thumb: bool,
    /// 十六进制形式的 AES 密钥。
    pub aeskey: String,
    pub base_info: BaseInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GetUploadUrlResponse {
    #[serde(default)]
    pub upload_param: Option<String>,
    #[serde(default)]
    pub thumb_upload_param: Option<String>,
    #[serde(default)]
    pub upload_full_url: Option<String>,
}

// ---------------------------------------------------------------------------
// getconfig / sendtyping / notify
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetConfigRequest {
    pub ilink_user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
    pub base_info: BaseInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GetConfigResponse {
    #[serde(default)]
    pub ret: Option<i64>,
    #[serde(default)]
    pub typing_ticket: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendTypingRequest {
    pub ilink_user_id: String,
    pub typing_ticket: String,
    /// 1 正在输入 / 2 取消。
    pub status: u32,
    pub base_info: BaseInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyRequest {
    pub base_info: BaseInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NotifyResponse {
    #[serde(default)]
    pub ret: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
}

// ---------------------------------------------------------------------------
// 扫码登录
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QrCodeRequest {
    #[serde(default)]
    pub local_token_list: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QrCodeResponse {
    #[serde(default)]
    pub qrcode: Option<String>,
    #[serde(default)]
    pub qrcode_img_content: Option<String>,
}

/// get_qrcode_status 的 status 字段全集。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrState {
    Wait,
    Scaned,
    Confirmed,
    Expired,
    NeedVerifyCode,
    VerifyCodeBlocked,
    ScanedButRedirect,
    BindedRedirect,
}

impl QrState {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "wait" => Self::Wait,
            "scaned" => Self::Scaned,
            "confirmed" => Self::Confirmed,
            "expired" => Self::Expired,
            "need_verifycode" => Self::NeedVerifyCode,
            "verify_code_blocked" => Self::VerifyCodeBlocked,
            "scaned_but_redirect" => Self::ScanedButRedirect,
            "binded_redirect" => Self::BindedRedirect,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QrStatusResponse {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub ilink_bot_id: Option<String>,
    #[serde(default)]
    pub baseurl: Option<String>,
    /// 扫码用户 ID（即绑定的主人）。
    #[serde(default)]
    pub ilink_user_id: Option<String>,
    /// scaned_but_redirect 时的新轮询主机。
    #[serde(default)]
    pub redirect_host: Option<String>,
}

impl QrStatusResponse {
    pub fn state(&self) -> Option<QrState> {
        self.status.as_deref().and_then(QrState::parse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_inbound_text_message() {
        let raw = r#"{
            "seq": 1,
            "message_id": 100,
            "from_user_id": "user-a",
            "to_user_id": "bot-x",
            "client_id": "c-1",
            "create_time_ms": 1710000000000,
            "session_id": "s-1",
            "message_type": 1,
            "message_state": 0,
            "context_token": "ctx-token-1",
            "item_list": [
                {"type": 1, "text_item": {"text": "/agents"}}
            ]
        }"#;
        let msg: Message = serde_json::from_str(raw).expect("文本消息必须可解析");
        assert_eq!(msg.from_user_id.as_deref(), Some("user-a"));
        assert_eq!(msg.message_type, Some(message_type::USER));
        assert_eq!(msg.context_token.as_deref(), Some("ctx-token-1"));
        assert_eq!(msg.text_content().as_deref(), Some("/agents"));
        assert!(msg.voice_transcript().is_none());
    }

    /// 回归：服务端 message_id/seq 为带 v1: 前缀的字符串，不能导致整条响应解析失败。
    #[test]
    fn test_parse_inbound_string_ids() {
        let raw = r#"{
            "seq": "v1:2936668428851763483",
            "message_id": "v1:2936668428851763484",
            "from_user_id": "user-a",
            "create_time_ms": "1710000000000",
            "message_type": 1,
            "item_list": [{"type": 1, "text_item": {"text": "你好"}}]
        }"#;
        let msg: Message = serde_json::from_str(raw).expect("字符串型 ID 必须可解析");
        assert_eq!(msg.message_id.as_deref(), Some("v1:2936668428851763484"));
        assert_eq!(msg.seq.as_deref(), Some("v1:2936668428851763483"));
        assert_eq!(msg.create_time_ms, Some(1710000000000));
        assert_eq!(msg.text_content().as_deref(), Some("你好"));
    }

    /// 回归：getUpdates 整包响应含字符串型 message_id 时必须解析成功。
    /// 回归：sendmessage 响应的 contextToken（滚动续期）必须能解析。
    #[test]
    fn test_parse_send_response_context_token() {
        let raw = r#"{"ret":0,"messageId":42,"contextToken":"tok-new-1"}"#;
        let resp: SendMessageResponse = serde_json::from_str(raw).expect("应可解析");
        assert_eq!(resp.ret, Some(0));
        assert_eq!(resp.context_token.as_deref(), Some("tok-new-1"));
        // 官方还可能放在 data.contextToken
        let raw2 = r#"{"ret":0,"data":{"contextToken":"tok-in-data"}}"#;
        let resp2: SendMessageResponse = serde_json::from_str(raw2).expect("应可解析");
        assert_eq!(
            resp2.data.and_then(|d| d
                .get("contextToken")
                .and_then(|t| t.as_str())
                .map(String::from)),
            Some("tok-in-data".to_string())
        );
    }

    #[test]
    fn test_parse_get_updates_with_string_id() {
        let raw = r#"{
            "ret": 0,
            "msgs": [
                {"seq": 1, "message_id": "v1:42", "message_type": 1,
                 "item_list": [{"type": 1, "text_item": {"text": "/agents"}}]}
            ],
            "get_updates_buf": "next-cursor"
        }"#;
        let resp: GetUpdatesResponse = serde_json::from_str(raw).expect("getUpdates 必须可解析");
        let msgs = resp.msgs.expect("应有消息");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].message_id.as_deref(), Some("v1:42"));
        assert_eq!(resp.get_updates_buf.as_deref(), Some("next-cursor"));
    }

    #[test]
    fn test_parse_inbound_voice_with_transcript() {
        let raw = r#"{
            "message_type": 1,
            "item_list": [
                {"type": 3, "voice_item": {"text": "看看第二个在干什么", "sample_rate": 16000, "playtime": 2300}, "is_completed": true}
            ]
        }"#;
        let msg: Message = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.voice_transcript(), Some("看看第二个在干什么"));
        assert!(msg.text_content().is_none());
    }

    #[test]
    fn test_parse_inbound_image_with_aeskey() {
        let raw = r#"{
            "message_type": 1,
            "item_list": [
                {
                    "type": 2,
                    "image_item": {
                        "aeskey": "00112233445566778899aabbccddeeff",
                        "mid_size": 1024,
                        "media": {
                            "encrypt_query_param": "param-x",
                            "aes_key": "AD3a2VkdDKD+TEs=",
                            "encrypt_type": 1
                        }
                    }
                }
            ]
        }"#;
        let msg: Message = serde_json::from_str(raw).unwrap();
        let img = msg.image_item().expect("应能取到 image_item");
        assert_eq!(
            img.aeskey.as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
        assert_eq!(img.mid_size, Some(1024));
        let media = img.media.as_ref().unwrap();
        assert_eq!(media.encrypt_query_param.as_deref(), Some("param-x"));
        assert_eq!(media.encrypt_type, Some(1));
    }

    #[test]
    fn test_parse_inbound_file_and_video() {
        let raw = r#"{
            "message_type": 1,
            "item_list": [
                {"type": 4, "file_item": {"file_name": "log.txt", "len": "2048",
                 "media": {"encrypt_query_param": "p", "aes_key": "AAA="}}},
                {"type": 5, "video_item": {"video_size": 4096}}
            ]
        }"#;
        let msg: Message = serde_json::from_str(raw).unwrap();
        let f = msg.file_item().unwrap();
        assert_eq!(f.file_name.as_deref(), Some("log.txt"));
        assert_eq!(f.len.as_deref(), Some("2048"));
        assert_eq!(f.media.as_ref().unwrap().aes_key.as_deref(), Some("AAA="));
        assert_eq!(msg.video_item().unwrap().video_size, Some(4096));
    }

    #[test]
    fn test_parse_multi_text_items_and_ref_msg() {
        let raw = r#"{
            "message_type": 1,
            "item_list": [
                {"type": 1, "text_item": {"text": "第一行"}},
                {"type": 1, "text_item": {"text": "第二行"}},
                {"type": 2, "ref_msg": {"item_list": [{"type": 1, "text_item": {"text": "被引用"}}]}}
            ]
        }"#;
        let msg: Message = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.text_content().as_deref(), Some("第一行\n第二行"));
        let ref_msg = msg.item_list.as_ref().unwrap()[2]
            .ref_msg
            .as_ref()
            .expect("ref_msg 应保留");
        assert_eq!(ref_msg.text_content().as_deref(), Some("被引用"));
    }

    #[test]
    fn test_parse_tool_call_items_forward_compatible() {
        let raw = r#"{
            "message_type": 2,
            "item_list": [
                {"type": 11, "tool_call_start_item": {"tool_name": "bash", "custom": 1}},
                {"type": 12, "tool_call_result_item": {"ok": true}}
            ]
        }"#;
        let msg: Message = serde_json::from_str(raw).expect("未知字段必须被宽容接受");
        let start = msg.first_item(item_type::TOOL_CALL_START).unwrap();
        assert_eq!(
            start
                .tool_call_start_item
                .as_ref()
                .unwrap()
                .extra
                .get("tool_name"),
            Some(&serde_json::json!("bash"))
        );
    }

    #[test]
    fn test_outbound_message_serialization() {
        let msg = Message {
            to_user_id: Some("user-a".to_string()),
            client_id: Some("c-2".to_string()),
            message_type: Some(message_type::BOT),
            message_state: Some(message_state::DONE),
            context_token: Some("ctx".to_string()),
            item_list: Some(vec![MessageItem::text("你好")]),
            ..Default::default()
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"text_item\":{\"text\":\"你好\"}"));
        assert!(json.contains("\"message_type\":2"));
        assert!(json.contains("\"message_state\":2"));
        assert!(
            !json.contains("from_user_id\":\"\""),
            "None 字段不应序列化出来"
        );
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn test_qr_state_parse() {
        assert_eq!(QrState::parse("wait"), Some(QrState::Wait));
        assert_eq!(QrState::parse("scaned"), Some(QrState::Scaned));
        assert_eq!(QrState::parse("confirmed"), Some(QrState::Confirmed));
        assert_eq!(QrState::parse("expired"), Some(QrState::Expired));
        assert_eq!(
            QrState::parse("need_verifycode"),
            Some(QrState::NeedVerifyCode)
        );
        assert_eq!(
            QrState::parse("verify_code_blocked"),
            Some(QrState::VerifyCodeBlocked)
        );
        assert_eq!(
            QrState::parse("scaned_but_redirect"),
            Some(QrState::ScanedButRedirect)
        );
        assert_eq!(
            QrState::parse("binded_redirect"),
            Some(QrState::BindedRedirect)
        );
        assert_eq!(QrState::parse("unknown"), None);

        let resp: QrStatusResponse =
            serde_json::from_str(r#"{"status":"confirmed","bot_token":"t"}"#).unwrap();
        assert_eq!(resp.state(), Some(QrState::Confirmed));
    }

    #[test]
    fn test_get_updates_response_parse() {
        let raw = r#"{
            "ret": 0,
            "msgs": [],
            "get_updates_buf": "cursor-2",
            "longpolling_timeout_ms": 35000
        }"#;
        let resp: GetUpdatesResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.ret, Some(0));
        assert_eq!(resp.get_updates_buf.as_deref(), Some("cursor-2"));
        assert_eq!(resp.longpolling_timeout_ms, Some(35000));
        assert!(resp.msgs.unwrap().is_empty());
    }

    #[test]
    fn test_media_type_values() {
        assert_eq!(MediaType::Image.as_u32(), 1);
        assert_eq!(MediaType::Video.as_u32(), 2);
        assert_eq!(MediaType::File.as_u32(), 3);
        assert_eq!(MediaType::Voice.as_u32(), 4);
    }
}
