//! 翻译服务抽象。
//!
//! 任务内核只依赖本模块的 `TranslatorProvider`，不依赖任何具体协议：提示词格式、
//! 批量能力与计费口径都由适配器声明，不写进调度器。首个适配器实现用户自配的
//! OpenAI 兼容 HTTP 端点（`openai`），本地模型或专用翻译服务按同一 trait 增加。

pub mod language;
mod openai;
mod prompt;

pub(crate) use prompt::fit_items;

use std::pin::Pin;
use std::time::Duration;

use crate::{Locale, ProviderCapabilities};

pub use openai::OpenAiCompatibleProvider;

/// 一批共享语言对的待翻片段。调度器保证同一批的源语言与目标语言一致，
/// 因此语言与上下文材料只在批级给一份，而不是每个片段各带一份。
#[derive(Debug, Clone, PartialEq)]
pub struct BatchRequest {
    pub source_locale: Locale,
    pub target_locale: Locale,
    /// 仅当批内所有片段的语境一致时才有值；不一致时留空，避免把某一行的语境当成全批的。
    pub context: Option<String>,
    /// 批内片段：`id` 是 1-based 序号，提示词与响应都按它对齐。
    pub items: Vec<BatchItem>,
    /// 只来自**启用**词条，且只取在某个片段里真实出现的表面形。
    pub constraints: Vec<TermConstraint>,
    /// 同语言对的项目既有译文，仅作参考。
    pub examples: Vec<MemoryExample>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchItem {
    pub id: u32,
    pub source_text: String,
}

/// 句内命中的词条约束：源文跨度上必须遵守的用词规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermConstraint {
    pub source_surface: String,
    /// `Some(译名)` 是必须使用的译名；`None` 是"保持原文不译"（不可译词，如品牌名）。
    ///
    /// 不可译词不能像禁用译法那样被丢掉：它约束的是**句中出现**的写法，
    /// 只做整格精确匹配挡不住 "欢迎来到 Wonderland" 这类句子。
    pub target_text: Option<String>,
}

/// 同语言对的既有译文示例，仅作参考，不构成可自动复用的记忆。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryExample {
    pub source_text: String,
    pub target_text: String,
}

/// 模型返回的单个片段结果；`id` 与请求里的序号一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedItem {
    pub id: u32,
    pub target_text: String,
}

/// 一次批量响应的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchResponse {
    /// 成功解析出的片段，可能少于请求数：缺哪几条由调度器按 id 判定为失败。
    pub items: Vec<TranslatedItem>,
    /// 服务未返回 token 数时为 `None`；此时只累计请求数，不猜用量。
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// 适配器错误。
///
/// 变体划分服务于两个决策：**要不要重试**与**落什么状态**。调度器直接按变体映射，
/// 不做字符串判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// 临时失败：429、5xx、超时、连接失败。可自动重试，`retry_after` 优先于默认退避。
    Transient {
        status: Option<u16>,
        retry_after: Option<Duration>,
    },
    /// 请求已发出但无法确认远端是否处理。纯机械化流程不引入人工核对：按重试规则重发，
    /// 用尽次数后记为失败。
    Indeterminate,
    /// 响应不可用：缺字段、错语言、无效 JSON、结构不符。重试无意义，落 `failed`。
    Invalid(String),
    /// 端点、模型名或密钥配置问题：界面直接提示，不重试。
    Config(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient { status, .. } => match status {
                Some(status) => write!(f, "模型服务暂时不可用（HTTP {status}）"),
                None => write!(f, "模型服务连接失败或超时"),
            },
            Self::Indeterminate => write!(f, "模型请求结果未知，远端可能已处理"),
            // 只带原因，不带响应正文：正文可能回显用户输入。
            Self::Invalid(reason) => write!(f, "模型响应不可用：{reason}"),
            Self::Config(reason) => write!(f, "模型配置无效：{reason}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// 手写 boxed future 类型别名：避免引入 `async-trait`，同时保持 trait 对象安全。
pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BatchResponse, ProviderError>> + Send + 'a>>;

pub type TransportFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, ProviderError>> + Send + 'a>>;

/// Model requests are sent through the Core host service. `secret_id` is a namespaced reference;
/// the provider never receives the key itself.
pub trait ModelTransport: Send + Sync {
    fn post_json<'a>(
        &'a self,
        base_url: &'a str,
        allow_loopback: bool,
        timeout_seconds: u32,
        secret_id: i64,
        path: &'a str,
        body: &'a str,
    ) -> TransportFuture<'a>;
}

/// 翻译服务适配器。**只按批调用**：单片段就是长度为 1 的批，提示词与解析只有一套形状。
pub trait TranslatorProvider: Send + Sync {
    /// 能力声明是**声明**而不是假设；调度器只按它决定限额与批量。
    fn capabilities(&self) -> ProviderCapabilities;

    fn translate_batch<'a>(&'a self, request: BatchRequest) -> ProviderFuture<'a>;
}

/// 单次请求允许的最大连续重试次数（不含首次）。由调度器执行，适配器只管分类。
pub const MAX_RETRIES: u32 = 2;

/// 并发上限。超过这个数只会把用户自己的额度打满并触发限流，不会更快。
pub const MAX_CONCURRENCY: u32 = 32;
