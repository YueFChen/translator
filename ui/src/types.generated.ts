// Generated from Rust by examples/generate_bindings.rs. Do not edit.
export type Locale = { tag: string, };
export type SourceVersion = { sha256: string, size_bytes: number, };
export type ColumnMapping = { source_text_column: number, source_locale: Locale, selection_column: number | null, context_column: number | null, resource_key_column: number | null, target_columns: Array<TargetColumn>, };
export type TargetColumn = { column: number, locale: Locale, };
export type Segment = { row_number: number, source_text: string, source_locale: Locale, context: string | null, resource_key: string | null, source_version: SourceVersion, };
export type PlaceholderSignature = { schema_version: number, canonical: string, };
export type TranslationUnit = { segment: Segment, target_locale: Locale, target_column: number, placeholder_signature: PlaceholderSignature, state: CellState, target_text: string | null, };
export type CellState = "empty" | "existing" | "memory_reused" | "glossary_exact" | "machine_draft" | "human_edited" | "approved";
export type IssueSeverity = "blocking" | "review";
export type QualityIssue = { row_number: number, target_locale: Locale, code: string, severity: IssueSeverity, details: string, };
export type JobStatus = "queued" | "running" | "paused" | "succeeded" | "failed" | "cancelled";
export type ProviderCapabilities = { supported_pairs: Array<LocalePair>, supports_batch: boolean, supports_structured_output: boolean, supports_glossary_constraints: boolean, max_input_chars: number | null, max_batch_units: number | null, requests_per_minute: number | null, price_per_million_input_tokens: number | null, price_per_million_output_tokens: number | null, currency: string | null, };
export type LocalePair = { source: Locale, target: Locale, };
export type CsvInspection = { file_name: string, headers: Array<string>, has_bom: boolean, source_detected: boolean, default_mapping: ColumnMapping, };
export type TargetSummary = { locale: Locale, pending: number, existing: number, memory_reused: number, glossary_exact: number, };
export type Preflight = { input_name: string, source_version: SourceVersion, mapping: ColumnMapping, has_bom: boolean, total_rows: number, selected_rows: number, pending_cells: number, existing_cells: number, memory_reused_cells: number, glossary_exact_cells: number, unique_units: number, empty_source_rows: number, format_rows: number, newline_rows: number, carriage_return_rows: number, issue_count: number, term_hit_count: number, term_hit_preview: Array<TermHit>, memory_version: number, glossary_version: number, issue_preview: Array<QualityIssue>, targets: Array<TargetSummary>, };
export type TranslationExport = { directory: string, csv_path: string, rows: number, };
export type MemoryEntry = { id: number, source_locale: Locale, target_locale: Locale, source_text: string, target_text: string, context: string, resource_key: string, placeholder_signature: string, source_kind: string, enabled: boolean, input_hash: string | null, row_number: number | null, qa_blocking: boolean, version: number, };
export type TermKind = "ordinary" | "do_not_translate" | "forbidden";
export type GlossaryTerm = { id: number, kind: TermKind, source_locale: Locale, target_locale: Locale, source_text: string, target_text: string, aliases: Array<string>, context: string | null, resource_key: string | null, disambiguation: string, enabled: boolean, version: number, };
export type TermInput = { id: number | null, kind: TermKind, source_locale: Locale, target_locale: Locale, source_text: string, target_text: string, aliases: Array<string>, context: string | null, resource_key: string | null, disambiguation: string, expected_version: number | null, };
export type TermHit = { row_number: number, target_locale: Locale, term_id: number, kind: TermKind, start_char: number, end_char: number, source_surface: string, target_text: string, };
export type ProviderConfig = { base_url: string, model: string, remark: string, allow_loopback: boolean, timeout_seconds: number,
/**
 * 每分钟请求上限；`None` 或 `0` 表示不人为设限。
 */
requests_per_minute: number | null, price_per_million_input_tokens: number | null, price_per_million_output_tokens: number | null, currency: string | null, has_key: boolean, };
export type ProviderConfigInput = { base_url: string, model: string, remark: string, allow_loopback: boolean, timeout_seconds: number,
/**
 * 每分钟请求上限；`None` 或 `0` 表示不人为设限。
 */
requests_per_minute: number | null, price_per_million_input_tokens: number | null, price_per_million_output_tokens: number | null, currency: string | null,
/**
 * `None` 保持既有密钥；`Some("")` 清除；`Some(非空)` 覆盖。
 */
api_key: string | null, };
export type JobLimits = { concurrency: number,
/**
 * 单次请求打包的片段数；较大批次能显著减少请求数与提示词重复。
 */
batch_size: number, include_false_rows: boolean, input_token_budget: number, output_token_budget: number, };
export type JobProgress = { job_id: number, status: JobStatus, total: number, done: number, failed: number, requests: number, input_tokens: number, output_tokens: number, estimated_cost: number | null, currency: string | null,
/**
 * 暂停或失败的原因，供界面直接展示。
 */
note: string | null, };
export type WorkRecord = { id: number, file_name: string, title: string | null, input_hash: string, created_at: number, };
export type WorkJob = { id: number, work_id: number, status: JobStatus, created_at: number, };
export type WorkTree = { records: Array<WorkRecord>, jobs: Array<WorkJob>, };
export type CsvPage = { headers: Array<string>, rows: Array<Array<string>>, offset: number, has_more: boolean, };
export type ProviderProfile = { id: number, name: string, config: ProviderConfig, active: boolean, };
