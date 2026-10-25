//! iLink 协议层（微信 Bot API）：types 消息模型 / client 接口实现 / media 媒体与加密。
//!
//! 本模块只依赖 config（凭据结构），不依赖 herdr、bridge、daemon。

pub mod client;
pub mod media;
pub mod types;
