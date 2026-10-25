//! iLink 媒体层：AES-128-ECB / PKCS#7、密钥三格式编解码、CDN 上传（3 次重试）与下载解密。

use std::time::Duration;

use aes::Aes128;
use base64::Engine;
use cipher::block_padding::Pkcs7;
use cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit};
use ecb::{Decryptor, Encryptor};
use thiserror::Error;

use crate::ilink::types::CdnMedia;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("AES 密钥长度必须为 16 字节（收到 {0} 字节）")]
    /// v0.2 媒体加密路径预留（当前上传走 CDN，不在本地做 AES 加密）。
    #[allow(dead_code)]
    BadKeyLen(usize),
    #[error("无法识别的 AES 密钥格式（裸 hex / hex 的 base64 / 原始字节的 base64 之外）")]
    UnrecognizedKey,
    #[error("PKCS#7 去填充失败（密文损坏或密钥错误）")]
    Unpad,
    #[error("CDN 上传失败（重试 {attempts} 次后）：{message}")]
    UploadFailed { attempts: u32, message: String },
    #[error("CDN 上传被客户端错误终止：{0}")]
    UploadClientError(String),
    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP 错误：{0}")]
    Http(#[from] reqwest::Error),
}

pub const CDN_UPLOAD_MAX_RETRIES: u32 = 3;
pub const DEFAULT_CDN_BASE: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

// ---------------------------------------------------------------------------
// AES-128-ECB + PKCS#7
// ---------------------------------------------------------------------------

/// AES-128-ECB + PKCS#7 填充后的密文大小：ceil((n+1)/16)*16（至少补 1 字节，满块补整块）。
pub fn padded_size(plaintext_size: usize) -> usize {
    (plaintext_size + 16) / 16 * 16
}

/// AES-128-ECB 加密（PKCS#7 填充）。
pub fn encrypt_aes_ecb(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    Encryptor::<Aes128>::new(key.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

/// AES-128-ECB 解密（去 PKCS#7 填充）。
pub fn decrypt_aes_ecb(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, MediaError> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err(MediaError::Unpad);
    }
    Decryptor::<Aes128>::new(key.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| MediaError::Unpad)
}

/// 手工 PKCS#7 填充（供 pad/unpad 单元测试与校验 padded_size 一致性）。
/// 当前上传走 CDN 加密，本地加密路径 v0.2 启用前保留。
#[allow(dead_code)]
pub fn pkcs7_pad(data: &[u8], block: usize) -> Vec<u8> {
    let pad = block - (data.len() % block);
    let mut out = data.to_vec();
    out.resize(data.len() + pad, pad as u8);
    out
}

/// 手工 PKCS#7 去填充。
#[allow(dead_code)]
pub fn pkcs7_unpad(data: &[u8], block: usize) -> Result<&[u8], MediaError> {
    if data.is_empty() || !data.len().is_multiple_of(block) {
        return Err(MediaError::Unpad);
    }
    let pad = *data.last().unwrap() as usize;
    if pad == 0 || pad > block || pad > data.len() {
        return Err(MediaError::Unpad);
    }
    if data[data.len() - pad..].iter().any(|&b| b as usize != pad) {
        return Err(MediaError::Unpad);
    }
    Ok(&data[..data.len() - pad])
}

// ---------------------------------------------------------------------------
// 密钥三格式兼容（下载端）
// ---------------------------------------------------------------------------

/// 解码 AES 密钥，兼容三种形式：
/// 1. 32 位裸十六进制字符串（如 image_item.aeskey）
/// 2. 「32 位十六进制密钥串」的 base64
/// 3. 「16 字节原始密钥」的 base64
pub fn decode_aes_key(s: &str) -> Result<[u8; 16], MediaError> {
    let trimmed = s.trim();
    // 形式 1：裸 hex
    if trimmed.len() == 32 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes = hex::decode(trimmed).map_err(|_| MediaError::UnrecognizedKey)?;
        if let Ok(k) = <[u8; 16]>::try_from(bytes.as_slice()) {
            return Ok(k);
        }
    }
    // 形式 2/3：base64
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(trimmed) {
        match <[u8; 16]>::try_from(bytes.as_slice()) {
            // 形式 3：原始 16 字节
            Ok(k) => return Ok(k),
            Err(_) => {
                // 形式 2：base64 解出的是 32 位 hex 字符串
                if let Ok(text) = String::from_utf8(bytes) {
                    let text = text.trim();
                    if text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit()) {
                        if let Ok(raw) = hex::decode(text) {
                            if let Ok(k) = <[u8; 16]>::try_from(raw.as_slice()) {
                                return Ok(k);
                            }
                        }
                    }
                }
            }
        }
    }
    Err(MediaError::UnrecognizedKey)
}

/// 出站编码：16 字节密钥 → hex 字符串 → base64（与参考实现发送行为一致）。
pub fn encode_aes_key_hex_to_b64(key: &[u8; 16]) -> String {
    base64::engine::general_purpose::STANDARD.encode(hex::encode(key).as_bytes())
}

/// 从 CDN 媒体引用中解码密钥：image 场景优先 image_item.aeskey（hex），否则 media.aes_key。
/// 两者都缺失时返回 Ok(None)，下载内容按明文处理。
pub fn resolve_media_key(image_aeskey: Option<&str>, media: Option<&CdnMedia>) -> Option<[u8; 16]> {
    if let Some(k) = image_aeskey {
        if let Ok(key) = decode_aes_key(k) {
            return Some(key);
        }
    }
    let media = media?;
    let k = media.aes_key.as_deref()?;
    decode_aes_key(k).ok()
}

// ---------------------------------------------------------------------------
// CDN URL 构造
// ---------------------------------------------------------------------------

pub fn build_cdn_download_url(cdn_base_url: &str, encrypted_query_param: &str) -> String {
    format!(
        "{}/download?encrypted_query_param={}",
        cdn_base_url.trim_end_matches('/'),
        urlencoding_lite(encrypted_query_param)
    )
}

pub fn build_cdn_upload_url(cdn_base_url: &str, upload_param: &str, filekey: &str) -> String {
    format!(
        "{}/upload?encrypted_query_param={}&filekey={}",
        cdn_base_url.trim_end_matches('/'),
        urlencoding_lite(upload_param),
        urlencoding_lite(filekey)
    )
}

/// URL 编码（encodeURIComponent 语义：仅保留 RFC3986 unreserved 字符）。
fn urlencoding_lite(s: &str) -> String {
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

/// 4xx 立即终止重试，其他失败重试。
pub fn is_client_error(status: u16) -> bool {
    (400..500).contains(&status)
}

// ---------------------------------------------------------------------------
// CDN 传输（网络部分，不做单测）
// ---------------------------------------------------------------------------

/// 上传密文到 CDN，返回响应头 `x-encrypted-param`（即下载参数）。
/// 成功判定：HTTP 200 且 x-encrypted-param 非空；4xx 立即终止；其余失败最多重试 3 次。
pub async fn upload_ciphertext_to_cdn(
    http: &reqwest::Client,
    url: &str,
    ciphertext: &[u8],
) -> Result<String, MediaError> {
    let mut last_error = String::new();
    for attempt in 1..=CDN_UPLOAD_MAX_RETRIES {
        let result = try_upload_once(http, url, ciphertext).await;
        match result {
            Ok(param) => return Ok(param),
            Err(UploadAttemptError::Client(msg)) => {
                return Err(MediaError::UploadClientError(msg));
            }
            Err(UploadAttemptError::Retryable(msg)) => {
                tracing::warn!(attempt, url, "CDN 上传失败，将重试：{msg}");
                last_error = msg;
                if attempt < CDN_UPLOAD_MAX_RETRIES {
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                }
            }
        }
    }
    Err(MediaError::UploadFailed {
        attempts: CDN_UPLOAD_MAX_RETRIES,
        message: last_error,
    })
}

enum UploadAttemptError {
    Client(String),
    Retryable(String),
}

async fn try_upload_once(
    http: &reqwest::Client,
    url: &str,
    ciphertext: &[u8],
) -> Result<String, UploadAttemptError> {
    let resp = http
        .post(url)
        .header("Content-Type", "application/octet-stream")
        .body(ciphertext.to_vec())
        .send()
        .await
        .map_err(|e| UploadAttemptError::Retryable(e.to_string()))?;
    let status = resp.status().as_u16();
    let header_msg = resp
        .headers()
        .get("x-error-message")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if is_client_error(status) {
        let msg = match header_msg {
            Some(m) => m,
            None => resp.text().await.unwrap_or_default(),
        };
        return Err(UploadAttemptError::Client(format!("HTTP {status}: {msg}")));
    }
    if status != 200 {
        let msg = header_msg.unwrap_or_else(|| format!("HTTP {status}"));
        return Err(UploadAttemptError::Retryable(msg));
    }
    let param = resp
        .headers()
        .get("x-encrypted-param")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if param.is_empty() {
        return Err(UploadAttemptError::Retryable(
            "响应缺少 x-encrypted-param 头".to_string(),
        ));
    }
    Ok(param)
}

/// GET 下载媒体字节。
pub async fn download_bytes(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, MediaError> {
    let resp = http.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(MediaError::Http(resp.error_for_status().unwrap_err()));
    }
    Ok(resp.bytes().await?.to_vec())
}

/// 下载内容解密：有密钥则 AES-128-ECB 解密，无密钥按明文返回。
pub fn decrypt_media_bytes(
    ciphertext: &[u8],
    key: Option<&[u8; 16]>,
) -> Result<Vec<u8>, MediaError> {
    match key {
        Some(k) => decrypt_aes_ecb(ciphertext, k),
        None => Ok(ciphertext.to_vec()),
    }
}

/// 完整下载解密：优先 full_url，否则按 CDN base 拼接；密钥解析见 resolve_media_key。
pub async fn download_and_decrypt(
    http: &reqwest::Client,
    cdn_base_url: &str,
    image_aeskey: Option<&str>,
    media: Option<&CdnMedia>,
) -> Result<Vec<u8>, MediaError> {
    let media = media.ok_or(MediaError::UnrecognizedKey)?;
    let url = match media.full_url.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(u) => u.to_string(),
        None => {
            let param = media
                .encrypt_query_param
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .ok_or(MediaError::UnrecognizedKey)?;
            build_cdn_download_url(cdn_base_url, param)
        }
    };
    let bytes = download_bytes(http, &url).await?;
    let key = resolve_media_key(image_aeskey, Some(media));
    decrypt_media_bytes(&bytes, key.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_padded_size_matches_pkcs7() {
        for len in 0usize..=600 {
            let padded = pkcs7_pad(&vec![0xAB; len], 16);
            assert_eq!(padded.len(), padded_size(len), "len={len}");
            assert_eq!(padded.len() % 16, 0);
        }
        assert_eq!(padded_size(0), 16);
        assert_eq!(padded_size(15), 16);
        assert_eq!(padded_size(16), 32);
        assert_eq!(padded_size(255), 256);
        assert_eq!(padded_size(256), 272);
    }

    #[test]
    fn test_pkcs7_pad_unpad_roundtrip() {
        for len in 0usize..=100 {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let padded = pkcs7_pad(&data, 16);
            assert_eq!(pkcs7_unpad(&padded, 16).unwrap(), data.as_slice());
        }
    }

    #[test]
    fn test_pkcs7_unpad_rejects_bad_padding() {
        // pad 字节超块长
        let mut bad = vec![0u8; 16];
        bad[15] = 17;
        assert!(pkcs7_unpad(&bad, 16).is_err());
        // pad 字节与内容不一致
        let mut bad = vec![0u8; 16];
        bad[15] = 3;
        bad[13] = 1;
        assert!(pkcs7_unpad(&bad, 16).is_err());
        // 空输入 / 非整块
        assert!(pkcs7_unpad(&[], 16).is_err());
        assert!(pkcs7_unpad(&[1; 10], 16).is_err());
    }

    /// NIST SP 800-38A F.1.1 ECB-AES128.Encrypt 已知答案测试。
    #[test]
    fn test_aes_ecb_nist_vector() {
        let key: [u8; 16] = hex::decode("2b7e151628aed2a6abf7158809cf4f3c")
            .unwrap()
            .try_into()
            .unwrap();
        let plaintext = hex::decode("6bc1bee22e409f96e93d7e117393172a").unwrap();
        let expected = "3ad77bb40d7a3660a89ecaf32466ef97";
        let ct = encrypt_aes_ecb(&plaintext, &key);
        // PKCS#7 补一个整块
        assert_eq!(ct.len(), 32);
        assert_eq!(hex::encode(&ct[..16]), expected);
        let pt = decrypt_aes_ecb(&ct, &key).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn test_aes_ecb_roundtrip_various_lengths() {
        let key: [u8; 16] = rand::random();
        for len in [0usize, 1, 15, 16, 17, 100, 1024] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
            let ct = encrypt_aes_ecb(&data, &key);
            assert_eq!(ct.len(), padded_size(len));
            assert_ne!(ct, data, "空输入以外密文不得等于明文");
            assert_eq!(decrypt_aes_ecb(&ct, &key).unwrap(), data);
        }
    }

    #[test]
    fn test_aes_ecb_roundtrip_random_data() {
        for _ in 0..8 {
            let key: [u8; 16] = rand::random();
            let len = (rand::random::<u32>() % 4096) as usize;
            let data: Vec<u8> = (0..len).map(|_| rand::random()).collect();
            let ct = encrypt_aes_ecb(&data, &key);
            assert_eq!(decrypt_aes_ecb(&ct, &key).unwrap(), data);
        }
    }

    #[test]
    fn test_decrypt_with_wrong_key_fails() {
        let key1 = [1u8; 16];
        let key2 = [2u8; 16];
        let ct = encrypt_aes_ecb(b"hello wxherd", &key1);
        // 密钥错误时要么去填充失败，要么解出垃圾数据；这里实现必须返回错误或 Ok，
        // 但 ECB+PKCS7 语境下绝大多数会 Unpad 错误。
        match decrypt_aes_ecb(&ct, &key2) {
            Err(MediaError::Unpad) => {}
            Ok(other) => assert_ne!(other, b"hello wxherd".to_vec()),
            Err(_) => {}
        }
    }

    /// 密钥三格式：裸 hex / hex 的 base64 / 原始字节的 base64 解出同一密钥。
    #[test]
    fn test_decode_aes_key_three_formats() {
        let key: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let hex_str = hex::encode(key); // 00112233445566778899aabbccddeeff
        let b64_of_hex = base64::engine::general_purpose::STANDARD.encode(hex_str.as_bytes());
        let b64_of_raw = base64::engine::general_purpose::STANDARD.encode(key);

        assert_eq!(decode_aes_key(&hex_str).unwrap(), key, "形式 1：裸 hex");
        assert_eq!(
            decode_aes_key(&b64_of_hex).unwrap(),
            key,
            "形式 2：hex 的 base64"
        );
        assert_eq!(
            decode_aes_key(&b64_of_raw).unwrap(),
            key,
            "形式 3：原始字节的 base64"
        );
        // 非法输入
        assert!(decode_aes_key("not-a-key").is_err());
        assert!(
            decode_aes_key("0011").is_err(),
            "太短的 hex 不是 16 字节密钥"
        );
        assert!(decode_aes_key(&hex_str[..31]).is_err());
        assert!(decode_aes_key("").is_err());
    }

    #[test]
    fn test_encode_aes_key_hex_to_b64_roundtrip() {
        let key: [u8; 16] = rand::random();
        let hex_str = hex::encode(key);
        let b64 = encode_aes_key_hex_to_b64(&key);
        assert_eq!(
            b64,
            base64::engine::general_purpose::STANDARD.encode(hex_str.as_bytes()),
            "出站编码必须是对 hex 字符串做 base64"
        );
        // 与 decode 的三格式兼容闭环
        assert_eq!(decode_aes_key(&b64).unwrap(), key);
    }

    #[test]
    fn test_resolve_media_key_priority() {
        let key: [u8; 16] = [9u8; 16];
        let hex_str = hex::encode(key);
        let media = CdnMedia {
            encrypt_query_param: Some("p".to_string()),
            aes_key: Some(base64::engine::general_purpose::STANDARD.encode([1u8; 16])),
            encrypt_type: Some(1),
            full_url: None,
        };
        // image_item.aeskey 优先
        assert_eq!(
            resolve_media_key(Some(&hex_str), Some(&media)).unwrap(),
            key
        );
        // 无 image aeskey 时取 media.aes_key
        let fallback = resolve_media_key(None, Some(&media)).unwrap();
        assert_eq!(fallback, [1u8; 16]);
        // 都没有 → None（明文处理）
        assert!(resolve_media_key(None, None).is_none());
        let no_key_media = CdnMedia {
            aes_key: None,
            ..Default::default()
        };
        assert!(resolve_media_key(None, Some(&no_key_media)).is_none());
    }

    #[test]
    fn test_build_cdn_urls() {
        let base = DEFAULT_CDN_BASE;
        assert_eq!(
            build_cdn_download_url(base, "abc/def+1="),
            "https://novac2c.cdn.weixin.qq.com/c2c/download?encrypted_query_param=abc%2Fdef%2B1%3D"
        );
        assert_eq!(
            build_cdn_upload_url(base, "up param~", "file/key"),
            format!(
                "{}/upload?encrypted_query_param=up%20param~&filekey=file%2Fkey",
                base
            )
        );
        // 尾部斜杠容忍
        assert_eq!(
            build_cdn_download_url("https://x.example/c2c/", "a"),
            "https://x.example/c2c/download?encrypted_query_param=a"
        );
    }

    #[test]
    fn test_is_client_error() {
        assert!(is_client_error(400));
        assert!(is_client_error(403));
        assert!(is_client_error(499));
        assert!(!is_client_error(200));
        assert!(!is_client_error(302));
        assert!(!is_client_error(500));
        assert!(!is_client_error(503));
    }

    #[test]
    fn test_decrypt_media_bytes_no_key_is_plaintext() {
        let raw = b"\x89PNG fake bytes".to_vec();
        assert_eq!(decrypt_media_bytes(&raw, None).unwrap(), raw);
    }

    #[test]
    fn test_padded_size_edge_values() {
        // 与参考实现公式一致：ceil((n+1)/16)*16
        let expect = |n: usize| (n + 1).div_ceil(16) * 16;
        for n in [0, 1, 31, 32, 33, 1000, 65535] {
            assert_eq!(padded_size(n), expect(n));
        }
    }
}
