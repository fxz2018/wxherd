//! iLink 协议客户端：鉴权头、base_info、8 个 Bot 接口、扫码登录全状态机、媒体上传编排。

use base64::Engine;
use rand::RngCore;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

use crate::config::Credentials;
use crate::ilink::media::{self, MediaError};
use crate::ilink::types::{
    item_type, message_state, message_type, BaseInfo, CdnMedia, FileItem, GetConfigRequest,
    GetConfigResponse, GetUpdatesRequest, GetUpdatesResponse, GetUploadUrlRequest,
    GetUploadUrlResponse, ImageItem, Message, MessageItem, NotifyRequest, NotifyResponse,
    QrCodeRequest, QrCodeResponse, QrStatusResponse, SendMessageRequest, SendMessageResponse,
    SendTypingRequest,
};

/// 上传的媒体信息（sendmessage 时组装媒体引用用）。
#[derive(Debug, Clone)]
pub struct UploadedMedia {
    /// CDN 文件标识（v0.2 媒体引用预留，当前 sendmessage 组装引用暂不读取）。
    #[allow(dead_code)]
    pub filekey: String,
    /// CDN 返回的下载加密参数（encrypt_query_param）。
    pub encrypt_query_param: String,
    /// 16 字节 AES 密钥的 hex 编码。
    pub aeskey_hex: String,
    pub file_size: u64,
    pub file_size_ciphertext: u64,
}

#[derive(Debug, Error)]
pub enum IlinkError {
    /// 业务码 -14：账号会话被暂停 1 小时，调用方须停止轮询静默。
    #[error("会话被暂停（-14），须静默 1 小时")]
    SessionPaused,
    #[error("业务错误 ret={code}: {message}")]
    Api { code: i64, message: String },
    #[error("HTTP 状态 {0}: {1}")]
    HttpStatus(u16, String),
    #[error("媒体错误：{0}")]
    Media(#[from] MediaError),
    #[error("响应解析失败：{0}")]
    Json(#[from] serde_json::Error),
    #[error("请求失败：{0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("扫码登录流程错误：{0}")]
    Login(String),
}

pub const QR_API_BASE: &str = "https://ilinkai.weixin.qq.com";

/// 扫码登录默认 bot_type。
pub const QR_BOT_TYPE: &str = "3";

// ---------------------------------------------------------------------------
// 扫码登录（不依赖已保存凭据）
// ---------------------------------------------------------------------------

/// 获取登录二维码。POST 不携带 Authorization 与 base_info。
pub async fn fetch_qrcode(http: &reqwest::Client) -> Result<QrCodeResponse, IlinkError> {
    let url = format!("{QR_API_BASE}/ilink/bot/get_bot_qrcode?bot_type={QR_BOT_TYPE}");
    let body = QrCodeRequest {
        local_token_list: Vec::new(),
    };
    let resp = http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("AuthorizationType", "ilink_bot_token")
        .header("X-WECHAT-UIN", random_wechat_uin())
        .header("iLink-App-Id", "bot")
        .header("iLink-App-ClientVersion", app_client_version())
        .json(&body)
        .send()
        .await?;
    parse_http(resp).await
}

/// 轮询二维码状态。GET 仅带应用头（无 AuthorizationType/Authorization/X-WECHAT-UIN）。
/// `verify_code` 为 need_verifycode 状态下用户输入的配对码。
pub async fn poll_qrcode_status(
    http: &reqwest::Client,
    base_url: &str,
    qrcode: &str,
    verify_code: Option<&str>,
) -> Result<QrStatusResponse, IlinkError> {
    let mut url = format!(
        "{}/ilink/bot/get_qrcode_status?qrcode={}",
        base_url.trim_end_matches('/'),
        urlencode(qrcode)
    );
    if let Some(code) = verify_code.filter(|c| !c.trim().is_empty()) {
        url.push_str(&format!("&verify_code={}", urlencode(code)));
    }
    let resp = http
        .get(&url)
        .header("iLink-App-Id", "bot")
        .header("iLink-App-ClientVersion", app_client_version())
        .send()
        .await?;
    parse_http(resp).await
}

// ---------------------------------------------------------------------------
// Bot 客户端
// ---------------------------------------------------------------------------

pub struct IlinkClient {
    http: reqwest::Client,
    pub credentials: Credentials,
    pub bot_agent: String,
    pub channel_version: String,
    pub cdn_base_url: String,
    /// 最近一次 context_token（滚动续期）：入站消息与每次成功发送的响应都会刷新。
    /// 参照腾讯官方 Octop gateway 的实现；None = 尚未获得（需用户先发消息）。
    context_token: tokio::sync::RwLock<Option<String>>,
}

impl IlinkClient {
    pub fn new(credentials: Credentials, bot_agent: &str) -> Result<Self, IlinkError> {
        // 不设全局超时：getUpdates 长轮询与普通请求分别按请求设置。
        let http = reqwest::Client::builder().build()?;
        Ok(Self {
            http,
            credentials,
            bot_agent: bot_agent.to_string(),
            channel_version: env!("CARGO_PKG_VERSION").to_string(),
            cdn_base_url: media::DEFAULT_CDN_BASE.to_string(),
            context_token: tokio::sync::RwLock::new(None),
        })
    }

    pub fn base_url(&self) -> &str {
        let base = self.credentials.base_url.trim();
        if base.is_empty() {
            QR_API_BASE
        } else {
            base
        }
    }

    fn base_info(&self) -> BaseInfo {
        BaseInfo::new(self.channel_version.clone(), self.bot_agent.clone())
    }

    fn app_id_header() -> (&'static str, &'static str) {
        ("iLink-App-Id", "bot")
    }

    /// 鉴权 Bot POST 公共请求。
    async fn post_json<B: serde::Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        timeout: Duration,
        body: &B,
    ) -> Result<R, IlinkError> {
        let url = format!("{}/{}", self.base_url().trim_end_matches('/'), path);
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("AuthorizationType", "ilink_bot_token")
            .header(
                "Authorization",
                format!("Bearer {}", self.credentials.bot_token),
            )
            .header(Self::app_id_header().0, Self::app_id_header().1)
            .header("iLink-App-ClientVersion", app_client_version())
            .header("X-WECHAT-UIN", random_wechat_uin())
            .timeout(timeout)
            .json(body)
            .send()
            .await?;
        parse_http(resp).await
    }

    /// getUpdates：ret/errcode 任一为 -14 → SessionPaused（1 小时静默）；其他非零 → Api 错误。
    pub async fn get_updates(
        &self,
        cursor: &str,
        long_poll_seconds: u64,
    ) -> Result<GetUpdatesResponse, IlinkError> {
        let body = GetUpdatesRequest {
            get_updates_buf: cursor.to_string(),
            base_info: self.base_info(),
        };
        // 服务端长轮询最多 long_poll_seconds；客户端超时放宽 10s 余量。
        let resp: GetUpdatesResponse = self
            .post_json(
                "ilink/bot/getupdates",
                Duration::from_secs(long_poll_seconds + 10),
                &body,
            )
            .await?;
        if resp.ret == Some(-14) || resp.errcode == Some(-14) {
            return Err(IlinkError::SessionPaused);
        }
        if let Some(code) = resp.ret.filter(|&c| c != 0) {
            return Err(IlinkError::Api {
                code,
                message: resp.errmsg.clone().unwrap_or_default(),
            });
        }
        if let Some(code) = resp.errcode.filter(|&c| c != 0) {
            return Err(IlinkError::Api {
                code,
                message: resp.errmsg.clone().unwrap_or_default(),
            });
        }
        Ok(resp)
    }

    /// sendMessage：非零 ret → Api 错误。
    /// 发送消息（官方 Octop 策略）：
    /// - token 解析：msg 里显式带的优先；否则用内部缓存（滚动续期）
    /// - 成功后把响应里的新 context_token 回存缓存（服务端每发一条就滚动签发）
    /// - -14（会话过期）/-2（认证失败）：清空缓存、不带 token 重试一次
    pub async fn send_message(&self, msg: &Message) -> Result<SendMessageResponse, IlinkError> {
        // 解析本次使用的 token：显式 > 缓存 > 无
        let explicit = msg.context_token.clone().filter(|t| !t.trim().is_empty());
        let token = match explicit {
            Some(t) => Some(t),
            None => self.context_token.read().await.clone(),
        };

        let mut attempt_msg = msg.clone();
        attempt_msg.context_token = token.clone();
        let mut resp = self.send_message_once(&attempt_msg).await;

        // -14 / -2：token 失效 → 清缓存、无 token 重试一次（官方做法）
        if let Err(IlinkError::Api { code, .. }) = &resp {
            if (*code == -14 || *code == -2) && attempt_msg.context_token.is_some() {
                *self.context_token.write().await = None;
                attempt_msg.context_token = None;
                tracing::warn!("发送被拒（ret={code}），已清除缓存 token 并重试一次");
                resp = self.send_message_once(&attempt_msg).await;
            }
        }

        let resp = resp?;
        // 滚动续期：响应带新 token 就回存
        if let Some(new_tok) = resp
            .context_token
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                resp.data
                    .as_ref()
                    .and_then(|d| d.get("contextToken"))
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
        {
            *self.context_token.write().await = Some(new_tok);
        }
        Ok(resp)
    }

    async fn send_message_once(&self, msg: &Message) -> Result<SendMessageResponse, IlinkError> {
        let body = SendMessageRequest {
            msg: msg.clone(),
            base_info: self.base_info(),
        };
        let resp: SendMessageResponse = self
            .post_json("ilink/bot/sendmessage", Duration::from_secs(15), &body)
            .await?;
        if let Some(code) = resp.ret.filter(|&c| c != 0) {
            return Err(IlinkError::Api {
                code,
                message: resp.errmsg.clone().unwrap_or_default(),
            });
        }
        Ok(resp)
    }

    /// 入站消息携带的 context_token → 刷新缓存（daemon 收消息循环调用）。
    pub async fn remember_context_token(&self, token: &str) {
        if token.trim().is_empty() {
            return;
        }
        *self.context_token.write().await = Some(token.to_string());
    }

    /// 当前缓存的 token（None = 冷启动，从未收到入站消息）。
    pub async fn current_context_token(&self) -> Option<String> {
        self.context_token.read().await.clone()
    }

    /// 发文本（message_type=2, message_state=2, 回传 context_token）。
    pub async fn send_text(
        &self,
        to_user_id: &str,
        text: &str,
        context_token: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<SendMessageResponse, IlinkError> {
        let msg = Message {
            // 参考实现恒带空串 from_user_id；省略该字段服务端报 ret=-2 prepare failed
            from_user_id: Some(String::new()),
            to_user_id: Some(to_user_id.to_string()),
            client_id: Some(uuid::Uuid::new_v4().to_string()),
            message_type: Some(message_type::BOT),
            message_state: Some(message_state::DONE),
            context_token: context_token.map(str::to_string),
            run_id: run_id.map(str::to_string),
            item_list: Some(vec![MessageItem::text(text)]),
            ..Default::default()
        };
        self.send_message(&msg).await
    }

    /// getuploadurl：要求非空 upload_full_url 或 upload_param。
    pub async fn get_upload_url(
        &self,
        req: &GetUploadUrlRequest,
    ) -> Result<GetUploadUrlResponse, IlinkError> {
        let resp: GetUploadUrlResponse = self
            .post_json("ilink/bot/getuploadurl", Duration::from_secs(15), req)
            .await?;
        let has_full = resp
            .upload_full_url
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let has_param = resp
            .upload_param
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        if !has_full && !has_param {
            return Err(IlinkError::Login(
                "getuploadurl 未返回上传地址（upload_full_url / upload_param 均为空）".to_string(),
            ));
        }
        Ok(resp)
    }

    /// getConfig：拿 typing ticket（ret=0 才接受）。
    pub async fn get_config(
        &self,
        ilink_user_id: &str,
        context_token: Option<&str>,
    ) -> Result<GetConfigResponse, IlinkError> {
        let body = GetConfigRequest {
            ilink_user_id: ilink_user_id.to_string(),
            context_token: context_token.map(str::to_string),
            base_info: self.base_info(),
        };
        let resp: GetConfigResponse = self
            .post_json("ilink/bot/getconfig", Duration::from_secs(10), &body)
            .await?;
        if resp.ret != Some(0) {
            return Err(IlinkError::Api {
                code: resp.ret.unwrap_or(-1),
                message: "getconfig 失败".to_string(),
            });
        }
        Ok(resp)
    }

    /// sendTyping：status=1 输入中 / 2 取消。
    pub async fn send_typing(
        &self,
        ilink_user_id: &str,
        typing_ticket: &str,
        status: u32,
    ) -> Result<(), IlinkError> {
        let body = SendTypingRequest {
            ilink_user_id: ilink_user_id.to_string(),
            typing_ticket: typing_ticket.to_string(),
            status,
            base_info: self.base_info(),
        };
        let _: serde_json::Value = self
            .post_json("ilink/bot/sendtyping", Duration::from_secs(10), &body)
            .await?;
        Ok(())
    }

    pub async fn notify_start(&self) -> Result<NotifyResponse, IlinkError> {
        self.notify("ilink/bot/msg/notifystart").await
    }

    pub async fn notify_stop(&self) -> Result<NotifyResponse, IlinkError> {
        self.notify("ilink/bot/msg/notifystop").await
    }

    async fn notify(&self, path: &str) -> Result<NotifyResponse, IlinkError> {
        let body = NotifyRequest {
            base_info: self.base_info(),
        };
        self.post_json(path, Duration::from_secs(10), &body).await
    }

    // -----------------------------------------------------------------------
    // 媒体上传编排：读文件 → md5 → 密钥 → getuploadurl → CDN 加密上传
    // -----------------------------------------------------------------------

    pub async fn upload_media(
        &self,
        path: &Path,
        media_type: crate::ilink::types::MediaType,
        to_user_id: &str,
    ) -> Result<UploadedMedia, IlinkError> {
        let plaintext = std::fs::read(path)?;
        let rawsize = plaintext.len() as u64;
        let rawfilemd5 = md5_hex(&plaintext);
        let filesize = media::padded_size(plaintext.len()) as u64;
        let mut key = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut key);
        let aeskey_hex = hex::encode(key);
        let filekey = hex::encode({
            let mut fk = [0u8; 16];
            rand::rngs::OsRng.fill_bytes(&mut fk);
            fk
        });

        let req = GetUploadUrlRequest {
            filekey: filekey.clone(),
            media_type: media_type.as_u32(),
            to_user_id: to_user_id.to_string(),
            rawsize,
            rawfilemd5,
            filesize,
            no_need_thumb: true,
            aeskey: aeskey_hex.clone(),
            base_info: self.base_info(),
        };
        let resp = self.get_upload_url(&req).await?;

        let upload_url = match resp
            .upload_full_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(u) => u.to_string(),
            None => media::build_cdn_upload_url(
                &self.cdn_base_url,
                resp.upload_param.as_deref().unwrap_or_default(),
                &filekey,
            ),
        };
        let ciphertext = media::encrypt_aes_ecb(&plaintext, &key);
        let download_param =
            media::upload_ciphertext_to_cdn(&self.http, &upload_url, &ciphertext).await?;

        Ok(UploadedMedia {
            filekey,
            encrypt_query_param: download_param,
            aeskey_hex,
            file_size: rawsize,
            file_size_ciphertext: filesize,
        })
    }

    /// 发图片（说明文字拆成独立文本请求先行，与参考实现行为一致）。
    pub async fn send_image(
        &self,
        to_user_id: &str,
        path: &Path,
        caption: Option<&str>,
        context_token: Option<&str>,
    ) -> Result<SendMessageResponse, IlinkError> {
        if let Some(cap) = caption.map(str::trim).filter(|s| !s.is_empty()) {
            self.send_text(to_user_id, cap, context_token, None).await?;
        }
        let uploaded = self
            .upload_media(path, crate::ilink::types::MediaType::Image, to_user_id)
            .await?;
        let item = MessageItem {
            item_type: item_type::IMAGE,
            image_item: Some(ImageItem {
                media: Some(CdnMedia {
                    encrypt_query_param: Some(uploaded.encrypt_query_param),
                    aes_key: Some(media::encode_aes_key_hex_to_b64(&decode_key_hex(
                        &uploaded.aeskey_hex,
                    )?)),
                    encrypt_type: Some(1),
                    full_url: None,
                }),
                mid_size: Some(uploaded.file_size_ciphertext),
                ..Default::default()
            }),
            ..Default::default()
        };
        let msg = Message {
            from_user_id: Some(String::new()),
            to_user_id: Some(to_user_id.to_string()),
            client_id: Some(uuid::Uuid::new_v4().to_string()),
            message_type: Some(message_type::BOT),
            message_state: Some(message_state::DONE),
            context_token: context_token.map(str::to_string),
            item_list: Some(vec![item]),
            ..Default::default()
        };
        self.send_message(&msg).await
    }

    /// 发文件：getuploadurl(MediaType::File) → CDN 加密上传 → file_item 消息。
    pub async fn send_file(
        &self,
        to_user_id: &str,
        path: &Path,
        caption: Option<&str>,
        context_token: Option<&str>,
    ) -> Result<SendMessageResponse, IlinkError> {
        if let Some(cap) = caption.map(str::trim).filter(|s| !s.is_empty()) {
            self.send_text(to_user_id, cap, context_token, None).await?;
        }
        let uploaded = self
            .upload_media(path, crate::ilink::types::MediaType::File, to_user_id)
            .await?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        let item = MessageItem {
            item_type: item_type::FILE,
            file_item: Some(FileItem {
                file_name: Some(file_name),
                len: Some(uploaded.file_size.to_string()),
                media: Some(CdnMedia {
                    encrypt_query_param: Some(uploaded.encrypt_query_param),
                    aes_key: Some(media::encode_aes_key_hex_to_b64(&decode_key_hex(
                        &uploaded.aeskey_hex,
                    )?)),
                    encrypt_type: Some(1),
                    full_url: None,
                }),
            }),
            ..Default::default()
        };
        let msg = Message {
            from_user_id: Some(String::new()),
            to_user_id: Some(to_user_id.to_string()),
            client_id: Some(uuid::Uuid::new_v4().to_string()),
            message_type: Some(message_type::BOT),
            message_state: Some(message_state::DONE),
            context_token: context_token.map(str::to_string),
            item_list: Some(vec![item]),
            ..Default::default()
        };
        self.send_message(&msg).await
    }

    /// 下载并解密入站媒体（full_url 优先；密钥三格式兼容，见 media::resolve_media_key）。
    pub async fn download_media(
        &self,
        image_aeskey: Option<&str>,
        media: Option<&CdnMedia>,
    ) -> Result<Vec<u8>, MediaError> {
        media::download_and_decrypt(&self.http, &self.cdn_base_url, image_aeskey, media).await
    }
}

fn decode_key_hex(hex_key: &str) -> Result<[u8; 16], IlinkError> {
    let bytes =
        hex::decode(hex_key).map_err(|_| IlinkError::Login("AES 密钥 hex 解码失败".into()))?;
    <[u8; 16]>::try_from(bytes.as_slice())
        .map_err(|_| IlinkError::Login("AES 密钥长度不是 16 字节".into()))
}

/// 统一 HTTP 响应处理：非 2xx → HttpStatus 错误；JSON 解码。
async fn parse_http<R: DeserializeOwned>(resp: reqwest::Response) -> Result<R, IlinkError> {
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(IlinkError::HttpStatus(
            status.as_u16(),
            String::from_utf8_lossy(&bytes).into_owned(),
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// X-WECHAT-UIN：随机 uint32 的十进制字符串再 base64。
pub fn random_wechat_uin() -> String {
    let n = rand::random::<u32>();
    base64::engine::general_purpose::STANDARD.encode(n.to_string().as_bytes())
}

/// iLink-App-ClientVersion：0x00MMNNPP 编码（major<<16 | minor<<8 | patch）的十进制字符串。
pub fn app_client_version() -> String {
    let parts: Vec<u32> = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let major = parts.first().copied().unwrap_or(0) & 0xff;
    let minor = parts.get(1).copied().unwrap_or(0) & 0xff;
    let patch = parts.get(2).copied().unwrap_or(0) & 0xff;
    let code = (major << 16) | (minor << 8) | patch;
    code.to_string()
}

pub fn md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_wechat_uin_format() {
        let uin = random_wechat_uin();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&uin)
            .unwrap();
        let text = String::from_utf8(decoded).unwrap();
        let n: u32 = text.parse().expect("解码后必须是十进制数字串");
        let _ = n; // 随机值，只需可解析
    }

    #[test]
    fn test_app_client_version_encoding() {
        // wxherd 版本 0.1.0 → (0<<16)|(1<<8)|0 = 256
        assert_eq!(app_client_version(), "256");
    }

    #[test]
    fn test_md5_hex() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn test_session_paused_detection() {
        // -14 判定逻辑借由响应结构直接验证
        let raw = r#"{"ret": -14, "errmsg": "paused"}"#;
        let resp: GetUpdatesResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.ret, Some(-14));

        let raw = r#"{"ret": 0, "errcode": -14}"#;
        let resp: GetUpdatesResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.errcode, Some(-14));
    }

    #[test]
    fn test_upload_url_construction_from_param() {
        // 无 upload_full_url 时按 upload_param + filekey 拼 CDN 上传 URL
        let url =
            media::build_cdn_upload_url(media::DEFAULT_CDN_BASE, "param-value", "filekey-value");
        assert_eq!(
            url,
            "https://novac2c.cdn.weixin.qq.com/c2c/upload?encrypted_query_param=param-value&filekey=filekey-value"
        );
    }
}
