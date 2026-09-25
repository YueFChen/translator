//! 全自动翻译作业：任务表、批量调度与状态迁移。
//!
//! 三条不变式贯穿本模块：
//! 1. **一批一事务**：一个批次的结果与组内每一格的 QA 结论同时提交，不会出现"有译文没校验"的中间态。
//! 2. **不盲目重放，但也不等人**：进程中断时仍标 `running` 的格在下次启动直接退回待处理，
//!    而不是留给人工核对；结果不确定的请求同样按重试规则机械重发。
//! 3. **暂停不等于丢弃**：暂停/取消只停止派发新请求，已发出的请求按限时收尾并照样落库。
//!
//! 派发以**批次**为单位：一批里装若干去重组，组内按源文、语境、资源键与目标语言去重，
//! 因此预检报出的 `unique_units` 就是本作业要翻的片段总数。同批共享源语言与目标语言，
//! 提示词按 1-based 序号列出片段，响应按序号回填。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use tokio::task::JoinSet;

use crate::config::{MAX_INPUT_CHARS, MIN_SOURCE_BUDGET, PROMPT_OVERHEAD_RESERVE};
use crate::engine::{DraftCell, DraftLookup, UnitDraft};
use crate::provider::{
    BatchItem, BatchRequest, MAX_CONCURRENCY, MAX_RETRIES, MemoryExample, ProviderError,
    TermConstraint, TranslatorProvider, fit_items,
};
use crate::store::{Store, safe_translation};
use crate::{GlossaryTerm, JobLimits, JobProgress, JobStatus, Locale, ProviderCapabilities};

/// 进度事件的节流间隔。作业每结算一个批次都会产生进度变化，但界面不需要跟着每一批重画。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(1000);

fn err(error: impl std::fmt::Display) -> String {
    format!("翻译作业数据库错误：{error}")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 作业运行控制。暂停与取消只影响"是否继续派发"，不打断已发出的请求。
#[derive(Default)]
pub struct JobControl {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

impl JobControl {
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// 调用方注入的上下文材料：词条（强约束）与既有译文（仅参考）。
#[derive(Default)]
pub struct ContextSources {
    /// 目标语言标签 → 该语言的启用词条。
    pub terms: HashMap<String, Vec<GlossaryTerm>>,
    /// 目标语言标签 → 少量既有译文示例。
    pub examples: HashMap<String, Vec<MemoryExample>>,
}

/// 一次批量调用的结果。
enum BatchOutcome {
    /// 与请求同序的逐片段结果；`Err` 是该片段失败的原因，其余片段照样落库。
    Items(Vec<Result<String, String>>),
    /// 服务整体不可用（连接不上、持续 5xx/429、超时）：这批一个片段都没结算。
    ///
    /// 这不是"这些格出了问题"，而是"现在没法翻"。因此它**不是失败**：整批退回待处理、
    /// 作业停在可续跑的 `paused`，等服务恢复后点"继续"接着跑——否则一次网络抖动
    /// 就会把一批好数据记成失败，还逼用户重新发起。
    Unavailable(String),
    /// 配置不可用（端点、模型名、密钥）：重试无用，直接结束作业。
    Fatal(String),
}

/// 结算一批之后作业该怎么走。
enum SettleResult {
    /// 正常结算，继续派发。
    Settled,
    /// 这一批没结算，应当把作业停在 `paused`（原因随返回值给出）。
    Unavailable(String),
    /// 这一批没结算且重试无用，作业应当记为 `failed`。
    Fatal(String),
}

/// 一批在飞请求的回执：组键、日志定位标签、批次结果、请求次数、token 用量。
type InflightResult = (
    Vec<String>,
    Vec<(u64, String, u64)>,
    BatchOutcome,
    u64,
    (Option<u64>, Option<u64>),
);

fn job_status(value: &str) -> JobStatus {
    match value {
        "running" => JobStatus::Running,
        "paused" => JobStatus::Paused,
        "succeeded" => JobStatus::Succeeded,
        "failed" => JobStatus::Failed,
        "cancelled" => JobStatus::Cancelled,
        _ => JobStatus::Queued,
    }
}

fn job_status_str(value: JobStatus) -> &'static str {
    match value {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Paused => "paused",
        JobStatus::Succeeded => "succeeded",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
}

/// 日志里用的中文状态名。
fn status_cn(value: JobStatus) -> &'static str {
    match value {
        JobStatus::Queued => "排队中",
        JobStatus::Running => "运行中",
        JobStatus::Paused => "已暂停",
        JobStatus::Succeeded => "已完成",
        JobStatus::Failed => "失败",
        JobStatus::Cancelled => "已取消",
    }
}

/// 在事务中执行一段数据库操作。
fn in_transaction<T>(
    connection: &Connection,
    body: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
) -> Result<T, String> {
    let transaction = connection.unchecked_transaction().map_err(err)?;
    let value = body(&transaction).map_err(err)?;
    transaction.commit().map_err(err)?;
    Ok(value)
}

/// 新建作业并把待翻格一次性写入任务表。
///
/// 每次调用创建独立作业；同一作业内按记录序号和目标列保证单格唯一。
/// 续跑必须使用已有作业 ID，创建新作业表示重新翻译。
///
/// 初始状态是 `queued` 而不是 `running`：作业行先存在、调度器随后才接手。这样"建出来了但
/// 一次请求都没发"这个窗口在库里有如实的状态，也能被后续的孤儿收口规则看见。
pub fn create_job(
    store: &Store,
    input_hash: &str,
    fingerprint: &str,
    versions: (u64, u64),
    limits: JobLimits,
    drafts: &[UnitDraft],
    capabilities: &ProviderCapabilities,
) -> Result<i64, String> {
    let limits_json = serde_json::to_string(&limits).map_err(err)?;
    let timestamp = now();
    in_transaction(store.conn(), |connection| {
        connection.execute(
            "INSERT INTO jobs(input_hash,provider_fingerprint,memory_version,glossary_version,status,limits_json,created_at,updated_at) VALUES(?1,?2,?3,?4,'queued',?5,?6,?6)",
            params![
                input_hash,
                fingerprint,
                versions.0 as i64,
                versions.1 as i64,
                limits_json,
                timestamp
            ],
        )?;
        let job_id = connection.last_insert_rowid();
        {
            let mut statement = connection.prepare(
                "INSERT INTO job_units(job_id,row_number,target_column,target_locale,source_locale,source_text,context,resource_key,signature,group_key,status,attempt,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'queued',0,?11)",
            )?;
            for draft in drafts {
                statement.execute(params![
                    job_id,
                    draft.row_number as i64,
                    draft.target_column as i64,
                    draft.target_locale.tag,
                    draft.source_locale.tag,
                    draft.source_text,
                    draft.context,
                    draft.resource_key,
                    draft.signature,
                    draft.group_key,
                    timestamp,
                ])?;
            }
        }
        connection.execute(
            "INSERT INTO job_usage(job_id,requests,input_tokens,output_tokens) VALUES(?1,0,0,0)",
            params![job_id],
        )?;
        connection.execute(
            "INSERT INTO job_pricing(job_id,price_input,price_output,currency) VALUES(?1,?2,?3,?4)",
            params![
                job_id,
                capabilities.price_per_million_input_tokens,
                capabilities.price_per_million_output_tokens,
                capabilities.currency,
            ],
        )?;
        Ok(job_id)
    })
}

/// 读取作业创建时锁定的参数，供草稿导出使用相同的行选择规则。
pub fn limits_for_job(store: &Store, job_id: i64) -> Result<JobLimits, String> {
    let json: String = store
        .conn()
        .query_row(
            "SELECT limits_json FROM jobs WHERE id=?1",
            [job_id],
            |row| row.get(0),
        )
        .map_err(err)?;
    serde_json::from_str(&json).map_err(err)
}

/// 作业是否可以接着跑。
///
/// 只有 `succeeded` 被排除：它的格都已结算，继续也只会空转。其余状态（含 `cancelled`）
/// 都可能有"尚未结算的格"——取消时那句"未完成的格保留为待处理"必须真的能兑现，
/// 否则用户只能新建作业，把已经付过费的译文整份重译一遍。
pub fn resumable_exact_job(
    store: &Store,
    job_id: i64,
    work_id: i64,
    input_hash: &str,
) -> Result<bool, String> {
    let found: Option<i64> = store.conn().query_row(
        "SELECT id FROM jobs WHERE id=?1 AND work_id=?2 AND input_hash=?3 AND status<>'succeeded'",
        params![job_id, work_id, input_hash], |row| row.get(0),
    ).optional().map_err(err)?;
    Ok(found.is_some())
}

/// 用户主动续跑时重试失败格。
pub fn requeue_failed_units(store: &Store, job_id: i64) -> Result<u64, String> {
    let changed = store.conn().execute(
        "UPDATE job_units SET status='queued',attempt=0,qa_state=NULL,updated_at=?2 WHERE job_id=?1 AND status='failed'",
        params![job_id, now()],
    ).map_err(err)?;
    Ok(changed as u64)
}

pub fn progress_for(store: &Store, job_id: i64) -> Result<JobProgress, String> {
    let status: String = store
        .conn()
        .query_row("SELECT status FROM jobs WHERE id=?1", [job_id], |row| {
            row.get(0)
        })
        .map_err(err)?;
    snapshot(store, job_id, job_status(&status), None)
}

/// 把上次进程遗留在 `running` 的格退回待处理。
///
/// 纯机械化流程不判断"上次到底发出去了没有"：退回去重发即可。代价是可能重复计费，
/// 收益是作业永远不会卡在等人工确认上——这正是本插件"全自动"的取舍。
pub fn requeue_stale_running(store: &Store, job_id: i64) -> Result<u64, String> {
    let changed = store
        .conn()
        .execute(
            "UPDATE job_units SET status='queued', attempt=0, updated_at=?2 WHERE job_id=?1 AND status='running'",
            params![job_id, now()],
        )
        .map_err(err)?;
    Ok(changed as u64)
}

/// 把库里遗留的 `running` 作业收成 `paused`。
///
/// 只有持有作业控制块的进程才算真的在跑：进程被杀、或作业因内部错误提前退出时，库里会留下
/// `running`。这种行会让界面显示"运行中"却又暂停不了——用户点暂停只会得到"当前没有进行中的
/// 翻译作业"，既看不出发生了什么，也没法收场。`paused` 是诚实的状态：不知道上次跑完没有，
/// 但用户可以点"继续"，续跑会把遗留在 `running` 的格重新排上。
pub fn pause_orphan_running(store: &Store) -> Result<u64, String> {
    let changed = store
        .conn()
        .execute(
            "UPDATE jobs SET status='paused', updated_at=?1 WHERE status='running'",
            params![now()],
        )
        .map_err(err)?;
    Ok(changed as u64)
}

/// 作业因内部错误提前退出时的兜底结算：把作业与未结算的格一起记为失败。
///
/// 这条路径不是"模型失败"而是"调度自己出错"，但结果必须同样明确：绝不能把任务留在
/// `running`。格记成 `failed` 而不是退回 `queued`，是为了让界面如实报出失败数；
/// "继续"照旧会把失败格重新排上（[`requeue_failed_units`]）。
pub fn settle_after_error(store: &Store, job_id: i64, reason: &str) -> Result<(), String> {
    let timestamp = now();
    in_transaction(store.conn(), |connection| {
        connection.execute(
            "UPDATE jobs SET status='failed', updated_at=?2 WHERE id=?1",
            params![job_id, timestamp],
        )?;
        connection.execute(
            "UPDATE job_units SET status='failed', qa_state=?2, updated_at=?3 WHERE job_id=?1 AND status IN ('queued','running')",
            params![job_id, reason, timestamp],
        )?;
        Ok(())
    })
}

#[derive(Default)]
struct JobPricing {
    input: Option<f64>,
    output: Option<f64>,
    currency: Option<String>,
}

fn cost_of(usage: (u64, u64, u64), pricing: JobPricing) -> (Option<f64>, Option<String>) {
    let (_, input_tokens, output_tokens) = usage;
    let (Some(price_in), Some(price_out)) = (pricing.input, pricing.output) else {
        return (None, pricing.currency);
    };
    let cost = input_tokens as f64 / 1_000_000.0 * price_in
        + output_tokens as f64 / 1_000_000.0 * price_out;
    (Some(cost), pricing.currency)
}

fn pricing_of(store: &Store, job_id: i64) -> Result<JobPricing, String> {
    store
        .conn()
        .query_row(
            "SELECT price_input,price_output,currency FROM job_pricing WHERE job_id=?1",
            [job_id],
            |row| {
                Ok(JobPricing {
                    input: row.get(0)?,
                    output: row.get(1)?,
                    currency: row.get(2)?,
                })
            },
        )
        .optional()
        .map(|value| value.unwrap_or_default())
        .map_err(err)
}

fn usage_of(store: &Store, job_id: i64) -> Result<(u64, u64, u64), String> {
    store
        .conn()
        .query_row(
            "SELECT requests,input_tokens,output_tokens FROM job_usage WHERE job_id=?1",
            params![job_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )
        .map_err(err)
}

/// 组装进度快照；计数与用量都从库里读，避免内存与磁盘不一致。
pub fn snapshot(
    store: &Store,
    job_id: i64,
    status: JobStatus,
    note: Option<String>,
) -> Result<JobProgress, String> {
    let counts = store
        .conn()
        .query_row(
            "SELECT COUNT(*), SUM(status='succeeded'), SUM(status='failed') FROM job_units WHERE job_id=?1",
            params![job_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64,
                    row.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
                ))
            },
        )
        .map_err(err)?;
    let usage = usage_of(store, job_id)?;
    let (estimated_cost, currency) = cost_of(usage, pricing_of(store, job_id)?);
    Ok(JobProgress {
        job_id,
        status,
        total: counts.0,
        done: counts.1,
        failed: counts.2,
        requests: usage.0,
        input_tokens: usage.1,
        output_tokens: usage.2,
        estimated_cost,
        currency,
        note,
    })
}

pub fn draft_lookup_job(
    store: &Store,
    job_id: i64,
    input_hash: &str,
) -> Result<DraftLookup, String> {
    let actual_hash: String = store
        .conn()
        .query_row("SELECT input_hash FROM jobs WHERE id=?1", [job_id], |row| {
            row.get(0)
        })
        .map_err(err)?;
    if actual_hash != input_hash {
        return Err("翻译任务与当前 CSV 不匹配".into());
    }
    let mut statement = store
        .conn()
        .prepare(
            "SELECT row_number,target_column,target_text,COALESCE(qa_state,'') FROM job_units WHERE job_id=?1 AND status='succeeded' AND target_text IS NOT NULL",
        )
        .map_err(err)?;
    let rows = statement
        .query_map(params![job_id], |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    Ok(rows
        .into_iter()
        .map(|(row_number, column, text, qa_state)| {
            (
                (row_number, column),
                DraftCell {
                    target_text: text,
                    qa_state,
                },
            )
        })
        .collect())
}

/// 请求速率闸门（令牌桶）。
///
/// 早先用的是"请求起点之间至少间隔 60/N 秒"，那等于把速率当成硬节拍：并发再高也只能
/// 每 60/N 秒发一个，`concurrency` 变成惰性参数。桶的做法是允许 `burst` 个请求立即起飞
/// （填满在飞管道），之后才按 N/分钟补充——这才是"上限"该有的语义。
struct RateLimiter {
    /// 每分钟上限；`None` 或 `0` 表示不设限。
    per_minute: Option<u32>,
    /// 桶容量，取并发数：管道能装多少，就允许多少个同时起步。
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    fn new(per_minute: Option<u32>, burst: usize) -> Self {
        let burst = burst.max(1) as f64;
        Self {
            per_minute: per_minute.filter(|value| *value > 0),
            burst,
            tokens: burst,
            last: Instant::now(),
        }
    }

    /// 按经过的时间补充令牌。
    fn refill(&mut self) {
        let Some(per_minute) = self.per_minute else {
            return;
        };
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * per_minute as f64 / 60.0).min(self.burst);
    }

    /// 取一个令牌，必要时等待。
    async fn acquire(&mut self) {
        loop {
            let Some(per_minute) = self.per_minute else {
                return;
            };
            self.refill();
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                return;
            }
            let wait = (1.0 - self.tokens) * 60.0 / per_minute as f64;
            tokio::time::sleep(Duration::from_secs_f64(wait.max(0.01))).await;
        }
    }
}

/// 内存中的去重组快照：一次派发所需的全部字段。
///
/// 组内所有格按定义共享源文、语境、资源键与目标语言，因此一份快照就够；
/// `row_number` 取组内最早一格的记录序号、`cells` 是组内格数，两者都只用于日志定位。
struct GroupSnapshot {
    group_key: String,
    row_number: u64,
    cells: u64,
    source_locale: String,
    target_locale: String,
    source_text: String,
    context: String,
}

/// 把一批去重组组装成一次批量请求。
///
/// 同批语言一致（[`take_next_batch`] 的选取条件），因此语言、词条与示例只在批级给一份；
/// 语境只在全批一致时给——否则会把某一行的语境当成整批的。
fn build_batch(groups: &[GroupSnapshot], sources: &ContextSources) -> BatchRequest {
    let target_locale = groups[0].target_locale.clone();
    let first_context = groups[0].context.trim();
    let context = groups
        .iter()
        .all(|group| group.context.trim() == first_context)
        .then_some(first_context)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let items = groups
        .iter()
        .enumerate()
        .map(|(index, group)| BatchItem {
            id: index as u32 + 1,
            source_text: group.source_text.clone(),
        })
        .collect();
    let constraints = sources
        .terms
        .get(&target_locale)
        .map(|terms| in_sentence_constraints(groups, terms))
        .unwrap_or_default();
    let examples = sources
        .examples
        .get(&target_locale)
        .cloned()
        .unwrap_or_default();
    BatchRequest {
        source_locale: Locale {
            tag: groups[0].source_locale.clone(),
        },
        target_locale: Locale { tag: target_locale },
        context,
        items,
        constraints,
        examples,
    }
}

/// 只取在**本批任一片段**里真实出现的启用词条：普通词给译名，不可译词给"照抄"。
///
/// 同一条词条在批内出现多次也只给一次约束：重复条目只会撑大提示词。禁用译法不进提示词
/// ——它约束的是"不许出现的写法"，把错误答案提前写进提示词反而容易把它招来。
fn in_sentence_constraints(
    groups: &[GroupSnapshot],
    terms: &[GlossaryTerm],
) -> Vec<TermConstraint> {
    let mut constraints = Vec::new();
    for term in terms.iter().filter(|term| {
        term.enabled
            && term.kind != crate::TermKind::Forbidden
            && (term.kind == crate::TermKind::DoNotTranslate || !term.target_text.is_empty())
    }) {
        let hit = groups.iter().any(|group| {
            std::iter::once(&term.source_text)
                .chain(term.aliases.iter())
                .any(|surface| !surface.is_empty() && group.source_text.contains(surface.as_str()))
        });
        if !hit {
            continue;
        }
        let target_text = match term.kind {
            crate::TermKind::DoNotTranslate => None,
            _ => Some(term.target_text.clone()),
        };
        constraints.push(TermConstraint {
            source_surface: term.source_text.clone(),
            target_text,
        });
    }
    constraints
}

/// 取下一批待发的去重组，并立即把批内所有待发格标 `running`；返回空表示没有待发任务。
///
/// 批内只装源语言与目标语言相同的组：同批共用一份提示词头，串了语言就会得到自相矛盾的提示。
/// `source_budget` 是批内源文总量的**粗筛**上限：超出的组留到下一批，免得提示词被源文挤爆。
/// 真正的定稿由调用方用 [`fit_items`] 按渲染结果做——源文字符数只是一个上界估计。
///
/// 标记整组而不只是代表格，是为了让 `running` 语义保持"这一批正在飞"：进程中断时
/// [`requeue_stale_running`] 才能把批内每一格都退回待处理，而不是只退代表格。
fn take_next_batch(
    store: &Store,
    job_id: i64,
    size: usize,
    source_budget: usize,
) -> Result<Vec<GroupSnapshot>, String> {
    in_transaction(store.conn(), |connection| {
        let first: Option<(String, String)> = connection
            .query_row(
                "SELECT source_locale, target_locale FROM job_units WHERE job_id=?1 AND status='queued' ORDER BY id LIMIT 1",
                params![job_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((source_locale, target_locale)) = first else {
            return Ok(Vec::new());
        };
        let candidates: Vec<(String, i64, i64, i64, String, String)> = {
            let mut statement = connection.prepare(
                "SELECT group_key, MIN(id), MIN(row_number), COUNT(*), source_text, context FROM job_units WHERE job_id=?1 AND status='queued' AND source_locale=?2 AND target_locale=?3 GROUP BY group_key ORDER BY MIN(id) LIMIT ?4",
            )?;
            statement
                .query_map(
                    params![job_id, source_locale, target_locale, size as i64],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut groups = Vec::new();
        let mut chars = 0usize;
        for (group_key, unit_id, row_number, cells, source_text, context) in candidates {
            let width = source_text.chars().count();
            if !groups.is_empty() && chars + width > source_budget {
                break;
            }
            chars += width;
            groups.push((
                GroupSnapshot {
                    group_key,
                    row_number: row_number as u64,
                    cells: cells as u64,
                    source_locale: source_locale.clone(),
                    target_locale: target_locale.clone(),
                    source_text,
                    context,
                },
                unit_id,
            ));
        }
        for (group, _) in &groups {
            connection.execute(
                "UPDATE job_units SET status='running', attempt=attempt+1, updated_at=?3 WHERE job_id=?1 AND group_key=?2 AND status='queued'",
                params![job_id, group.group_key, now()],
            )?;
        }
        Ok(groups.into_iter().map(|(group, _)| group).collect())
    })
}

/// 结算一批：结果与 QA 结论同一事务扇出到批内每一格，并累计用量。
///
/// `requests` 是这一批真实发出的请求次数（含重试）：请求数按它入账，失败也算，
/// 这样界面上的 `requests` 才是完整的网络调用审计；token 只认成功响应里的 `usage`。
fn commit_batch(
    store: &Store,
    job_id: i64,
    settled: &[(String, Result<String, String>)],
    requests: u64,
    usage: (Option<u64>, Option<u64>),
) -> Result<(), String> {
    in_transaction(store.conn(), |connection| {
        for (group_key, outcome) in settled {
            match outcome {
                Ok(text) => {
                    // 同组的源文一致，QA 结论也就一致；取组内一格判定即可。
                    let source: String = connection.query_row(
                        "SELECT source_text FROM job_units WHERE job_id=?1 AND group_key=?2 AND status='running' ORDER BY id LIMIT 1",
                        params![job_id, group_key],
                        |row| row.get(0),
                    )?;
                    let qa_state = if safe_translation(&source, text) {
                        "ok"
                    } else {
                        "blocking"
                    };
                    connection.execute(
                        "UPDATE job_units SET status='succeeded', target_text=?3, qa_state=?4, updated_at=?5 WHERE job_id=?1 AND group_key=?2 AND status='running'",
                        params![job_id, group_key, text, qa_state, now()],
                    )?;
                }
                Err(reason) => {
                    connection.execute(
                        "UPDATE job_units SET status='failed', qa_state=?3, updated_at=?4 WHERE job_id=?1 AND group_key=?2 AND status='running'",
                        params![job_id, group_key, reason, now()],
                    )?;
                }
            }
        }
        add_usage(connection, job_id, requests, usage)?;
        Ok(())
    })
}

/// 累计网络调用与 token 用量。
///
/// 请求数按**真实发出的次数**入账：失败也算，服务不可用同样算——那些调用确实打出去了，
/// 只有把它们记上，界面上的 `requests` 才是一份完整的网络调用审计。
fn add_usage(
    connection: &Connection,
    job_id: i64,
    requests: u64,
    usage: (Option<u64>, Option<u64>),
) -> Result<(), rusqlite::Error> {
    connection.execute(
        "UPDATE job_usage SET requests=requests+?2, input_tokens=input_tokens+?3, output_tokens=output_tokens+?4 WHERE job_id=?1",
        params![
            job_id,
            requests as i64,
            usage.0.unwrap_or(0) as i64,
            usage.1.unwrap_or(0) as i64
        ],
    )?;
    Ok(())
}

fn set_job_status(store: &Store, job_id: i64, status: JobStatus) -> Result<(), String> {
    store
        .conn()
        .execute(
            "UPDATE jobs SET status=?2, updated_at=?3 WHERE id=?1",
            params![job_id, job_status_str(status), now()],
        )
        .map_err(err)?;
    Ok(())
}

/// 把已经取出但还没发出去的整批格退回待处理。
///
/// 用于"取了任务但还没发请求就被暂停/取消"的窗口：这一批并没有花钱，标成 `running`
/// 反而会让下次启动把它误判成"上一轮发过"。`attempt` 也要退回去，否则重试次数被白扣。
fn release_group(store: &Store, job_id: i64, group_key: &str) -> Result<(), String> {
    store
        .conn()
        .execute(
            "UPDATE job_units SET status='queued', attempt=MAX(attempt-1,0), updated_at=?3 WHERE job_id=?1 AND group_key=?2 AND status='running'",
            params![job_id, group_key, now()],
        )
        .map_err(err)?;
    Ok(())
}

/// 结算一次批量调用的结果，返回要写进日志的行与作业接下来的走向。
///
/// 三种结果对应三种处置：正常结算只落库；**服务不可用**把整批退回待处理（不记失败——
/// 这些格没有任何问题，只是现在翻不了）；**配置错误**把整批记为失败，因为重试也无解。
fn settle_batch(
    store: &Store,
    job_id: i64,
    keys: Vec<String>,
    labels: Vec<(u64, String, u64)>,
    outcome: BatchOutcome,
    attempts: u64,
    usage: (Option<u64>, Option<u64>),
) -> Result<(Vec<String>, SettleResult), String> {
    let items = match outcome {
        BatchOutcome::Items(items) => items,
        BatchOutcome::Unavailable(reason) => {
            for key in &keys {
                release_group(store, job_id, key)?;
            }
            // 请求确实发出去了（只是没成功），审计口径照实入账。
            add_usage(store.conn(), job_id, attempts, usage).map_err(err)?;
            let mut lines: Vec<String> = labels
                .iter()
                .map(|(row, locale, cells)| {
                    format!("第 {row} 行 · {locale} 暂未翻译（{cells} 格）")
                })
                .collect();
            lines.push(format!(
                "{reason}：本批片段已退回待处理，点“继续”即可接着跑"
            ));
            return Ok((lines, SettleResult::Unavailable(reason)));
        }
        BatchOutcome::Fatal(reason) => {
            let settled: Vec<_> = keys
                .iter()
                .map(|key| (key.clone(), Err(reason.clone())))
                .collect();
            commit_batch(store, job_id, &settled, attempts, usage)?;
            let mut lines: Vec<String> = labels
                .iter()
                .map(|(row, locale, cells)| {
                    format!("第 {row} 行 · {locale} 未完成（{cells} 格）：{reason}")
                })
                .collect();
            lines.push(reason.clone());
            return Ok((lines, SettleResult::Fatal(reason)));
        }
    };
    let settled: Vec<(String, Result<String, String>)> = keys.into_iter().zip(items).collect();
    commit_batch(store, job_id, &settled, attempts, usage)?;
    // 逐格给出模型返回的译文或失败原因；成功行正是"日志里能看到 LLM 输出"的那一半。
    // 日志按去重片段记（一行可能覆盖多格），因此每行都带上它实际覆盖的格数。
    let mut lines: Vec<String> = Vec::with_capacity(settled.len() + 1);
    let mut failed = 0u64;
    let mut cells = 0u64;
    for ((_, result), (row, locale, group_cells)) in settled.iter().zip(labels.iter()) {
        cells += group_cells;
        match result {
            Ok(text) => lines.push(format!(
                "第 {row} 行 · {locale} 完成（{group_cells} 格）：{}",
                log_excerpt(text)
            )),
            Err(reason) => {
                failed += 1;
                lines.push(format!(
                    "第 {row} 行 · {locale} 失败（{group_cells} 格）：{reason}"
                ));
            }
        }
    }
    lines.push(format!(
        "批次完成：片段 成功 {}、失败 {failed}，共结算 {cells} 格",
        settled.len() as u64 - failed
    ));
    Ok((lines, SettleResult::Settled))
}

/// 发一次批量请求，按错误分类决定重试；同时报出这一批真实发出的请求次数。
///
/// 三条出口分得很清：**结果不确定**按退避重发，用尽后整批记为失败（重试是机械的，不等人）；
/// **服务不可用**用尽重试也不记失败，而是把整批退回待处理（见 [`BatchOutcome::Unavailable`]）；
/// **配置错误**重试无用，直接结束作业。
async fn run_batch(
    provider: Arc<dyn TranslatorProvider>,
    request: BatchRequest,
) -> (BatchOutcome, u64, (Option<u64>, Option<u64>)) {
    let count = request.items.len();
    let mut attempt = 0u32;
    loop {
        // 这一次调用已经发生，不管结果如何都计入请求数；token 另按成功响应记。
        let attempts = attempt as u64 + 1;
        match provider.translate_batch(request.clone()).await {
            Ok(response) => {
                let mut by_id: HashMap<u32, String> = response
                    .items
                    .into_iter()
                    .map(|item| (item.id, item.target_text))
                    .collect();
                let items = (1..=count as u32)
                    .map(|id| {
                        by_id
                            .remove(&id)
                            .ok_or_else(|| "模型响应缺少该片段".to_owned())
                    })
                    .collect();
                return (
                    BatchOutcome::Items(items),
                    attempts,
                    (response.input_tokens, response.output_tokens),
                );
            }
            Err(error) if retryable(&error) && attempt < MAX_RETRIES => {
                let delay = match error {
                    ProviderError::Transient { retry_after, .. } => {
                        retry_after.unwrap_or(Duration::from_millis(500 * (1 << attempt)))
                    }
                    _ => Duration::from_millis(500 * (1 << attempt)),
                };
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            // 重试次数用尽仍是临时失败：服务整体不可用。继续跑只会白花钱，
            // 但也绝不能把这些格记成失败——它们什么问题都没有。
            Err(ProviderError::Transient { .. }) => {
                return (
                    BatchOutcome::Unavailable("模型服务持续不可用（连接失败、超时或限流）".into()),
                    attempts,
                    (None, None),
                );
            }
            Err(ProviderError::Indeterminate) => {
                return (
                    BatchOutcome::Items(
                        (0..count)
                            .map(|_| Err("请求结果无法确认，已重试用尽".to_owned()))
                            .collect(),
                    ),
                    attempts,
                    (None, None),
                );
            }
            Err(ProviderError::Invalid(reason)) => {
                return (
                    BatchOutcome::Items((0..count).map(|_| Err(reason.clone())).collect()),
                    attempts,
                    (None, None),
                );
            }
            Err(ProviderError::Config(reason)) => {
                return (
                    BatchOutcome::Fatal(format!("模型配置无效：{reason}")),
                    attempts,
                    (None, None),
                );
            }
        }
    }
}

/// 是否值得重发：临时失败与结果不确定都重试，其余重发只会重复同样的失败。
fn retryable(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Transient { .. } | ProviderError::Indeterminate
    )
}

/// 日志一行一条、且有长度上限：真实换行与控制字符转成可见转义，超长译文截断。
///
/// 完整译文在表格与该任务的草稿导出里都有；日志只负责回答"模型这次回了什么"，
/// 因此不必（也不该）把 1 MiB 的单格原文整段搬进界面状态。
fn log_excerpt(text: &str) -> String {
    const LIMIT: usize = 500;
    let flattened = text
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    if flattened.chars().count() <= LIMIT {
        return flattened;
    }
    let head: String = flattened.chars().take(LIMIT).collect();
    format!("{head}…（已截断，共 {} 字符）", flattened.chars().count())
}

/// 执行（或续跑）一个作业，直到跑完、暂停、取消或整体失败。
///
/// `progress` 按 [`PROGRESS_INTERVAL`] 节流回调，调用方把它转成事件推给界面；
/// `log` 按批回调若干行可读日志（含模型返回的译文），一次调用就是一个事件，
/// 免得大批量作业把 IPC 刷成每格一条。
#[allow(clippy::too_many_arguments)]
pub async fn run(
    store: Store,
    provider: Arc<dyn TranslatorProvider>,
    job_id: i64,
    limits: JobLimits,
    sources: ContextSources,
    control: Arc<JobControl>,
    mut progress: impl FnMut(JobProgress) + Send,
    mut log: impl FnMut(Vec<String>) + Send,
) -> Result<JobProgress, String> {
    let capabilities = provider.capabilities();
    // 并发上限足够高，让"服务端才是限流权威"这条原则成立；过低的上限才是真的卡住吞吐。
    let concurrency = limits.concurrency.clamp(1, MAX_CONCURRENCY) as usize;
    let batch_cap = capabilities.max_batch_units.unwrap_or(1).max(1) as usize;
    let batch_size = if capabilities.supports_batch {
        (limits.batch_size.max(1) as usize).min(batch_cap)
    } else {
        1
    };
    let prompt_limit = capabilities.max_input_chars.unwrap_or(MAX_INPUT_CHARS) as usize;
    // 源文预算只是选批的粗筛：先留出指令与约束的位置，再由 fit_items 用真实渲染结果定稿。
    let source_budget = prompt_limit
        .saturating_sub(PROMPT_OVERHEAD_RESERVE)
        .max(MIN_SOURCE_BUDGET);
    let mut limiter = RateLimiter::new(capabilities.requests_per_minute, concurrency);

    set_job_status(&store, job_id, JobStatus::Running)?;
    let mut status = JobStatus::Running;
    let mut note: Option<String> = None;
    let mut last_emit: Option<Instant> = None;
    let mut inflight: JoinSet<InflightResult> = JoinSet::new();

    log(vec![format!(
        "作业 #{job_id} 开始：每批最多 {batch_size} 个片段、源文 {source_budget} 字符，并发 {concurrency}"
    )]);

    loop {
        if control.is_cancelled() {
            status = JobStatus::Cancelled;
            note = Some("已取消，未完成的格保留为待处理".into());
            log(vec!["已取消，未完成的格保留为待处理".into()]);
            break;
        }
        if control.is_paused() {
            status = JobStatus::Paused;
            note = Some("已暂停，已发出的请求已收尾".into());
            log(vec!["已暂停，已发出的请求已收尾".into()]);
            break;
        }
        // 预算按已累计用量判定：触顶就暂停而不是继续花钱。
        let usage = usage_of(&store, job_id)?;
        if over_budget(limits, usage) {
            status = JobStatus::Paused;
            note = Some("已达预算上限，请调整预算后继续".into());
            log(vec!["已达预算上限，已停止派发".into()]);
            break;
        }

        // 并发数约束的是"同时在飞的批量请求数"。
        'dispatch: while inflight.len() < concurrency {
            let mut groups = take_next_batch(&store, job_id, batch_size, source_budget)?;
            if groups.is_empty() {
                break;
            }
            limiter.acquire().await;
            // 等令牌期间可能被暂停/取消：把已取的批退回待处理，绝不把这一发打出去。
            if control.is_cancelled() || control.is_paused() {
                for group in &groups {
                    release_group(&store, job_id, &group.group_key)?;
                }
                break 'dispatch;
            }
            // 片段数与源文字符数都只是粗筛：一条超长源文就能顶满提示词。这里用真实渲染
            // 结果定稿——装不下只能少装尾部几条，绝不截断任何一条源文。
            let fit = fit_items(&build_batch(&groups, &sources), prompt_limit);
            if fit < groups.len() {
                // 退回的组与"取了没发"同样处置：状态回待处理、attempt 退回，下次仍是完整预算。
                for group in &groups[fit..] {
                    release_group(&store, job_id, &group.group_key)?;
                }
                groups.truncate(fit);
            }
            let request = build_batch(&groups, &sources);
            let keys: Vec<String> = groups.iter().map(|g| g.group_key.clone()).collect();
            let labels: Vec<(u64, String, u64)> = groups
                .iter()
                .map(|g| (g.row_number, g.target_locale.clone(), g.cells))
                .collect();
            let provider = Arc::clone(&provider);
            inflight.spawn(async move {
                let (outcome, attempts, usage) = run_batch(provider, request).await;
                (keys, labels, outcome, attempts, usage)
            });
        }

        // 退回待处理的批说明用户已经按下暂停或取消：回到循环顶部按其意图收口，
        // 不要在"没有在飞请求"的兜底分支里落一个含混的说明。
        if control.is_cancelled() || control.is_paused() {
            continue;
        }
        if inflight.is_empty() {
            break;
        }

        let Some(joined) = inflight.join_next().await else {
            break;
        };
        // 子任务 panic 时它的组还停在 running，这里认领不了：留一句日志，交给作业收尾的
        // "仍有 N 个待处理格"兜住，续跑会把它们退回待处理。
        let (keys, labels, outcome, attempts, usage) = match joined {
            Ok(value) => value,
            Err(error) => {
                log(vec![format!(
                    "一个批次异常结束（{error}），它的片段留待续跑重排"
                )]);
                continue;
            }
        };
        let (lines, result) = settle_batch(&store, job_id, keys, labels, outcome, attempts, usage)?;
        log(lines);
        match result {
            SettleResult::Settled => {}
            SettleResult::Fatal(reason) => {
                status = JobStatus::Failed;
                note = Some(reason);
                break;
            }
            SettleResult::Unavailable(reason) => {
                // 服务不可用不是这些格的错：整批已退回待处理，作业停在可续跑的暂停态。
                status = JobStatus::Paused;
                note = Some(format!(
                    "{reason}；未完成的格保留为待处理，稍后点“继续”接着跑"
                ));
                break;
            }
        }
        if last_emit.is_none_or(|at: Instant| at.elapsed() >= PROGRESS_INTERVAL) {
            progress(snapshot(&store, job_id, JobStatus::Running, None)?);
            last_emit = Some(Instant::now());
        }
    }

    // 收尾：暂停或取消都不打断已发出的请求，但它们的结果仍要落库。
    while let Some(joined) = inflight.join_next().await {
        if let Ok((keys, labels, outcome, attempts, usage)) = joined {
            let (lines, _) = settle_batch(&store, job_id, keys, labels, outcome, attempts, usage)?;
            log(lines);
        }
    }

    if status == JobStatus::Running {
        let remaining: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM job_units WHERE job_id=?1 AND status IN ('queued','running')",
                params![job_id],
                |row| row.get(0),
            )
            .map_err(err)?;
        if remaining > 0 {
            // 只在"没有可派发的"情况下退出循环，因此这里几乎不会发生；留作防线。
            note = Some(format!("仍有 {remaining} 个待处理格"));
            status = JobStatus::Paused;
        } else {
            let failed: i64 = store
                .conn()
                .query_row(
                    "SELECT COALESCE(SUM(status='failed'),0) FROM job_units WHERE job_id=?1",
                    [job_id],
                    |row| row.get(0),
                )
                .map_err(err)?;
            status = if failed > 0 {
                JobStatus::Failed
            } else {
                JobStatus::Succeeded
            };
        }
    }

    set_job_status(&store, job_id, status)?;
    let final_progress = snapshot(&store, job_id, status, note)?;
    // 失败明确报数，绝不在界面上冒充"全部成功"。
    let final_progress = if final_progress.failed > 0 && final_progress.note.is_none() {
        JobProgress {
            note: Some(format!("{} 个格失败，可续跑重试", final_progress.failed)),
            ..final_progress
        }
    } else {
        final_progress
    };
    log(vec![format!(
        "作业 #{} 结束：{}，完成 {}/{} 格，失败 {} 格",
        final_progress.job_id,
        status_cn(final_progress.status),
        final_progress.done,
        final_progress.total,
        final_progress.failed
    )]);
    progress(final_progress.clone());
    Ok(final_progress)
}

fn over_budget(limits: JobLimits, usage: (u64, u64, u64)) -> bool {
    let (_, input_tokens, output_tokens) = usage;
    (limits.input_token_budget > 0 && input_tokens >= limits.input_token_budget)
        || (limits.output_token_budget > 0 && output_tokens >= limits.output_token_budget)
}

#[cfg(test)]
mod tests {
    use super::RateLimiter;
    use std::time::{Duration, Instant};

    /// 不设限时必须完全不等待：这是"不替用户设闸门"的直接断言。
    #[tokio::test]
    async fn unlimited_rate_never_waits() {
        let mut limiter = RateLimiter::new(None, 4);
        let start = Instant::now();
        for _ in 0..20 {
            limiter.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    /// 桶容量内的请求立即放行，之后的请求按配置速率排队。
    ///
    /// 这正是旧实现缺的那一半：旧代码把起点间隔固定成 60/N 秒，并发再多也只能一个个发。
    #[tokio::test]
    async fn burst_passes_then_paces_at_configured_rate() {
        // 60 次/分钟 = 每秒 1 个；桶容量 2 允许两个请求立即起飞。
        let mut limiter = RateLimiter::new(Some(60), 2);
        let start = Instant::now();
        limiter.acquire().await;
        limiter.acquire().await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "桶内应直接放行"
        );
        limiter.acquire().await;
        assert!(
            start.elapsed() >= Duration::from_millis(900),
            "第三个请求要等令牌补充"
        );
    }

    /// `Some(0)` 与 `None` 同义：界面把 0 当作"不限制"。
    #[tokio::test]
    async fn zero_rate_means_unlimited() {
        let mut limiter = RateLimiter::new(Some(0), 2);
        let start = Instant::now();
        for _ in 0..10 {
            limiter.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_millis(100));
    }
}
