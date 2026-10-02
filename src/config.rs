//! 模型服务的非密钥配置与由此推导的能力声明。
//!
//! 密钥不在这里：它单独经 DPAPI 加密落盘（见 `secrets`），本模块只存"可以出现在界面和
//! 报告里"的部分。能力由本模块推导，适配器不各自再猜一遍。

use crate::{LocalePair, ProviderCapabilities};

/// 单次请求的提示词上限（system + user 的字符数）。取保守值：真实端点的窗口通常远大于它，
/// 而"按字符数而不是按 token 猜"是这里唯一能机械判定的口径。
pub const MAX_INPUT_CHARS: u32 = 8000;

/// 从提示词上限里预留给"固定部分"的字符数：指令模板 + 术语约束 + 既有译文示例。
///
/// 选批时的源文预算要减去它，否则一批的源文刚好顶满上限、再加上指令就必然超限，
/// 只能靠事后截断参考材料来收场——那等于把"装不下"的代价转嫁到约束上。
pub const PROMPT_OVERHEAD_RESERVE: usize = 2000;

/// 源文预算的下限。术语再多、预留再大，也不能把一批压到装不下一个片段：
/// 单个片段再长也必须完整送出去（最小翻译单元不截断）。
pub const MIN_SOURCE_BUDGET: usize = 500;

/// 单次请求最多打包的片段数。再多只会让"一条响应里的某一条格式坏了"牵连同批其余片段。
pub const MAX_BATCH_UNITS: u32 = 32;

/// 超时下限与上限（秒）：过短会把正常的长响应判成失败，过长会让取消迟迟不生效。
pub const MIN_TIMEOUT_SECONDS: u32 = 10;
pub const MAX_TIMEOUT_SECONDS: u32 = 600;

/// 速率上限的合法上界（次/分钟）。再高已经等价于不限制，留个可校验的范围即可。
pub const MAX_REQUESTS_PER_MINUTE: u32 = 60_000;

/// 用户填写的模型服务配置（不含密钥）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSettings {
    pub base_url: String,
    pub model: String,
    pub allow_loopback: bool,
    pub timeout_seconds: u32,
    /// 每分钟请求上限；`None` 或 `Some(0)` 表示不人为设限。
    ///
    /// 不设限不等于鼓励打满：服务端的 429 + `Retry-After` 才是真正的权威口径，
    /// 插件只负责据此退避重试。默认不替用户设一个谁也没声明的闸门。
    pub requests_per_minute: Option<u32>,
    pub price_per_million_input_tokens: Option<f64>,
    pub price_per_million_output_tokens: Option<f64>,
    pub currency: Option<String>,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            allow_loopback: false,
            timeout_seconds: 120,
            requests_per_minute: None,
            price_per_million_input_tokens: None,
            price_per_million_output_tokens: None,
            currency: None,
        }
    }
}

impl ProviderSettings {
    /// 校验插件侧的配置约束。实际模型流量仍由 Core 的受控网络能力发送。
    pub fn validate(&self) -> Result<(), String> {
        if self.base_url.trim().is_empty() {
            return Err("请填写模型服务的基础地址".into());
        }
        if self.model.trim().is_empty() {
            return Err("请填写模型名".into());
        }
        validate_endpoint(&self.base_url, self.allow_loopback)?;
        if !(MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&self.timeout_seconds) {
            return Err(format!(
                "超时需在 {MIN_TIMEOUT_SECONDS}–{MAX_TIMEOUT_SECONDS} 秒之间"
            ));
        }
        if let Some(rate) = self.requests_per_minute.filter(|value| *value > 0)
            && rate > MAX_REQUESTS_PER_MINUTE
        {
            return Err(format!(
                "速率上限不能超过 {MAX_REQUESTS_PER_MINUTE} 次/分钟"
            ));
        }
        for price in [
            self.price_per_million_input_tokens,
            self.price_per_million_output_tokens,
        ]
        .into_iter()
        .flatten()
        {
            if !price.is_finite() || price < 0.0 {
                return Err("单价必须是非负数".into());
            }
        }
        if self.currency.as_deref().is_some_and(str::is_empty) {
            return Err("货币单位不能为空字符串".into());
        }
        Ok(())
    }

    /// 能力声明。任意 OpenAI 兼容端点都接受任意语言对，因此 `supported_pairs` 为空表示
    /// 「不限制」，而不是"没有可用语言对"。
    ///
    /// 批量是**结构化对齐**的前提：提示词给每个片段编号，响应必须按编号回填，
    /// 因此 `supports_batch` 由"能否可靠对齐"决定，而不是"端点是否收多段文本"。
    pub fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supported_pairs: Vec::<LocalePair>::new(),
            supports_batch: true,
            supports_structured_output: true,
            supports_glossary_constraints: true,
            max_input_chars: Some(MAX_INPUT_CHARS),
            max_batch_units: Some(MAX_BATCH_UNITS),
            requests_per_minute: self.requests_per_minute,
            price_per_million_input_tokens: self.price_per_million_input_tokens,
            price_per_million_output_tokens: self.price_per_million_output_tokens,
            currency: self.currency.clone(),
        }
    }

    /// 配置指纹：记录建任务时的服务配置，供诊断和区分重复任务。
    ///
    /// 不含密钥；续跑只核对工作记录和输入快照，允许用户调整当前服务配置。
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}",
            self.base_url.trim(),
            self.model.trim(),
            self.allow_loopback,
            self.timeout_seconds,
            self.requests_per_minute
                .map_or("-".to_owned(), |v| v.to_string()),
            self.price_per_million_input_tokens
                .map_or("-".to_owned(), |v| v.to_string()),
            self.price_per_million_output_tokens
                .map_or("-".to_owned(), |v| v.to_string()),
            self.currency.as_deref().unwrap_or("-"),
        )
    }
}

fn validate_endpoint(base_url: &str, allow_loopback: bool) -> Result<(), String> {
    let url = url::Url::parse(base_url)
        .map_err(|error| format!("模型服务配置错误：地址无法解析：{error}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("模型服务配置错误：地址不得包含用户名或密码".into());
    }
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "模型服务配置错误：地址缺少主机名".to_owned())?;
    match url.scheme() {
        "https" => Ok(()),
        "http"
            if allow_loopback
                && matches!(
                    host.to_ascii_lowercase().as_str(),
                    "127.0.0.1" | "localhost" | "[::1]" | "::1"
                ) =>
        {
            Ok(())
        }
        "http" => Err("模型服务配置错误：仅允许 https；本机联调需显式允许回环地址".into()),
        other => Err(format!("模型服务配置错误：不支持的协议：{other}")),
    }
}
