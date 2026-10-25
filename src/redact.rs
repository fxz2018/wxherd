//! 日志脱敏：token / 密钥 / 手机号在写日志与展示输出前打码（M5）。
//!
//! 原则：掩码结果保留可辨识度（末 4 位）以便对账，其余以 `*` 代替。

/// 掩码单个密钥：保留末 4 位，其余以 `****` 代替；长度 ≤4 全部打码。
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 4 {
        "****".to_string()
    } else {
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("****{tail}")
    }
}

/// 通用日志脱敏：手机号、Bearer token、长十六进制串（≥16 位，视为 token/密钥）。
pub fn redact_text(text: &str) -> String {
    let s = redact_phones(text);
    let s = mask_bearer(&s);
    mask_hex_runs(&s, 16)
}

/// 中国大陆手机号（1[3-9] 起头的 11 位独立数字段）打码为 `138****5678` 形式。
fn redact_phones(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let is_phone = chars[i] == '1'
            && i + 11 <= chars.len()
            && (i == 0 || !chars[i - 1].is_ascii_digit())
            && (i + 11 == chars.len() || !chars[i + 11].is_ascii_digit())
            && matches!(chars[i + 1], '3'..='9')
            && chars[i + 2..i + 11].iter().all(|c| c.is_ascii_digit());
        if is_phone {
            let phone: String = chars[i..i + 11].iter().collect();
            out.push_str(&phone[..3]);
            out.push_str("****");
            out.push_str(&phone[7..]);
            i += 11;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// `Bearer <token>`（大小写不敏感）中的 token 打码；
/// 「Bearer」必须后随空白（避免误伤 Bearers 之类的普通单词）。
fn mask_bearer(text: &str) -> String {
    let lower = text.to_lowercase();
    match lower.find("bearer") {
        Some(pos) => {
            let after_keyword = &text[pos + "bearer".len()..];
            if !after_keyword.starts_with(char::is_whitespace) {
                // 不是「Bearer + 空白 + token」形态：原样返回
                return text.to_string();
            }
            let head = &text[..pos + "bearer".len()];
            let after = after_keyword;
            let skipped = after.len() - after.trim_start().len();
            let token: String = after
                .chars()
                .skip_while(|c| c.is_whitespace())
                .take_while(|c| !c.is_whitespace())
                .collect();
            if token.is_empty() {
                // "Bearer" 后无 token：原样返回，避免误删后续文本
                text.to_string()
            } else {
                let mut out = String::with_capacity(text.len());
                out.push_str(head);
                out.push_str(&after[..skipped]);
                out.push_str(&mask_secret(&token));
                out.push_str(&after[skipped + token.len()..]);
                out
            }
        }
        None => text.to_string(),
    }
}

/// 长度 ≥ min_len 的连续十六进制串打码（token/密钥特征）。
fn mask_hex_runs(text: &str, min_len: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
        } else {
            flush_hex_run(&mut out, &mut run, min_len);
            out.push(c);
        }
    }
    flush_hex_run(&mut out, &mut run, min_len);
    out
}

fn flush_hex_run(out: &mut String, run: &mut String, min_len: usize) {
    if run.chars().count() >= min_len {
        out.push_str(&mask_secret(run));
    } else {
        out.push_str(run);
    }
    run.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_secret() {
        assert_eq!(mask_secret("abcdefgh"), "****efgh");
        assert_eq!(mask_secret("12345"), "****2345");
        assert_eq!(mask_secret("abc"), "****");
        assert_eq!(mask_secret(""), "****");
        // 多字节字符按字符数而非字节数：中文末 4 位完整保留
        assert_eq!(mask_secret("密钥abcdef"), "****cdef");
        assert_eq!(mask_secret("ab中文"), "****");
    }

    #[test]
    fn test_redact_phone() {
        // 运行时拼接，避免源码里出现完整手机号字面量
        let phone = format!("13{}{}", "8", "12345678");
        let text = format!("联系电话 {phone} 收到请回复");
        assert_eq!(redact_text(&text), "联系电话 138****5678 收到请回复");
        // 非手机号：12 位数字、1 开头非 3-9 段、被数字包裹
        assert_eq!(redact_text("123456789012"), "123456789012");
        assert_eq!(redact_text("12345678901"), "12345678901");
        assert_eq!(redact_text("113812345678"), "113812345678");
        assert_eq!(redact_text("无手机号文本"), "无手机号文本");
    }

    #[test]
    fn test_redact_bearer() {
        assert_eq!(
            redact_text("Authorization: Bearer abcdefgh1234"),
            "Authorization: Bearer ****1234"
        );
        assert_eq!(redact_text("bearer sk-abcdefgh"), "bearer ****efgh");
        // Bearer 后无 token：原样返回
        assert_eq!(redact_text("token: Bearer "), "token: Bearer ");
        assert_eq!(redact_text("Bearers"), "Bearers");
    }

    #[test]
    fn test_redact_hex_run() {
        // ≥16 位十六进制串 → 打码（运行时拼接避免源码密钥字面量被清洗）
        let secret = format!("{}{}", "fdecba", "9876543210");
        let text = format!("key={secret};");
        assert_eq!(redact_text(&text), "key=****3210;");
        // 短十六进制串保留
        assert_eq!(redact_text("id=deadbeef"), "id=deadbeef");
        // 时间戳（10 位数字）不受影响
        assert_eq!(redact_text("ts=1761234567"), "ts=1761234567");
    }

    #[test]
    fn test_redact_plain_text_untouched() {
        assert_eq!(
            redact_text("守护进程启动（pid 1234）"),
            "守护进程启动（pid 1234）"
        );
        assert_eq!(redact_text(""), "");
        assert_eq!(
            redact_text("E:\\wxherd\\target\\release\\wxherd.exe"),
            "E:\\wxherd\\target\\release\\wxherd.exe"
        );
    }

    #[test]
    fn test_redact_combined() {
        let secret = format!("{}{}", "fdecba", "9876543210");
        let phone = format!("15{}{}", "9", "88884321");
        let log = format!("token=Bearer {secret} phone={phone} done");
        let out = redact_text(&log);
        assert!(out.contains("****3210"), "{out}");
        assert!(out.contains("159****4321"), "{out}");
        assert!(!out.contains(&secret), "{out}");
        assert!(!out.contains(&phone), "{out}");
    }
}
