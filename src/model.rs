//! 翻译插件 JSON 契约。语言标识使用 BCP 47 字符串，CSV 列名只存在于映射层。

use serde::{Deserialize, Serialize};

/// BCP 47 标签，例如 `zh-Hans`、`en`、`pt-BR`；适配器负责校验与服务映射。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct Locale {
    pub tag: String,
}

/// 输入文件的稳定身份；hash 是原始字节的 SHA-256 十六进制值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct SourceVersion {
    pub sha256: String,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub size_bytes: u64,
}

/// CSV 表头到语义字段的显式映射。索引从零开始，未知列不出现在这里。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct ColumnMapping {
    pub source_text_column: usize,
    pub source_locale: Locale,
    pub selection_column: Option<usize>,
    pub context_column: Option<usize>,
    pub resource_key_column: Option<usize>,
    pub target_columns: Vec<TargetColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct TargetColumn {
    pub column: usize,
    pub locale: Locale,
}

/// 一个 CSV 源格。row_number 是含表头的 1-based 物理记录序号；换行单元格仍是一条记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct Segment {
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub row_number: u64,
    pub source_text: String,
    pub source_locale: Locale,
    pub context: Option<String>,
    pub resource_key: Option<String>,
    pub source_version: SourceVersion,
}

/// 占位签名是解析后保护片段的类型、身份、出现次数及结构的规范表示；不是原文子串。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct PlaceholderSignature {
    pub schema_version: u32,
    pub canonical: String,
}

/// 同一源文在不同上下文、资源 key 或占位签名下不可直接合并。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct TranslationUnit {
    pub segment: Segment,
    pub target_locale: Locale,
    pub target_column: usize,
    pub placeholder_signature: PlaceholderSignature,
    pub state: CellState,
    pub target_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub enum CellState {
    Empty,
    Existing,
    MemoryReused,
    GlossaryExact,
    MachineDraft,
    HumanEdited,
    Approved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub enum IssueSeverity {
    Blocking,
    Review,
}

/// 问题位置定位到原始记录与目标语言；code 供 UI 和报告稳定识别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct QualityIssue {
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub row_number: u64,
    pub target_locale: Locale,
    pub code: String,
    pub severity: IssueSeverity,
    pub details: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub enum JobStatus {
    Queued,
    Running,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
}

/// 服务能力是声明而非假设。价格单位由 provider 提供，None 表示无法估价。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct ProviderCapabilities {
    pub supported_pairs: Vec<LocalePair>,
    pub supports_batch: bool,
    pub supports_structured_output: bool,
    pub supports_glossary_constraints: bool,
    pub max_input_chars: Option<u32>,
    pub max_batch_units: Option<u32>,
    pub requests_per_minute: Option<u32>,
    pub price_per_million_input_tokens: Option<f64>,
    pub price_per_million_output_tokens: Option<f64>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct LocalePair {
    pub source: Locale,
    pub target: Locale,
}

/// 文件选择后的表头与默认映射；前端可修改映射后再提交预检。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct CsvInspection {
    pub file_name: String,
    pub headers: Vec<String>,
    pub has_bom: bool,
    pub source_detected: bool,
    pub default_mapping: ColumnMapping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct TargetSummary {
    pub locale: Locale,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub pending: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub existing: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub memory_reused: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub glossary_exact: u64,
}

/// `issue_preview` 最多 100 条；完整逐格问题写入导出报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct Preflight {
    pub input_name: String,
    pub source_version: SourceVersion,
    pub mapping: ColumnMapping,
    pub has_bom: bool,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub total_rows: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub selected_rows: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub pending_cells: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub existing_cells: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub memory_reused_cells: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub glossary_exact_cells: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub unique_units: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub empty_source_rows: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub format_rows: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub newline_rows: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub carriage_return_rows: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub issue_count: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub term_hit_count: u64,
    pub term_hit_preview: Vec<TermHit>,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub memory_version: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub glossary_version: u64,
    pub issue_preview: Vec<QualityIssue>,
    pub targets: Vec<TargetSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct TranslationExport {
    pub directory: String,
    pub csv_path: String,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct MemoryEntry {
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub id: i64,
    pub source_locale: Locale,
    pub target_locale: Locale,
    pub source_text: String,
    pub target_text: String,
    pub context: String,
    pub resource_key: String,
    pub placeholder_signature: String,
    pub source_kind: String,
    pub enabled: bool,
    pub input_hash: Option<String>,
    #[cfg_attr(feature = "bindings", ts(type = "number | null"))]
    pub row_number: Option<u64>,
    pub qa_blocking: bool,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub enum TermKind {
    Ordinary,
    DoNotTranslate,
    Forbidden,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct GlossaryTerm {
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub id: i64,
    pub kind: TermKind,
    pub source_locale: Locale,
    pub target_locale: Locale,
    pub source_text: String,
    pub target_text: String,
    pub aliases: Vec<String>,
    pub context: Option<String>,
    pub resource_key: Option<String>,
    pub disambiguation: String,
    pub enabled: bool,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct TermInput {
    #[cfg_attr(feature = "bindings", ts(type = "number | null"))]
    pub id: Option<i64>,
    pub kind: TermKind,
    pub source_locale: Locale,
    pub target_locale: Locale,
    pub source_text: String,
    pub target_text: String,
    pub aliases: Vec<String>,
    pub context: Option<String>,
    pub resource_key: Option<String>,
    pub disambiguation: String,
    #[cfg_attr(feature = "bindings", ts(type = "number | null"))]
    pub expected_version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct TermHit {
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub row_number: u64,
    pub target_locale: Locale,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub term_id: i64,
    pub kind: TermKind,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub start_char: usize,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub end_char: usize,
    pub source_surface: String,
    pub target_text: String,
}

/// 模型服务的配置视图。密钥只以 `has_key` 出现，明文不出 Rust 侧。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub remark: String,
    pub allow_loopback: bool,
    pub timeout_seconds: u32,
    /// 每分钟请求上限；`None` 或 `0` 表示不人为设限。
    pub requests_per_minute: Option<u32>,
    pub price_per_million_input_tokens: Option<f64>,
    pub price_per_million_output_tokens: Option<f64>,
    pub currency: Option<String>,
    pub has_key: bool,
}

/// 写入模型服务配置；`api_key` 为 `None` 表示保持既有密钥。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct ProviderConfigInput {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub remark: String,
    pub allow_loopback: bool,
    pub timeout_seconds: u32,
    /// 每分钟请求上限；`None` 或 `0` 表示不人为设限。
    pub requests_per_minute: Option<u32>,
    pub price_per_million_input_tokens: Option<f64>,
    pub price_per_million_output_tokens: Option<f64>,
    pub currency: Option<String>,
    /// `None` 保持既有密钥；`Some("")` 清除；`Some(非空)` 覆盖。
    pub api_key: Option<String>,
}

/// 用户确认后锁定的作业上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct JobLimits {
    pub concurrency: u32,
    /// 单次请求打包的片段数；较大批次能显著减少请求数与提示词重复。
    pub batch_size: u32,
    #[serde(default)]
    pub include_false_rows: bool,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub input_token_budget: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub output_token_budget: u64,
}

/// 作业进度快照。计数一律是累计值，界面不需要自己相加。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct JobProgress {
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub job_id: i64,
    pub status: JobStatus,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub total: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub done: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub failed: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub requests: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub input_tokens: u64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub output_tokens: u64,
    pub estimated_cost: Option<f64>,
    pub currency: Option<String>,
    /// 暂停或失败的原因，供界面直接展示。
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct WorkRecord {
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub id: i64,
    pub file_name: String,
    pub title: Option<String>,
    pub input_hash: String,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct WorkJob {
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub id: i64,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub work_id: i64,
    pub status: JobStatus,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct WorkTree {
    pub records: Vec<WorkRecord>,
    pub jobs: Vec<WorkJob>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct CsvPage {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub offset: u64,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct ProviderProfile {
    #[cfg_attr(feature = "bindings", ts(type = "number"))]
    pub id: i64,
    pub name: String,
    pub config: ProviderConfig,
    pub active: bool,
}
