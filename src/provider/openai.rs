//! 首个适配器：用户自配的 OpenAI 兼容 `chat/completions` 端点。
//!
//! 只依赖配置里的基础地址与模型名，不预置任何厂商、不按服务名猜能力。请求固定发往
//! `{base_url}/chat/completions`，密钥引用与 HTTP 鉴权由 Core 服务处理；响应必须是结构化 JSON，
//! 且按片段序号回填译文。

use std::sync::Arc;

use crate::ProviderCapabilities;
use crate::config::{MAX_INPUT_CHARS, ProviderSettings};
use crate::provider::prompt::{build_prompt, params_of, parse_translations, parse_usage};
use crate::provider::{
    BatchRequest, BatchResponse, ProviderError, ProviderFuture, TranslatedItem, TranslatorProvider,
    ModelTransport,
};

/// OpenAI 兼容适配器。
pub struct OpenAiCompatibleProvider {
    transport: Arc<dyn ModelTransport>,
    secret_id: i64,
    settings: ProviderSettings,
    model: String,
    capabilities: ProviderCapabilities,
}

impl OpenAiCompatibleProvider {
    pub fn new(settings: &ProviderSettings, secret_id: i64, transport: Arc<dyn ModelTransport>) -> Result<Self, ProviderError> {
        Ok(Self {
            transport,
            secret_id,
            settings: settings.clone(),
            model: settings.model.trim().to_owned(),
            capabilities: settings.capabilities(),
        })
    }

    async fn call(&self, request: BatchRequest) -> Result<BatchResponse, ProviderError> {
        if request.items.is_empty() {
            return Err(ProviderError::Invalid("批量请求没有片段".into()));
        }
        // 回显校验用的目标语言；调度器保证同批语言一致。
        let target = request.target_locale.tag.clone();
        let max_chars = self.capabilities.max_input_chars.unwrap_or(MAX_INPUT_CHARS) as usize;
        let (system, user) = build_prompt(params_of(&request, max_chars));
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        })
        .to_string();

        let response = self
            .transport
            .post_json(
                &self.settings.base_url,
                self.settings.allow_loopback,
                self.settings.timeout_seconds,
                self.secret_id,
                "/chat/completions",
                &body,
            )
            .await?;

        // 回填前先校验结构与语言，而不是"拿到什么就写什么"。
        let items = parse_translations(&response, &target).map_err(|reason| {
            // 解析失败时，日志里唯一能定位问题的就是模型到底回了什么：把响应正文
            // 截断后一并报出。成功路径不这么干——译文只经 job 日志的"完成"行。
            ProviderError::Invalid(format!(
                "{reason}；模型返回：{}",
                response_excerpt(&response)
            ))
        })?;
        let (input_tokens, output_tokens) = parse_usage(&response);
        Ok(BatchResponse {
            items: items
                .into_iter()
                .map(|(id, target_text)| TranslatedItem { id, target_text })
                .collect(),
            input_tokens,
            output_tokens,
        })
    }
}

/// 响应正文的可见摘要：截断到固定长度，并把换行压成空格，保证错误信息在日志里是一行。
fn response_excerpt(body: &[u8]) -> String {
    const LIMIT: usize = 400;
    let text = String::from_utf8_lossy(body);
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= LIMIT {
        return flattened;
    }
    let head: String = flattened.chars().take(LIMIT).collect();
    format!("{head}…")
}

impl TranslatorProvider for OpenAiCompatibleProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn translate_batch<'a>(&'a self, request: BatchRequest) -> ProviderFuture<'a> {
        Box::pin(self.call(request))
    }
}
