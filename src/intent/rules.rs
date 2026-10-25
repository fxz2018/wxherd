//! 内置规则匹配层（层②）：内置模式表、中文数字解析、同音错字兜底、目标提取器。
//! 纯函数实现，不依赖菜单与网络；命名目标由 bridge 用 agent 列表候选补全。

use super::Intent;

/// 同音错字兜底表（微信语音转写高频误识别 → 正确词）。
const HOMOPHONE_TABLE: &[(&str, &str)] = &[("壮态", "状态"), ("中段", "中断"), ("评比", "批准")];

/// 同音错字兜底：按词表整词替换（如「壮态」→「状态」）。
pub fn correct_homophones(input: &str) -> String {
    let mut out = input.to_string();
    for (wrong, right) in HOMOPHONE_TABLE {
        if out.contains(wrong) {
            out = out.replace(wrong, right);
        }
    }
    out
}

/// 归一化：全角→半角、小写、去空白与标点（保留字母/数字/CJK/#）。
pub fn normalize(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '\u{3000}' => ' ',
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            other => other,
        })
        .filter(|c| c.is_alphanumeric() || *c == '#')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

const CN_DIGITS: &[(char, u32)] = &[
    ('零', 0),
    ('一', 1),
    ('二', 2),
    ('两', 2),
    ('三', 3),
    ('四', 4),
    ('五', 5),
    ('六', 6),
    ('七', 7),
    ('八', 8),
    ('九', 9),
];

fn cn_digit(c: char) -> Option<u32> {
    CN_DIGITS.iter().find(|(ch, _)| *ch == c).map(|(_, n)| *n)
}

fn cn_str_value(s: &str) -> Option<u32> {
    let mut v = 0u32;
    for c in s.chars() {
        v = v * 10 + cn_digit(c)?;
    }
    Some(v)
}

/// 中文数字解析：一~十、两、十X / X十 / X十Y（≤99），兼容 ASCII 数字。
pub fn parse_chinese_number(s: &str) -> Option<u32> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(n) = t.parse::<u32>() {
        return Some(n);
    }
    let chars: Vec<char> = t.chars().collect();
    if chars.iter().any(|c| cn_digit(*c).is_none() && *c != '十') {
        return None;
    }
    if let Some(pos) = chars.iter().position(|&c| c == '十') {
        let tens: String = chars[..pos].iter().collect();
        let ones: String = chars[pos + 1..].iter().collect();
        let tens_n = if tens.is_empty() {
            1
        } else {
            cn_str_value(&tens)?
        };
        let ones_n = if ones.is_empty() {
            0
        } else {
            cn_str_value(&ones)?
        };
        if tens_n > 9 || ones_n > 9 {
            return None;
        }
        return Some(tens_n * 10 + ones_n);
    }
    cn_str_value(t)
}

/// 取连续数字段（ASCII 数字 + 中文数字 + 十），返回 (字段, 消耗字节数)。
fn take_number(s: &str) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        if c.is_ascii_digit() || cn_digit(c).is_some() || c == '十' {
            out.push(c);
            used += c.len_utf8();
        } else {
            break;
        }
    }
    (out, used)
}

/// 目标提取器：`第N个` / `第N号` / `N号` / `#N` / `首个` / `最后` / 裸数字（≤12）。
/// 返回 "#N" 或 "last"，未提取到返回 None。
pub fn extract_target(text: &str) -> Option<String> {
    let t = normalize(text);
    if t.is_empty() {
        return None;
    }
    if t.contains("首个") {
        return Some("#1".into());
    }
    if t.contains("最后") {
        return Some("last".into());
    }
    // 第X个 / 第X号 / 第X
    if let Some(pos) = t.find('第') {
        let after = &t[pos + '第'.len_utf8()..];
        let (num, _) = take_number(after);
        if let Some(n) = parse_chinese_number(&num) {
            if n > 0 {
                return Some(format!("#{n}"));
            }
        }
    }
    // #N
    if let Some(pos) = t.find('#') {
        let digits: String = t[pos + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(n) = digits.parse::<u32>() {
            if n > 0 {
                return Some(format!("#{n}"));
            }
        }
    }
    // N号
    for (i, _) in t.char_indices() {
        if t[i..].starts_with('号') {
            let num: String = t[..i]
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit() || cn_digit(*c).is_some() || *c == '十')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if let Some(n) = parse_chinese_number(&num) {
                if n > 0 {
                    return Some(format!("#{n}"));
                }
            }
        }
    }
    // 裸中文数字 + 量词（如「两个都批准」→ #2）；纯数词不命中，避免「等一下」误判
    for (i, _) in t.char_indices() {
        if t[i..].starts_with('个') {
            let num: String = t[..i]
                .chars()
                .rev()
                .take_while(|c| cn_digit(*c).is_some() || *c == '十')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if let Some(n) = parse_chinese_number(&num) {
                if (1..=12).contains(&n) {
                    return Some(format!("#{n}"));
                }
            }
        }
    }
    // 裸数字/裸中文数字（agent 菜单通常 ≤ 12 项，超出不视为目标，避免「看50行日志」误命中）
    let mut i = 0;
    while i < t.len() {
        let c = t[i..].chars().next().unwrap();
        if c.is_ascii_digit() {
            let run: String = t[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Some(n) = parse_chinese_number(&run) {
                if (1..=12).contains(&n) {
                    return Some(format!("#{n}"));
                }
            }
            i += run.len();
            continue;
        }
        i += c.len_utf8();
    }
    None
}

/// 命名目标提取：在候选（项目名/agent 名/标题）中找大小写不敏感的包含命中，取最长者。
pub fn extract_named_target(text: &str, candidates: &[&str]) -> Option<String> {
    let t = normalize(text);
    if t.is_empty() {
        return None;
    }
    let mut best: Option<(usize, String)> = None;
    for c in candidates {
        let nc = normalize(c);
        if nc.is_empty() {
            continue;
        }
        if t.contains(&nc) && best.as_ref().is_none_or(|(len, _)| nc.len() > *len) {
            best = Some((nc.len(), c.to_string()));
        }
    }
    best.map(|(_, name)| name)
}

/// send 模式：`跟/对/告诉 + [目标] + 说 + 内容`。内容取原文（保留空格）。
fn match_send(corrected: &str) -> Option<Intent> {
    let idx = corrected.find('说')?;
    let prefix = &corrected[..idx];
    // 剥离内容开头的标点（如「告诉X说,把服务重启一下」）
    let content = corrected[idx + '说'.len_utf8()..]
        .trim_start_matches(|c: char| {
            matches!(
                c,
                '，' | '。' | '、' | '！' | '？' | '；' | '：' | ',' | '.' | '!' | '?' | ';' | ':'
            )
        })
        .trim();
    if content.is_empty() {
        return None;
    }
    let np = normalize(prefix);
    let lead = (np.starts_with("跟") || np.starts_with("对") || np.starts_with("告诉"))
        && np.chars().count() >= 2;
    let marked = extract_target(&np).is_some();
    if !(lead || marked) {
        return None;
    }
    Some(Intent::Send {
        target: extract_target(&np),
        text: content.to_string(),
    })
}

/// 内置模式表（层②）。ASCII 词元（ok/y/n/stop）需独立词元匹配，避免 look→ok 之类误命中。
pub fn match_rules(text: &str) -> Option<Intent> {
    let corrected = correct_homophones(text);
    let norm = normalize(&corrected);
    if norm.is_empty() {
        return None;
    }
    if let Some(intent) = match_send(&corrected) {
        return Some(intent);
    }
    let tokens: Vec<String> = corrected
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();
    let target = extract_target(&norm);
    let has = |words: &[&str]| words.iter().any(|w| norm.contains(w));
    let token_is = |want: &[&str]| tokens.iter().any(|t| want.contains(&t.as_str()));

    if has(&["批准", "同意", "通过"]) || token_is(&["ok", "yes", "y"]) {
        return Some(Intent::Approve { target });
    }
    if has(&["拒绝", "不行", "不对"]) || token_is(&["n", "no"]) {
        return Some(Intent::Deny { target });
    }
    if has(&["中断", "停止", "停下", "打断", "别跑了"]) || token_is(&["stop"]) {
        return Some(Intent::Interrupt { target });
    }
    if has(&[
        "看看",
        "看下",
        "看一下",
        "看一眼",
        "输出",
        "日志",
        "干什么",
        "干啥",
        "进度",
        "跑啥",
        "结果",
    ]) {
        return Some(Intent::Read { target });
    }
    if has(&["状态", "情况", "怎么样"]) {
        return Some(Intent::Status);
    }
    if has(&["继续", "接着来"]) || (token_is(&["go"]) && token_is(&["on"])) {
        return Some(Intent::Send {
            target: None,
            text: "继续".into(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 中文数字 ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_chinese_number() {
        assert_eq!(parse_chinese_number("一"), Some(1));
        assert_eq!(parse_chinese_number("两"), Some(2));
        assert_eq!(parse_chinese_number("九"), Some(9));
        assert_eq!(parse_chinese_number("十"), Some(10));
        assert_eq!(parse_chinese_number("十一"), Some(11));
        assert_eq!(parse_chinese_number("二十"), Some(20));
        assert_eq!(parse_chinese_number("二十三"), Some(23));
        assert_eq!(parse_chinese_number("7"), Some(7));
        assert_eq!(parse_chinese_number(" 12 "), Some(12));
        assert_eq!(parse_chinese_number("第"), None);
        assert_eq!(parse_chinese_number(""), None);
        assert_eq!(parse_chinese_number("abc"), None);
    }

    // ── 归一化与同音错字 ──────────────────────────────────────────────

    #[test]
    fn test_normalize() {
        assert_eq!(normalize("Hello, World！"), "helloworld");
        assert_eq!(normalize("ＡＢＣ １２３"), "abc123");
        assert_eq!(normalize("　批　准　"), "批准");
        assert_eq!(normalize("看看…第二个，在干什么？"), "看看第二个在干什么");
        assert_eq!(normalize("＃2号"), "#2号");
    }

    #[test]
    fn test_correct_homophones() {
        assert_eq!(correct_homophones("壮态怎么样"), "状态怎么样");
        assert_eq!(correct_homophones("把任务中段掉"), "把任务中断掉");
        assert_eq!(correct_homophones("评比一下第2个"), "批准一下第2个");
        assert_eq!(correct_homophones("正常文本"), "正常文本");
    }

    // ── 目标提取 ──────────────────────────────────────────────────────

    #[test]
    fn test_extract_target() {
        assert_eq!(extract_target("第二个"), Some("#2".into()));
        assert_eq!(extract_target("批准第11个"), Some("#11".into()));
        assert_eq!(extract_target("看看2号"), Some("#2".into()));
        assert_eq!(extract_target("#3的状态"), Some("#3".into()));
        assert_eq!(extract_target("首个"), Some("#1".into()));
        assert_eq!(extract_target("最后一个"), Some("last".into()));
        assert_eq!(extract_target("批准 2"), Some("#2".into()));
        assert_eq!(extract_target("两个都批准"), Some("#2".into()));
        assert_eq!(extract_target("看看50行日志"), None, "大数字不是目标");
        assert_eq!(extract_target("看看输出"), None);
        assert_eq!(extract_target(""), None);
    }

    #[test]
    fn test_extract_named_target() {
        let cands = ["wxherd · pi", "FluidBar", "FluidBar2"];
        assert_eq!(
            extract_named_target("看看FluidBar的输出", &cands),
            Some("FluidBar".into())
        );
        assert_eq!(
            extract_named_target("fluidbar2 跑完没", &cands),
            Some("FluidBar2".into())
        );
        // 最长候选优先
        assert_eq!(
            extract_named_target("看看fluidbar2", &cands),
            Some("FluidBar2".into())
        );
        assert_eq!(extract_named_target("没有命中", &cands), None);
        assert_eq!(extract_named_target("", &cands), None);
    }

    // ── 规则匹配 ──────────────────────────────────────────────────────

    #[test]
    fn test_match_rules_read_with_target() {
        assert_eq!(
            match_rules("看看第二个在干什么"),
            Some(Intent::Read {
                target: Some("#2".into())
            })
        );
        assert_eq!(
            match_rules("看下2号的日志"),
            Some(Intent::Read {
                target: Some("#2".into())
            })
        );
        assert_eq!(
            match_rules("第四个跑完了吗？结果如何"),
            Some(Intent::Read {
                target: Some("#4".into())
            })
        );
        assert_eq!(match_rules("看看输出"), Some(Intent::Read { target: None }));
    }

    #[test]
    fn test_match_rules_approve_deny() {
        assert_eq!(
            match_rules("批准第2个"),
            Some(Intent::Approve {
                target: Some("#2".into())
            })
        );
        assert_eq!(match_rules("同意"), Some(Intent::Approve { target: None }));
        assert_eq!(match_rules("ok"), Some(Intent::Approve { target: None }));
        assert_eq!(
            match_rules("评比一下"),
            Some(Intent::Approve { target: None })
        );
        assert_eq!(match_rules("拒绝"), Some(Intent::Deny { target: None }));
        assert_eq!(
            match_rules("不行，不对"),
            Some(Intent::Deny { target: None })
        );
        assert_eq!(match_rules("n"), Some(Intent::Deny { target: None }));
    }

    #[test]
    fn test_match_rules_interrupt_status() {
        assert_eq!(
            match_rules("中段第3个"),
            Some(Intent::Interrupt {
                target: Some("#3".into())
            }),
            "同音错字「中段」→ 中断"
        );
        assert_eq!(
            match_rules("别跑了"),
            Some(Intent::Interrupt { target: None })
        );
        assert_eq!(
            match_rules("停下"),
            Some(Intent::Interrupt { target: None })
        );
        assert_eq!(
            match_rules("壮态如何"),
            Some(Intent::Status),
            "同音错字「壮态」→ 状态"
        );
        assert_eq!(match_rules("状态怎么样"), Some(Intent::Status));
        assert_eq!(match_rules("现在情况如何"), Some(Intent::Status));
    }

    #[test]
    fn test_match_rules_send() {
        assert_eq!(
            match_rules("跟第二个说把测试跑完"),
            Some(Intent::Send {
                target: Some("#2".into()),
                text: "把测试跑完".into()
            })
        );
        assert_eq!(
            match_rules("对FluidBar说 hello world"),
            Some(Intent::Send {
                target: None,
                text: "hello world".into()
            })
        );
        assert_eq!(
            match_rules("告诉FluidBar说,把服务重启一下"),
            Some(Intent::Send {
                target: None,
                text: "把服务重启一下".into()
            })
        );
        assert_eq!(
            match_rules("继续"),
            Some(Intent::Send {
                target: None,
                text: "继续".into()
            })
        );
        assert_eq!(
            match_rules("接着来"),
            Some(Intent::Send {
                target: None,
                text: "继续".into()
            })
        );
        // 「说」但无引导词无目标 → 不当 send
        assert_eq!(match_rules("他在说什么"), None);
        // 空内容 → 不当 send
        assert_eq!(match_rules("跟第二个说"), None);
    }

    #[test]
    fn test_match_rules_no_false_positive() {
        assert_eq!(match_rules("你好"), None);
        assert_eq!(match_rules("把测试跑完"), None);
        assert_eq!(match_rules("今天天气不错"), None);
        assert_eq!(
            match_rules("look at this"),
            None,
            "look 不能因 ok 误命中批准"
        );
        assert_eq!(match_rules(""), None);
        assert_eq!(match_rules("。！？"), None);
    }

    #[test]
    fn test_match_rules_digit_quick_style() {
        // 「N 动作」无空格变体（带空格的由层②快捷回复先接住）
        assert_eq!(
            match_rules("批准2"),
            Some(Intent::Approve {
                target: Some("#2".into())
            })
        );
        assert_eq!(
            match_rules("3中断"),
            Some(Intent::Interrupt {
                target: Some("#3".into())
            })
        );
    }
}
