//! 意图解析四级流水线：rules（内置规则层②）/ ai（AI 意图层③）/ learn（学习闭环层④）。
//! 层①精确命令解析在 bridge::parse_command；本模块只做自然语言 → 结构化意图。
//! 依赖方向：bridge → intent；intent 不依赖 bridge。

pub mod ai;
pub mod learn;
pub mod rules;

/// 结构化意图（动作白名单内，target 为 "#N" / pane ID / 项目名 / "last"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Approve {
        target: Option<String>,
    },
    Trust {
        target: Option<String>,
    },
    Deny {
        target: Option<String>,
    },
    Interrupt {
        target: Option<String>,
    },
    Read {
        target: Option<String>,
    },
    Status,
    Send {
        target: Option<String>,
        text: String,
    },
    /// 模板/动作不合法（学习规则校验用，不进入执行）。
    Unknown,
}

impl Intent {
    /// 是否为可执行意图。
    pub fn is_executable(&self) -> bool {
        !matches!(self, Intent::Unknown)
    }

    /// 是否值得直接执行：Unknown 不算；send 没有正文也不算（交回流水线后续层级）。
    pub fn is_actionable(&self) -> bool {
        match self {
            Intent::Unknown => false,
            Intent::Send { text, .. } => !text.trim().is_empty(),
            _ => true,
        }
    }

    /// 目标参数（无目标的动作返回 None）。
    pub fn target(&self) -> Option<&str> {
        match self {
            Intent::Approve { target }
            | Intent::Trust { target }
            | Intent::Deny { target }
            | Intent::Interrupt { target }
            | Intent::Read { target }
            | Intent::Send { target, .. } => target.as_deref(),
            Intent::Status | Intent::Unknown => None,
        }
    }

    /// 写入目标参数（命名目标补全用）。
    pub fn set_target(&mut self, target: Option<String>) {
        match self {
            Intent::Approve { target: t }
            | Intent::Trust { target: t }
            | Intent::Deny { target: t }
            | Intent::Interrupt { target: t }
            | Intent::Read { target: t }
            | Intent::Send { target: t, .. } => *t = target,
            Intent::Status | Intent::Unknown => {}
        }
    }
}

/// 本地两层解析：层②内置规则 → 层④学习规则（按 hit_count 降序）。
/// 层③ AI 由 bridge 异步调用（需网络与 api_key），不在本纯函数内。
pub fn parse_local(text: &str, learn: &learn::LearnBook, threshold: f64) -> Option<Intent> {
    rules::match_rules(text).or_else(|| learn.match_text(text, threshold))
}

#[cfg(test)]
mod tests {
    use super::*;
    use learn::LearnBook;

    fn temp_book(name: &str, max: usize) -> LearnBook {
        let dir = std::env::temp_dir().join(format!(
            "wxherd-pipeline-test-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        LearnBook::open(dir.join("learned-rules.json"), max)
    }

    #[test]
    fn test_pipeline_builtin_rule_hit() {
        let book = temp_book("builtin", 10);
        // 验收用例：「看看第二个在干什么」命中规则层
        assert_eq!(
            parse_local("看看第二个在干什么", &book, 0.9),
            Some(Intent::Read {
                target: Some("#2".into())
            })
        );
    }

    #[test]
    fn test_pipeline_learned_rule_second_hit() {
        let book = temp_book("learned", 10);
        // 新说法不命中内置规则（模拟之后由 AI 层沉淀）
        assert_eq!(parse_local("帮我把卡住的放行", &book, 0.9), None);
        book.add(
            vec!["卡住".into(), "放行".into()],
            "approve {target}".into(),
            "帮我把卡住的放行".into(),
        );
        // 二次同类说法直接命中学习规则，零 AI 调用
        assert_eq!(
            parse_local("帮我把卡住的放行", &book, 0.9),
            Some(Intent::Approve { target: None })
        );
        assert_eq!(book.snapshot()[0].hit_count, 1);
    }

    #[test]
    fn test_pipeline_builtin_wins_over_learned() {
        let book = temp_book("order", 10);
        // 学习规则试图把「状态」这句话教成 read，但内置规则优先
        book.add(vec![], "read {target}".into(), "状态".into());
        assert_eq!(parse_local("状态", &book, 0.9), Some(Intent::Status));
    }

    #[test]
    fn test_pipeline_unmatched_returns_none() {
        let book = temp_book("none", 10);
        assert_eq!(
            parse_local("帮我写一首诗", &book, 0.9),
            None,
            "未命中交由 AI 层"
        );
        assert_eq!(parse_local("", &book, 0.9), None);
    }
}
