use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use csv::StringRecord;
use sha2::{Digest, Sha256};

use crate::csvio::{self, MAX_FILE_BYTES, MAX_ROWS};
use crate::qa;
use crate::store::{Store, safe_translation};
use crate::{
    ColumnMapping, CsvInspection, IssueSeverity, Locale, Preflight, SourceVersion, TargetSummary,
    TranslationExport,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct Prepared {
    pub snapshot: PathBuf,
    pub preflight: Preflight,
}

fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("操作已取消".into())
    } else {
        Ok(())
    }
}

fn unique_name() -> String {
    let tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{tick}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    )
}

fn source_hash(path: &Path, cancel: &AtomicBool) -> Result<SourceVersion, String> {
    let mut file = BufReader::new(File::open(path).map_err(|e| format!("无法读取输入快照：{e}"))?);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0;
    loop {
        cancelled(cancel)?;
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("读取输入快照失败：{e}"))?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hash.update(&buffer[..n]);
    }
    Ok(SourceVersion {
        sha256: format!("{:x}", hash.finalize()),
        size_bytes: size,
    })
}

fn snapshot(
    source: &Path,
    data_dir: &Path,
    cancel: &AtomicBool,
) -> Result<(PathBuf, SourceVersion), String> {
    let dir = data_dir.join("translator").join("inputs");
    fs::create_dir_all(&dir).map_err(|e| format!("无法创建输入快照目录：{e}"))?;
    let temp = dir.join(format!(".{}.tmp", unique_name()));
    let result = (|| {
        let mut input =
            BufReader::new(File::open(source).map_err(|e| format!("无法打开 CSV：{e}"))?);
        let mut output = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|e| format!("无法建立输入快照：{e}"))?,
        );
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            cancelled(cancel)?;
            let n = input
                .read(&mut buffer)
                .map_err(|e| format!("读取 CSV 失败：{e}"))?;
            if n == 0 {
                break;
            }
            size += n as u64;
            if size > MAX_FILE_BYTES {
                return Err("CSV 文件超过 128 MiB 上限".into());
            }
            hash.update(&buffer[..n]);
            output
                .write_all(&buffer[..n])
                .map_err(|e| format!("写入输入快照失败：{e}"))?;
        }
        if size == 0 {
            return Err("CSV 文件为空".into());
        }
        output
            .flush()
            .map_err(|e| format!("刷新输入快照失败：{e}"))?;
        output
            .get_ref()
            .sync_all()
            .map_err(|e| format!("同步输入快照失败：{e}"))?;
        drop(output);
        let version = SourceVersion {
            sha256: format!("{:x}", hash.finalize()),
            size_bytes: size,
        };
        let final_path = dir.join(format!("{}.csv", version.sha256));
        if final_path.exists() {
            let existing = source_hash(&final_path, cancel)?;
            if existing != version {
                return Err("已有输入快照与文件哈希不一致".into());
            }
        } else {
            fs::rename(&temp, &final_path).map_err(|e| format!("发布输入快照失败：{e}"))?;
            let mut permissions = fs::metadata(&final_path)
                .map_err(|e| format!("读取快照权限失败：{e}"))?
                .permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&final_path, permissions)
                .map_err(|e| format!("设置快照只读失败：{e}"))?;
        }
        Ok((final_path, version))
    })();
    let _ = fs::remove_file(&temp);
    result
}

#[derive(Clone)]
struct CellReport {
    state: &'static str,
    target_text: String,
}

/// 已落库的机器草稿。键是 `(记录序号, 目标列)`，与任务表一一对应。
pub type DraftLookup = HashMap<(u64, usize), DraftCell>;

#[derive(Clone)]
pub struct DraftCell {
    pub target_text: String,
    pub qa_state: String,
}

/// 译文格的来源。显式记录而不是事后反查：反查要多打一次记忆索引，还容易把来源认错。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CellOrigin {
    Empty,
    Existing,
    Memory,
    Glossary,
    Draft,
}

/// 导出模式决定"草稿能不能写进 CSV"。正式导出只含已有内容与记忆/词条复用结果，机器草稿只进草稿导出。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExportMode {
    Approved,
    DraftsFor(i64),
}

impl ExportMode {
    fn directory_prefix(self) -> &'static str {
        match self {
            Self::Approved => "translation",
            Self::DraftsFor(_) => "drafts",
        }
    }
}

/// 哪些格会被写进导出 CSV。已有的非空格永远保持原值，不在此列。
fn written_state(state: &str) -> bool {
    matches!(state, "memory_reused" | "glossary_exact" | "machine_draft")
}

/// 流式扫描；闭包可逐记录写报告/结果，不保存整份 CSV。
#[allow(clippy::too_many_arguments)]
fn scan(
    path: &Path,
    inspection: &CsvInspection,
    mapping: &ColumnMapping,
    version: &SourceVersion,
    cancel: &AtomicBool,
    store: Option<&Store>,
    terms: &[crate::GlossaryTerm],
    drafts: Option<&DraftLookup>,
    include_false_rows: bool,
    mut on_row: impl FnMut(&StringRecord, u64, &[CellReport]) -> Result<(), String>,
) -> Result<Preflight, String> {
    let mut reader = csvio::open_reader(path)?;
    let headers = csvio::headers(&mut reader)?;
    if headers != inspection.headers {
        return Err("输入文件表头在选择后发生变化".into());
    }
    csvio::validate_mapping(&headers, mapping)?;
    let mut summary = Preflight {
        input_name: inspection.file_name.clone(),
        source_version: version.clone(),
        mapping: mapping.clone(),
        has_bom: inspection.has_bom,
        total_rows: 0,
        selected_rows: 0,
        pending_cells: 0,
        existing_cells: 0,
        memory_reused_cells: 0,
        glossary_exact_cells: 0,
        unique_units: 0,
        empty_source_rows: 0,
        format_rows: 0,
        newline_rows: 0,
        carriage_return_rows: 0,
        issue_count: 0,
        term_hit_count: 0,
        term_hit_preview: Vec::new(),
        memory_version: store.map(|s| s.versions()).transpose()?.map_or(0, |v| v.0),
        glossary_version: store.map(|s| s.versions()).transpose()?.map_or(0, |v| v.1),
        issue_preview: Vec::new(),
        targets: mapping
            .target_columns
            .iter()
            .map(|item| TargetSummary {
                locale: item.locale.clone(),
                pending: 0,
                existing: 0,
                memory_reused: 0,
                glossary_exact: 0,
            })
            .collect(),
    };
    let mut record = StringRecord::new();
    let mut units: HashSet<[u8; 32]> = HashSet::new();
    let mut prior: HashMap<[u8; 32], String> = HashMap::new();
    while reader
        .read_record(&mut record)
        .map_err(|e| format!("CSV 解析失败：{e}"))?
    {
        cancelled(cancel)?;
        summary.total_rows += 1;
        if summary.total_rows > MAX_ROWS {
            return Err("CSV 超过 200,000 条记录上限".into());
        }
        let row = summary.total_rows + 1;
        let source = record.get(mapping.source_text_column).unwrap_or_default();
        let marker = mapping
            .selection_column
            .and_then(|i| record.get(i))
            .unwrap_or("TRUE");
        let selected = marker.trim().eq_ignore_ascii_case("TRUE")
            || (include_false_rows && marker.trim().eq_ignore_ascii_case("FALSE"));
        if selected {
            summary.selected_rows += 1;
        }
        if selected && source.is_empty() {
            summary.empty_source_rows += 1;
        }
        let source_format = qa::parse(source);
        if selected && let Ok(parsed) = &source_format {
            if parsed.has_format() {
                summary.format_rows += 1;
            }
            if parsed.has_real_newline() {
                summary.newline_rows += 1;
            }
            if parsed.has_carriage_return() {
                summary.carriage_return_rows += 1;
            }
        }
        let context = mapping
            .context_column
            .and_then(|i| record.get(i))
            .unwrap_or_default();
        let resource = mapping
            .resource_key_column
            .and_then(|i| record.get(i))
            .unwrap_or_default();
        let mut reports = Vec::with_capacity(mapping.target_columns.len());
        for (target_index, target) in mapping.target_columns.iter().enumerate() {
            let original = record.get(target.column).unwrap_or_default();
            let mut resolved = original.to_owned();
            let mut origin = if original.is_empty() {
                CellOrigin::Empty
            } else {
                CellOrigin::Existing
            };
            let mut issues = Vec::new();
            if mapping.selection_column.is_some()
                && !selected
                && !marker.trim().eq_ignore_ascii_case("FALSE")
            {
                issues.push(qa::issue(
                    row,
                    &target.locale,
                    "selection_marker",
                    IssueSeverity::Review,
                    "选择标记不是 TRUE 或 FALSE，本行未处理",
                ));
            }
            if selected && !source.is_empty() && original.is_empty() {
                let signature = source_format
                    .as_ref()
                    .map(|f| f.signature().canonical.clone())
                    .unwrap_or_default();
                if let Some(store) = store
                    && source_format.is_ok()
                    && let Some(candidate) = store.lookup(
                        &mapping.source_locale.tag,
                        &target.locale.tag,
                        source,
                        context,
                        resource,
                        &signature,
                    )?
                    && safe_translation(source, &candidate)
                {
                    resolved = candidate;
                    origin = CellOrigin::Memory;
                }
                if origin == CellOrigin::Empty && source_format.is_ok() {
                    let matches: Vec<_> = terms
                        .iter()
                        .filter(|term| {
                            term.enabled
                                && term.kind != crate::TermKind::Forbidden
                                && term.source_locale == mapping.source_locale
                                && term.target_locale == target.locale
                                && term.context.as_deref().is_none_or(|v| v == context)
                                && term.resource_key.as_deref().is_none_or(|v| v == resource)
                                && (term.source_text == source
                                    || term.aliases.iter().any(|a| a == source))
                        })
                        .collect();
                    if matches.len() == 1 {
                        let term = matches[0];
                        let candidate = if term.kind == crate::TermKind::DoNotTranslate {
                            source.to_owned()
                        } else {
                            term.target_text.clone()
                        };
                        if safe_translation(source, &candidate) {
                            resolved = candidate;
                            origin = CellOrigin::Glossary;
                        }
                    }
                }
                // 机器草稿排在记忆与词条之后：已落库的复用规则优先于本次模型输出。
                // 格式校验不通过的草稿**不写进 CSV**，只在报告里点名，避免把坏占位符导出成成品。
                if origin == CellOrigin::Empty
                    && source_format.is_ok()
                    && let Some(cell) = drafts.and_then(|map| map.get(&(row, target.column)))
                {
                    if safe_translation(source, &cell.target_text) {
                        resolved = cell.target_text.clone();
                        origin = CellOrigin::Draft;
                    } else {
                        issues.push(qa::issue(
                            row,
                            &target.locale,
                            "draft_withheld",
                            IssueSeverity::Review,
                            format!("机器草稿未通过格式校验，未写入导出：{}", cell.qa_state),
                        ));
                    }
                }
            }
            for term in terms.iter().filter(|term| {
                term.enabled
                    && term.source_locale == mapping.source_locale
                    && term.target_locale == target.locale
                    && term.context.as_deref().is_none_or(|v| v == context)
                    && term.resource_key.as_deref().is_none_or(|v| v == resource)
            }) {
                if !selected || source.is_empty() {
                    break;
                }
                for surface in std::iter::once(&term.source_text).chain(term.aliases.iter()) {
                    if surface.is_empty() {
                        continue;
                    }
                    for (byte_start, _) in source.match_indices(surface) {
                        let start = source[..byte_start].chars().count();
                        summary.term_hit_count += 1;
                        if summary.term_hit_preview.len() < 100 {
                            summary.term_hit_preview.push(crate::TermHit {
                                row_number: row,
                                target_locale: target.locale.clone(),
                                term_id: term.id,
                                kind: term.kind,
                                start_char: start,
                                end_char: start + surface.chars().count(),
                                source_surface: surface.clone(),
                                target_text: term.target_text.clone(),
                            });
                        }
                    }
                }
            }
            let text = resolved.as_str();
            let state = if !selected {
                "skipped"
            } else if source.is_empty() {
                "empty_source"
            } else if origin == CellOrigin::Existing {
                summary.existing_cells += 1;
                summary.targets[target_index].existing += 1;
                "existing"
            } else if !text.is_empty() {
                match origin {
                    CellOrigin::Memory => {
                        summary.memory_reused_cells += 1;
                        summary.targets[target_index].memory_reused += 1;
                        "memory_reused"
                    }
                    CellOrigin::Glossary => {
                        summary.glossary_exact_cells += 1;
                        summary.targets[target_index].glossary_exact += 1;
                        "glossary_exact"
                    }
                    // 草稿不占预检的记忆/词条计数：预检根本不会传入草稿。
                    CellOrigin::Draft => "machine_draft",
                    CellOrigin::Empty | CellOrigin::Existing => unreachable!("已在前面的分支处理"),
                }
            } else {
                summary.pending_cells += 1;
                summary.targets[target_index].pending += 1;
                let signature = source_format
                    .as_ref()
                    .map(|info| info.signature().canonical.clone())
                    .unwrap_or_default();
                let key = task_key(
                    source,
                    context,
                    resource,
                    &mapping.source_locale,
                    &target.locale,
                    &signature,
                    version,
                );
                if units.insert(key) {
                    summary.unique_units += 1;
                    if summary.unique_units > 1_000_000 {
                        return Err("唯一片段任务超过 1,000,000 个上限".into());
                    }
                }
                "empty"
            };
            if selected && text.trim().is_empty() {
                issues.push(qa::issue(
                    row,
                    &target.locale,
                    "untranslated_required",
                    IssueSeverity::Blocking,
                    "该行标记为需要翻译，但目标单元格仍为空",
                ));
            }
            if selected && !source.is_empty() {
                // 源文形态是"被切开的片段"时先提示：这类格单独翻译只会脱离语境。
                // 与 `invalid_source_format` 一样属于源文级问题，按目标格各记一条，
                // 便于界面按列定位。只提示、不阻止导出、也不改变派发。
                if let Some(reason) = qa::fragment_reason(source) {
                    issues.push(qa::issue(
                        row,
                        &target.locale,
                        "suspected_fragment",
                        IssueSeverity::Review,
                        format!("疑似被上游切分的拼接片段，不适合单独翻译：{reason}"),
                    ));
                }
                match &source_format {
                    Err(error) => issues.push(qa::issue(
                        row,
                        &target.locale,
                        "invalid_source_format",
                        IssueSeverity::Blocking,
                        error.clone(),
                    )),
                    Ok(source_info) if !text.is_empty() => match qa::parse(text) {
                        Err(error) => issues.push(qa::issue(
                            row,
                            &target.locale,
                            "invalid_target_format",
                            IssueSeverity::Blocking,
                            error,
                        )),
                        Ok(target_info) => issues.extend(qa::compare(
                            source_info,
                            &target_info,
                            row,
                            &target.locale,
                        )),
                    },
                    _ => {}
                }
                if !text.is_empty() {
                    for term in terms.iter().filter(|term| {
                        term.enabled
                            && term.kind == crate::TermKind::Forbidden
                            && term.source_locale == mapping.source_locale
                            && term.target_locale == target.locale
                            && term.context.as_deref().is_none_or(|v| v == context)
                            && term.resource_key.as_deref().is_none_or(|v| v == resource)
                    }) {
                        if !term.target_text.is_empty()
                            && text.contains(&term.target_text)
                            && (source.contains(&term.source_text)
                                || term.aliases.iter().any(|alias| source.contains(alias)))
                        {
                            issues.push(qa::issue(
                                row,
                                &target.locale,
                                "forbidden_term",
                                IssueSeverity::Review,
                                format!("译文包含禁用译法：{}", term.target_text),
                            ));
                        }
                    }
                    if qa::numbers(source) != qa::numbers(text) {
                        issues.push(qa::issue(
                            row,
                            &target.locale,
                            "number_mismatch",
                            IssueSeverity::Review,
                            "数字与源文不同",
                        ));
                    }
                    if source == text && source.chars().any(char::is_alphabetic) {
                        issues.push(qa::issue(
                            row,
                            &target.locale,
                            "possibly_untranslated",
                            IssueSeverity::Review,
                            "译文与源文完全相同，请确认",
                        ));
                    }
                    let scope_key = task_key(
                        source,
                        context,
                        resource,
                        &mapping.source_locale,
                        &target.locale,
                        "conflict",
                        version,
                    );
                    if let Some(previous) = prior.get(&scope_key) {
                        if previous != text {
                            issues.push(qa::issue(
                                row,
                                &target.locale,
                                "scope_conflict",
                                IssueSeverity::Review,
                                "同作用域的相同源文已有另一种译文",
                            ));
                        }
                    } else {
                        prior.insert(scope_key, text.to_owned());
                    }
                }
            }
            summary.issue_count += issues.len() as u64;
            if summary.issue_preview.len() < 100 {
                summary.issue_preview.extend(
                    issues
                        .iter()
                        .take(100 - summary.issue_preview.len())
                        .cloned(),
                );
            }
            reports.push(CellReport {
                state,
                target_text: text.to_owned(),
            });
        }
        on_row(&record, row, &reports)?;
    }
    if summary.total_rows == 0 {
        return Err("CSV 没有数据记录".into());
    }
    Ok(summary)
}

fn task_key(
    source: &str,
    context: &str,
    resource: &str,
    source_locale: &Locale,
    target_locale: &Locale,
    signature: &str,
    version: &SourceVersion,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    for value in [
        &version.sha256,
        &source_locale.tag,
        &target_locale.tag,
        source,
        context,
        resource,
        signature,
    ] {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    hash.finalize().into()
}

/// 去重组键的十六进制表示；落库用文本，便于直接比对与排查。
fn hex_key(key: [u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in key {
        // 写入 `String` 不会失败。
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
pub fn prepare(
    source: &Path,
    data_dir: &Path,
    mapping: ColumnMapping,
    cancel: &AtomicBool,
) -> Result<Prepared, String> {
    prepare_with_store(source, data_dir, mapping, cancel, None)
}

pub fn prepare_with_store(
    source: &Path,
    data_dir: &Path,
    mapping: ColumnMapping,
    cancel: &AtomicBool,
    mut store: Option<&mut Store>,
) -> Result<Prepared, String> {
    let (snapshot, version) = snapshot(source, data_dir, cancel)?;
    let inspection = csvio::inspect(&snapshot)?;
    let source_name = source
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let inspection = CsvInspection {
        file_name: source_name,
        ..inspection
    };
    if let Some(store) = store.as_deref_mut() {
        let mut reader = csvio::open_reader(&snapshot)?;
        let headers = csvio::headers(&mut reader)?;
        csvio::validate_mapping(&headers, &mapping)?;
        store.begin_import()?;
        for (index, result) in reader.records().enumerate() {
            cancelled(cancel)?;
            let record = result.map_err(|e| format!("CSV 解析失败：{e}"))?;
            let source = record.get(mapping.source_text_column).unwrap_or_default();
            if source.is_empty() {
                continue;
            }
            let context = mapping
                .context_column
                .and_then(|i| record.get(i))
                .unwrap_or_default();
            let resource = mapping
                .resource_key_column
                .and_then(|i| record.get(i))
                .unwrap_or_default();
            let parsed = qa::parse(source);
            let signature = parsed
                .as_ref()
                .map(|p| p.signature().canonical.clone())
                .unwrap_or_default();
            for target in &mapping.target_columns {
                let text = record.get(target.column).unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                store.import(
                    &mapping.source_locale.tag,
                    &target.locale.tag,
                    source,
                    text,
                    context,
                    resource,
                    &signature,
                    &version.sha256,
                    (index + 2) as u64,
                    !safe_translation(source, text),
                )?;
            }
        }
        store.finish_import()?;
    }
    let terms = store
        .as_deref()
        .map(|s| s.list_terms())
        .transpose()?
        .unwrap_or_default();
    let preflight = scan(
        &snapshot,
        &inspection,
        &mapping,
        &version,
        cancel,
        store.as_deref(),
        &terms,
        None,
        false,
        |_, _, _| Ok(()),
    )?;
    Ok(Prepared {
        snapshot,
        preflight,
    })
}

#[cfg(test)]
pub fn export(
    prepared: &Prepared,
    data_dir: &Path,
    cancel: &AtomicBool,
) -> Result<TranslationExport, String> {
    export_with_store(prepared, data_dir, cancel, None, ExportMode::Approved)
}

pub fn export_with_store(
    prepared: &Prepared,
    data_dir: &Path,
    cancel: &AtomicBool,
    store: Option<&Store>,
    mode: ExportMode,
) -> Result<TranslationExport, String> {
    if let Some(store) = store {
        let versions = store.versions()?;
        if versions
            != (
                prepared.preflight.memory_version,
                prepared.preflight.glossary_version,
            )
        {
            return Err("记忆或词库已变化，请重新预检".into());
        }
    }
    let terms = store
        .map(|s| s.list_terms())
        .transpose()?
        .unwrap_or_default();
    let actual = source_hash(&prepared.snapshot, cancel)?;
    if actual != prepared.preflight.source_version {
        return Err("输入快照已变化，请重新导入".into());
    }
    // 只有草稿模式才去读作业结果；正式导出保持"只含已有内容与记忆/词条复用结果"。
    let include_false_rows = match (mode, store) {
        (ExportMode::DraftsFor(job_id), Some(store)) => {
            crate::job::limits_for_job(store, job_id)?.include_false_rows
        }
        _ => false,
    };
    let drafts = match (mode, store) {
        (ExportMode::DraftsFor(job_id), Some(store)) => {
            Some(crate::job::draft_lookup_job(store, job_id, &actual.sha256)?)
        }
        _ => None,
    };
    let inspection = csvio::inspect(&prepared.snapshot)?;
    let inspection = CsvInspection {
        file_name: prepared.preflight.input_name.clone(),
        ..inspection
    };
    let output_root = data_dir.join("翻译导出");
    fs::create_dir_all(&output_root).map_err(|e| format!("输出目录不可写：{e}"))?;
    let name = format!(
        "{}-{}-{}",
        mode.directory_prefix(),
        &actual.sha256[..12],
        unique_name()
    );
    let stage = output_root.join(format!(".{name}.tmp"));
    let final_dir = output_root.join(&name);
    fs::create_dir(&stage).map_err(|e| format!("输出目录不可写：{e}"))?;
    let result = (|| {
        let csv_path = stage.join("result.csv");
        let output = File::create_new(&csv_path).map_err(|e| format!("无法创建导出 CSV：{e}"))?;
        let mut output = BufWriter::new(output);
        output
            .write_all(&[0xef, 0xbb, 0xbf])
            .map_err(|e| format!("写入 BOM 失败：{e}"))?;
        let mut writer = csv::WriterBuilder::new().from_writer(output);
        let mut input = csvio::open_reader(&prepared.snapshot)?;
        let headers = csvio::headers(&mut input)?;
        writer
            .write_record(&headers)
            .map_err(|e| format!("写入导出表头失败：{e}"))?;
        let summary = scan(
            &prepared.snapshot,
            &inspection,
            &prepared.preflight.mapping,
            &actual,
            cancel,
            store,
            &terms,
            drafts.as_ref(),
            include_false_rows,
            |record, row, cells| {
                let mut fields: Vec<String> = record.iter().map(str::to_owned).collect();
                for (target, cell) in prepared.preflight.mapping.target_columns.iter().zip(cells) {
                    if written_state(cell.state) {
                        fields[target.column] = cell.target_text.clone();
                    }
                }
                writer
                    .write_record(&fields)
                    .map_err(|e| format!("写入导出记录 {row} 失败：{e}"))?;
                Ok(())
            },
        )?;
        cancelled(cancel)?;
        writer
            .flush()
            .map_err(|e| format!("刷新导出 CSV 失败：{e}"))?;
        drop(writer);
        // 发布前重新解析导出 CSV；坏文件不能以完成品出现在用户目录。
        let mut verify = csvio::open_reader(&csv_path)?;
        let verified_headers = csvio::headers(&mut verify)?;
        if verified_headers != headers {
            return Err(format!(
                "导出校验失败：表头变化（{headers:?} → {verified_headers:?}）"
            ));
        }
        let mut verified_rows = 0;
        let mut output_records = verify.records();
        scan(
            &prepared.snapshot,
            &inspection,
            &prepared.preflight.mapping,
            &actual,
            cancel,
            store,
            &terms,
            drafts.as_ref(),
            include_false_rows,
            |record, _, cells| {
                let output = output_records
                    .next()
                    .ok_or("导出校验失败：记录数变化")?
                    .map_err(|e| format!("导出校验失败：{e}"))?;
                let mut expected: Vec<String> = record.iter().map(str::to_owned).collect();
                for (target, cell) in prepared.preflight.mapping.target_columns.iter().zip(cells) {
                    if written_state(cell.state) {
                        expected[target.column] = cell.target_text.clone();
                    }
                }
                if expected.iter().map(String::as_str).collect::<Vec<_>>()
                    != output.iter().collect::<Vec<_>>()
                {
                    return Err(format!(
                        "导出校验失败：第 {} 条记录的单元格变化",
                        verified_rows + 1
                    ));
                }
                verified_rows += 1;
                Ok(())
            },
        )?;
        if output_records.next().is_some() {
            return Err("导出校验失败：记录数变化".into());
        }
        if verified_rows != summary.total_rows {
            return Err("导出校验失败：记录数变化".into());
        }
        drop(output_records);
        drop(input);
        drop(verify);
        if let Some(store) = store
            && store.versions()?
                != (
                    prepared.preflight.memory_version,
                    prepared.preflight.glossary_version,
                )
        {
            return Err("记忆或词库已变化，请重新预检".into());
        }
        fs::rename(&stage, &final_dir).map_err(|e| format!("发布导出文件失败：{e}"))?;
        Ok(TranslationExport {
            directory: final_dir.to_string_lossy().into_owned(),
            csv_path: final_dir.join("result.csv").to_string_lossy().into_owned(),
            rows: summary.total_rows,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

pub fn inspect_file(path: &Path) -> Result<CsvInspection, String> {
    csvio::inspect(path)
}

/// 一个待翻译的目标格。字段与作业表的去重主键一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitDraft {
    pub row_number: u64,
    pub target_column: usize,
    pub target_locale: Locale,
    pub source_locale: Locale,
    pub source_text: String,
    pub context: String,
    pub resource_key: String,
    pub signature: String,
    /// 去重组键：键相同的格在同一次作业里共用一次模型调用。
    pub group_key: String,
}

/// 收集当前快照下真正需要调用模型的格。
///
/// 复用 [`scan`]，因此"哪些格要翻"的判定与预检完全同一套规则：只有选中行、源文非空、
/// 目标格为空，且记忆与词条都没能填上的格才会进任务表。这样预检显示的待填数就是作业规模。
pub fn collect_units(
    prepared: &Prepared,
    store: Option<&Store>,
    cancel: &AtomicBool,
    include_false_rows: bool,
) -> Result<Vec<UnitDraft>, String> {
    let inspection = csvio::inspect(&prepared.snapshot)?;
    let inspection = CsvInspection {
        file_name: prepared.preflight.input_name.clone(),
        ..inspection
    };
    let terms = store
        .map(|s| s.list_terms())
        .transpose()?
        .unwrap_or_default();
    let mapping = &prepared.preflight.mapping;
    let mut drafts: Vec<UnitDraft> = Vec::new();
    scan(
        &prepared.snapshot,
        &inspection,
        mapping,
        &prepared.preflight.source_version,
        cancel,
        store,
        &terms,
        None,
        include_false_rows,
        |record, row, cells| {
            let source = record.get(mapping.source_text_column).unwrap_or_default();
            let context = mapping
                .context_column
                .and_then(|i| record.get(i))
                .unwrap_or_default();
            let resource = mapping
                .resource_key_column
                .and_then(|i| record.get(i))
                .unwrap_or_default();
            let signature = qa::parse(source)
                .as_ref()
                .map(|info| info.signature().canonical.clone())
                .unwrap_or_default();
            for (target, cell) in mapping.target_columns.iter().zip(cells) {
                if cell.state != "empty" {
                    continue;
                }
                // 去重键与预检统计 `unique_units` 用的是同一个 `task_key`：预检报多少唯一片段，
                // 作业就会发多少次请求，两者不会再各说各话。
                let group_key = hex_key(task_key(
                    source,
                    context,
                    resource,
                    &mapping.source_locale,
                    &target.locale,
                    &signature,
                    &prepared.preflight.source_version,
                ));
                drafts.push(UnitDraft {
                    row_number: row,
                    target_column: target.column,
                    target_locale: target.locale.clone(),
                    source_locale: mapping.source_locale.clone(),
                    source_text: source.to_owned(),
                    context: context.to_owned(),
                    resource_key: resource.to_owned(),
                    signature: signature.clone(),
                    group_key,
                });
            }
            Ok(())
        },
    )?;
    Ok(drafts)
}
