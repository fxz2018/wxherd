//! 学习闭环层（层④）：learned-rules.json 读写锁、归一化匹配、hit_count、
//! 上限淘汰、/rules 与 /forget。规则文件人工可编辑。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use super::rules;
use super::Intent;

/// 一条学习规则（learned-rules.json 单条记录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedRule {
    #[serde(default)]
    pub keywords: Vec<String>,
    pub template: String,
    pub example: String,
    #[serde(default)]
    pub hit_count: u64,
    #[serde(default)]
    pub created_at: String,
}

/// 学习规则簿：内部 RwLock 保护规则表，变更即落盘 learned-rules.json。
pub struct LearnBook {
    file: PathBuf,
    max: usize,
    rules: RwLock<Vec<LearnedRule>>,
}

impl LearnBook {
    pub fn open(file: PathBuf, max: usize) -> Self {
        let rules = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            file,
            max: max.max(1),
            rules: RwLock::new(rules),
        }
    }

    pub fn len(&self) -> usize {
        self.rules.read().unwrap().len()
    }

    /// 按 hit_count 降序快照（/rules 展示与 /forget 序号一致；平手按创建先后）。
    pub fn snapshot(&self) -> Vec<LearnedRule> {
        let mut v = self.rules.read().unwrap().clone();
        v.sort_by(|a, b| {
            b.hit_count
                .cmp(&a.hit_count)
                .then(a.created_at.cmp(&b.created_at))
        });
        v
    }

    /// 学习规则匹配（层②′）：消息归一化后与 example 相等 / keywords 全部包含 /
    /// 相似度 ≥ 阈值。命中即 hit_count += 1 并落盘；`{target}` 由目标提取器填充。
    /// 严格从宽命中宁可多调一次 AI，不冒误执行风险。
    pub fn match_text(&self, text: &str, threshold: f64) -> Option<Intent> {
        let norm = rules::normalize(text);
        if norm.is_empty() {
            return None;
        }
        for rule in self.snapshot() {
            let n_example = rules::normalize(&rule.example);
            let kws: Vec<String> = rule
                .keywords
                .iter()
                .map(|k| rules::normalize(k))
                .filter(|k| !k.is_empty())
                .collect();
            let by_example = !n_example.is_empty() && norm == n_example;
            let by_keywords = !kws.is_empty() && kws.iter().all(|k| norm.contains(k));
            let by_similarity = !n_example.is_empty() && similarity(&norm, &n_example) >= threshold;
            if !(by_example || by_keywords || by_similarity) {
                continue;
            }
            let intent = template_to_intent(&rule.template, extract_target_of(text).as_deref());
            if !intent.is_executable() {
                continue;
            }
            self.bump_hit(&rule.example);
            return Some(intent);
        }
        None
    }

    fn bump_hit(&self, example: &str) {
        {
            let mut w = self.rules.write().unwrap();
            if let Some(r) = w.iter_mut().find(|r| r.example == example) {
                r.hit_count += 1;
            }
        }
        self.save();
    }

    /// 沉淀新规则；example 归一化重复时只更新关键词与模板；超出上限按 hit_count 淘汰。
    /// 返回是否成功沉淀。
    pub fn add(&self, keywords: Vec<String>, template: String, example: String) -> bool {
        if !is_valid_template(&template) {
            return false;
        }
        let example = example.trim().to_string();
        if example.is_empty() {
            return false;
        }
        {
            let mut w = self.rules.write().unwrap();
            match w
                .iter_mut()
                .find(|r| rules::normalize(&r.example) == rules::normalize(&example))
            {
                Some(r) => {
                    r.keywords = keywords;
                    r.template = template;
                }
                None => {
                    while w.len() >= self.max {
                        let Some(idx) = w
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, r)| (r.hit_count, r.created_at.clone()))
                            .map(|(i, _)| i)
                        else {
                            break;
                        };
                        w.remove(idx);
                    }
                    w.push(LearnedRule {
                        keywords,
                        template,
                        example,
                        hit_count: 0,
                        created_at: chrono::Local::now().to_rfc3339(),
                    });
                }
            }
        }
        self.save();
        true
    }

    /// /forget：按 hit_count 降序的 1 起始序号删除，返回被删规则。
    pub fn remove(&self, index_1based: usize) -> Option<LearnedRule> {
        let snap = self.snapshot();
        let target = snap.get(index_1based.checked_sub(1)?)?;
        let removed = {
            let mut w = self.rules.write().unwrap();
            let pos = w.iter().position(|r| r.example == target.example)?;
            w.remove(pos)
        };
        self.save();
        Some(removed)
    }

    /// 规则清空（需 bridge 侧「确认」二次确认后调用），返回清除条数。
    pub fn clear(&self) -> usize {
        let n = {
            let mut w = self.rules.write().unwrap();
            let n = w.len();
            w.clear();
            n
        };
        self.save();
        n
    }

    fn save(&self) {
        let text = {
            let r = self.rules.read().unwrap();
            match serde_json::to_string_pretty(&*r) {
                Ok(t) => t,
                Err(_) => return,
            }
        };
        if let Some(parent) = self.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.file, text);
    }

    /// /rules 文案（中文硬编码）。
    pub fn format_list(&self) -> String {
        let snap = self.snapshot();
        if snap.is_empty() {
            return "📚 目前还没有已学习的规则。\nAI 意图层解析成功后会自动沉淀，也可人工编辑 learned-rules.json。".into();
        }
        let mut out = format!("📚 已学习规则（{} 条）\n\n", snap.len());
        for (i, r) in snap.iter().enumerate() {
            let kws = if r.keywords.is_empty() {
                "无关键词".to_string()
            } else {
                r.keywords.join(",")
            };
            out.push_str(&format!(
                "#{} 命中{}次 「{}」→ {}\n   例：「{}」\n",
                i + 1,
                r.hit_count,
                kws,
                r.template,
                r.example
            ));
        }
        out.push_str("\n「忘掉 序号」或 /forget 序号 删除对应规则");
        out
    }
}

fn extract_target_of(text: &str) -> Option<String> {
    rules::extract_target(text)
}

/// 字符 bigram Dice 相似度（CJK 友好）。任一方不足 2 字符时退化为全等判断。
pub fn similarity(a: &str, b: &str) -> f64 {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    if ac.is_empty() && bc.is_empty() {
        return 1.0;
    }
    if ac.len() < 2 || bc.len() < 2 {
        return if a == b { 1.0 } else { 0.0 };
    }
    let bigrams =
        |v: &[char]| -> HashSet<(char, char)> { v.windows(2).map(|w| (w[0], w[1])).collect() };
    let sa = bigrams(&ac);
    let sb = bigrams(&bc);
    let inter = sa.intersection(&sb).count();
    2.0 * inter as f64 / (sa.len() + sb.len()) as f64
}

/// 模板 → 意图：首词为动作（approve/deny/trust/interrupt/read/status/send），
/// `{target}` 占位符由目标提取器结果填充（send 模板中占位符剥离后剩余为消息文本）。
/// 动作不在白名单 → Intent::Unknown。
pub fn template_to_intent(template: &str, target: Option<&str>) -> Intent {
    let t = template.trim();
    let (action, rest) = match t.find(char::is_whitespace) {
        Some(i) => (&t[..i], t[i..].trim()),
        None => (t, ""),
    };
    let tgt = target.map(str::to_string).filter(|s| !s.trim().is_empty());
    match action.to_lowercase().as_str() {
        "approve" => Intent::Approve { target: tgt },
        "trust" => Intent::Trust { target: tgt },
        "deny" => Intent::Deny { target: tgt },
        "interrupt" | "stop" => Intent::Interrupt { target: tgt },
        "read" => Intent::Read { target: tgt },
        "status" => Intent::Status,
        "send" => {
            let text = rest.replace("{target}", "").trim().to_string();
            Intent::Send { target: tgt, text }
        }
        _ => Intent::Unknown,
    }
}

/// 模板动作白名单校验（学习沉淀前调用，防止 AI 发明新操作）。
pub fn is_valid_template(template: &str) -> bool {
    template_to_intent(template, None).is_executable()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_book(name: &str, max: usize) -> LearnBook {
        let dir =
            std::env::temp_dir().join(format!("wxherd-learn-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        LearnBook::open(dir.join("learned-rules.json"), max)
    }

    // ── 相似度 ────────────────────────────────────────────────────────

    #[test]
    fn test_similarity() {
        assert!(similarity("帮我把卡住的放行", "帮我把卡住的放行") >= 0.999);
        assert!(similarity("帮我把卡住的放行", "帮我把卡住的放行。") >= 0.9);
        assert!(similarity(" completely", "different") < 0.9);
        assert_eq!(similarity("好", "好"), 1.0);
        assert_eq!(similarity("好", "坏"), 0.0);
        assert_eq!(similarity("", ""), 1.0);
    }

    // ── 模板 → 意图 ───────────────────────────────────────────────────

    #[test]
    fn test_template_to_intent() {
        assert_eq!(
            template_to_intent("approve {target}", Some("#2")),
            Intent::Approve {
                target: Some("#2".into())
            }
        );
        assert_eq!(
            template_to_intent("approve {target}", None),
            Intent::Approve { target: None }
        );
        assert_eq!(template_to_intent("status", None), Intent::Status);
        assert_eq!(
            template_to_intent("send {target} 继续", Some("#3")),
            Intent::Send {
                target: Some("#3".into()),
                text: "继续".into()
            }
        );
        assert_eq!(
            template_to_intent("read {target}", Some("w8:p3")),
            Intent::Read {
                target: Some("w8:p3".into())
            }
        );
        // 白名单外动作 → Unknown
        assert_eq!(template_to_intent("rm -rf {target}", None), Intent::Unknown);
        assert_eq!(template_to_intent("", None), Intent::Unknown);
        assert!(!is_valid_template("restart {target}"));
        assert!(is_valid_template("interrupt {target}"));
    }

    // ── 学习规则匹配 ──────────────────────────────────────────────────

    #[test]
    fn test_learn_match_by_example_and_hit_count() {
        let book = temp_book("example", 10);
        assert!(book.add(
            vec!["卡住".into(), "放行".into()],
            "approve {target}".into(),
            "帮我把卡住的放行".into()
        ));
        // example 全等（归一化去标点）
        let intent = book.match_text("帮我把卡住的放行！", 0.9).expect("应命中");
        assert_eq!(intent, Intent::Approve { target: None });
        let snap = book.snapshot();
        assert_eq!(snap[0].hit_count, 1, "命中后 hit_count 递增");
        // 再次命中
        book.match_text("帮我把卡住的放行", 0.9);
        assert_eq!(book.snapshot()[0].hit_count, 2);
    }

    #[test]
    fn test_learn_match_by_keywords_fills_target() {
        let book = temp_book("keywords", 10);
        book.add(
            vec!["卡住".into(), "放行".into()],
            "approve {target}".into(),
            "帮我把卡住的放行".into(),
        );
        // keywords 全含即命中，{target} 由目标提取器填充
        let intent = book
            .match_text("第二个卡住了，快放行", 0.9)
            .expect("应命中");
        assert_eq!(
            intent,
            Intent::Approve {
                target: Some("#2".into())
            }
        );
        // keywords 不全含 → 不命中
        assert!(book.match_text("卡住了", 0.9).is_none());
    }

    #[test]
    fn test_learn_match_similarity_threshold() {
        let book = temp_book("sim", 10);
        book.add(vec![], "read {target}".into(), "看看第一个跑完没有".into());
        let intent = book
            .match_text("看看第一个跑完没有啊", 0.9)
            .expect("相似度足够应命中");
        assert_eq!(
            intent,
            Intent::Read {
                target: Some("#1".into())
            }
        );
        // 相似度不足 → 不命中（宁可多调 AI）
        assert!(
            book.match_text("看看第三个跑完没有啊", 0.9).is_none(),
            "相似度不足不应命中"
        );
    }

    #[test]
    fn test_learn_match_invalid_template_skipped() {
        let book = temp_book("invalid", 10);
        // 直接写盘绕过 add 校验
        let rules = vec![LearnedRule {
            keywords: vec![],
            template: "shutdown {target}".into(),
            example: "关掉一切".into(),
            hit_count: 5,
            created_at: String::new(),
        }];
        std::fs::write(
            std::env::temp_dir().join(format!(
                "wxherd-learn-test-{}-invalid/learned-rules.json",
                std::process::id()
            )),
            serde_json::to_string(&rules).unwrap(),
        )
        .unwrap();
        let book2 = LearnBook::open(
            std::env::temp_dir().join(format!(
                "wxherd-learn-test-{}-invalid/learned-rules.json",
                std::process::id()
            )),
            10,
        );
        assert!(
            book2.match_text("关掉一切", 0.9).is_none(),
            "模板非法不应命中"
        );
        let _ = book;
    }

    // ── 增删与淘汰 ────────────────────────────────────────────────────

    #[test]
    fn test_learn_add_dedupe_and_evict() {
        let book = temp_book("evict", 2);
        book.add(vec!["a".into()], "status".into(), "说法甲".into());
        book.add(vec!["b".into()], "status".into(), "说法乙".into());
        // 甲被命中一次，hit_count=1 > 乙的 0
        book.match_text("说法甲", 0.9);
        // 丙加入触发淘汰：hit_count 最低的乙被淘汰
        book.add(vec!["c".into()], "status".into(), "说法丙".into());
        let snap = book.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(
            snap.iter().all(|r| r.example != "说法乙"),
            "hit_count 最低者应被淘汰"
        );
        // example 重复 → 更新而非新增
        book.add(
            vec!["新关键词".into()],
            "read {target}".into(),
            "说法甲".into(),
        );
        assert_eq!(book.len(), 2);
        let updated = book
            .snapshot()
            .into_iter()
            .find(|r| r.example == "说法甲")
            .unwrap();
        assert_eq!(updated.keywords, vec!["新关键词".to_string()]);
        assert_eq!(updated.hit_count, 1, "更新保留 hit_count");
    }

    #[test]
    fn test_learn_remove_and_clear() {
        let book = temp_book("remove", 10);
        book.add(vec![], "status".into(), "规则一".into());
        book.add(vec![], "status".into(), "规则二".into());
        let removed = book.remove(1).expect("应删除 #1");
        assert_eq!(removed.example, "规则一");
        assert!(book.remove(5).is_none());
        assert_eq!(book.len(), 1);
        assert_eq!(book.clear(), 1);
        assert_eq!(book.len(), 0);
        // 落盘后确认为空
        assert_eq!(book.snapshot().len(), 0);
    }

    #[test]
    fn test_learn_persistence_roundtrip() {
        let book = temp_book("persist", 10);
        book.add(
            vec!["卡住".into()],
            "approve {target}".into(),
            "把卡住的放行".into(),
        );
        // 重新打开同一路径 → 规则仍在
        let path = std::env::temp_dir().join(format!(
            "wxherd-learn-test-{}-persist/learned-rules.json",
            std::process::id()
        ));
        let book2 = LearnBook::open(path, 10);
        assert_eq!(book2.len(), 1);
        assert!(book2.match_text("把卡住的放行", 0.9).is_some());
    }

    #[test]
    fn test_learn_format_list() {
        let book = temp_book("fmt", 10);
        assert!(book.format_list().contains("还没有已学习的规则"));
        book.add(
            vec!["卡住".into(), "放行".into()],
            "approve {target}".into(),
            "帮我把卡住的放行".into(),
        );
        book.match_text("帮我把卡住的放行", 0.9);
        let text = book.format_list();
        assert!(text.contains("已学习规则（1 条）"));
        assert!(text.contains("#1 命中1次 「卡住,放行」→ approve {target}"));
        assert!(text.contains("帮我把卡住的放行"));
        assert!(text.contains("忘掉"));
    }
}
