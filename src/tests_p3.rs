//! P3 用例：模型适配器、作业调度与失败路径。
//!
//! 全部用本机假 HTTP 服务驱动，不访问真实模型端点。覆盖文档「P3 测试清单」的条目：
//! 正常响应与缺用量、429 退避、超时、无效 JSON、错语言、响应体截断、连接失败、
//! 进程中断后的不确定状态、暂停、取消与预算触顶。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::ProviderSettings;
use crate::engine;
use crate::job::{self, ContextSources, JobControl};
use crate::provider::{ModelTransport, OpenAiCompatibleProvider, ProviderError, TransportFuture, TranslatorProvider};
use crate::store::Store;
use crate::tests::TestDir;
use crate::{JobLimits, JobStatus, Locale, TargetColumn};

// ---------------------------------------------------------------------------
// 假 HTTP 服务
// ---------------------------------------------------------------------------

/// 一次预置的响应。
#[derive(Clone)]
enum Reply {
    /// 完整响应；`delay_ms` 用于制造超时。
    Json {
        status: u16,
        body: String,
        headers: Vec<(&'static str, String)>,
        delay_ms: u64,
    },
    /// 声明了更长的 `content-length` 就断开：客户端读响应体中途失败。
    Partial { declared: usize, body: String },
}

impl Reply {
    fn ok(body: &str) -> Self {
        Self::Json {
            status: 200,
            body: body.to_owned(),
            headers: Vec::new(),
            delay_ms: 0,
        }
    }
    fn status(status: u16, body: &str) -> Self {
        Self::Json {
            status,
            body: body.to_owned(),
            headers: Vec::new(),
            delay_ms: 0,
        }
    }
    fn header(mut self, name: &'static str, value: &str) -> Self {
        if let Self::Json { headers, .. } = &mut self {
            headers.push((name, value.to_owned()));
        }
        self
    }
    fn delayed(mut self, delay_ms: u64) -> Self {
        if let Self::Json {
            delay_ms: value, ..
        } = &mut self
        {
            *value = delay_ms;
        }
        self
    }
}

/// 按顺序返回预置响应的假服务；请求数与同时在飞连接数的峰值可断言。
struct FakeServer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    /// 历史峰值：同一时刻最多有几个请求在处理。用来证明并发真的生效。
    peak: Arc<AtomicUsize>,
    /// 收到的请求体（JSON 字符串）。提示词的内容只能从这里回看。
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let peak = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let replies = Arc::new(replies);
        let index = Arc::new(std::sync::Mutex::new(0usize));
        let live = Arc::new(AtomicUsize::new(0));
        let peak_counter = Arc::clone(&peak);
        let bodies_recorder = Arc::clone(&bodies);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let replies = Arc::clone(&replies);
                let index = Arc::clone(&index);
                let live = Arc::clone(&live);
                let peak = Arc::clone(&peak_counter);
                let bodies = Arc::clone(&bodies_recorder);
                // 每个连接各起一个任务：慢响应不该挡住后续重试连接的接收，
                // 否则"超时后重试了几次"这件事在测试里根本观测不到。
                tokio::spawn(async move {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    let raw = drain_request(&mut socket).await.unwrap_or_default();
                    if let Some(body) = request_body(&raw) {
                        bodies.lock().unwrap().push(body);
                    }
                    // 用完之后一直重复最后一条：重试用例不必逐个列出响应。
                    let reply = {
                        let mut position = index.lock().unwrap();
                        let reply = replies.get(*position).or_else(|| replies.last()).cloned();
                        *position += 1;
                        reply
                    };
                    if let Some(reply) = reply {
                        let _ = write_reply(&mut socket, reply).await;
                    }
                    live.fetch_sub(1, Ordering::SeqCst);
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            addr,
            hits,
            peak,
            bodies,
            task,
        }
    }

    /// 形如 `http://127.0.0.1:PORT/v1` 的基础地址。
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// 收到的请求体，按到达顺序。
    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 从原始请求字节里取 JSON 请求体。用例只断言拿得到的内容，取不到就返回 `None`。
fn request_body(raw: &[u8]) -> Option<String> {
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    Some(String::from_utf8_lossy(&raw[split..]).into_owned())
}

/// 读掉一个完整请求（含 `content-length` 指定的请求体），返回原始字节。
async fn drain_request(socket: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(Vec::new());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut remaining = length.saturating_sub(buffer.len() - header_end);
    while remaining > 0 {
        let size = remaining.min(chunk.len());
        let read = socket.read(&mut chunk[..size]).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        remaining -= read;
    }
    buffer.truncate(header_end + length);
    Ok(buffer)
}

async fn write_reply(socket: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    match reply {
        Reply::Json {
            status,
            body,
            headers,
            delay_ms,
        } => {
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            let mut head = format!(
                "HTTP/1.1 {status} {}\r\ncontent-length: {}\r\nconnection: close\r\n",
                reason(status),
                body.len()
            );
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(body.as_bytes()).await
        }
        Reply::Partial { declared, body } => {
            // 声明得比实际长，客户端会在读响应体时失败。
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {declared}\r\nconnection: close\r\n\r\n"
            );
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(body.as_bytes()).await
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

// ---------------------------------------------------------------------------
// 夹具与运行辅助
// ---------------------------------------------------------------------------

/// 一次合法的批量模型响应：`translations` 按序号回填，外加回显的目标语言。
fn batch_body(items: &[(u32, &str)], target: &str, usage: Option<(u64, u64)>) -> String {
    let usage = match usage {
        Some((prompt, completion)) => {
            format!(",\"usage\":{{\"prompt_tokens\":{prompt},\"completion_tokens\":{completion}}}")
        }
        None => String::new(),
    };
    let entries = items
        .iter()
        .map(|(id, text)| {
            format!(
                "{{\"id\":{id},\"translation\":{}}}",
                serde_json::to_string(text).unwrap()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let inner = format!("{{\"translations\":[{entries}],\"target\":\"{target}\"}}");
    format!(
        "{{\"choices\":[{{\"message\":{{\"content\":{}}}}}]{} }}",
        serde_json::to_string(&inner).unwrap(),
        usage
    )
}

/// 单片段响应：多数用例一次只打包一个片段。
fn model_body(translation: &str, target: &str, usage: Option<(u64, u64)>) -> String {
    batch_body(&[(1, translation)], target, usage)
}

fn limits(concurrency: u32) -> JobLimits {
    JobLimits {
        include_false_rows: false,
        concurrency,
        batch_size: 1,
        input_token_budget: 0,
        output_token_budget: 0,
    }
}

/// 两列 CSV（`Source,English`），只映射源文与英语目标列。
fn prepare(dir: &TestDir, rows: &str) -> engine::Prepared {
    let path = dir.write("job.csv", format!("Source,English\n{rows}").as_bytes());
    let mut mapping = crate::csvio::inspect(&path).unwrap().default_mapping;
    mapping.source_text_column = 0;
    mapping.selection_column = None;
    mapping.context_column = None;
    mapping.resource_key_column = None;
    mapping.target_columns = vec![TargetColumn {
        column: 1,
        locale: Locale { tag: "en".into() },
    }];
    let mut store = Store::open(dir.path()).unwrap();
    engine::prepare_with_store(
        &path,
        dir.path(),
        mapping,
        &AtomicBool::new(false),
        Some(&mut store),
    )
    .unwrap()
}

fn settings(base_url: &str, timeout_seconds: u32) -> ProviderSettings {
    ProviderSettings {
        base_url: base_url.to_owned(),
        model: "test-model".into(),
        allow_loopback: true,
        timeout_seconds,
        // 测试一律不设速率闸门：本用例集考察的是调度与错误分类，不是限速。
        requests_per_minute: None,
        price_per_million_input_tokens: Some(1.0),
        price_per_million_output_tokens: Some(2.0),
        currency: Some("USD".into()),
    }
}

fn provider(base_url: &str, timeout_seconds: u32) -> Arc<dyn TranslatorProvider> {
    Arc::new(
        OpenAiCompatibleProvider::new(&settings(base_url, timeout_seconds), 1, Arc::new(DirectModelTransport)).unwrap(),
    )
}

struct DirectModelTransport;

impl ModelTransport for DirectModelTransport {
    fn post_json<'a>(&'a self, base_url: &'a str, allow_loopback: bool, timeout_seconds: u32, _secret_id: i64, path: &'a str, body: &'a str) -> TransportFuture<'a> {
        Box::pin(async move {
            let endpoint = wonderland_net::ModelEndpoint {
                base_url: base_url.to_owned(),
                timeout: std::time::Duration::from_secs(timeout_seconds.into()),
                allow_loopback,
                ..wonderland_net::ModelEndpoint::new(base_url)
            };
            let client = wonderland_net::ModelClient::new(endpoint).map_err(test_model_error)?;
            client.post_json(path, "test-key", body).await.map_err(test_model_error)
        })
    }
}

fn test_model_error(error: wonderland_net::ModelError) -> ProviderError {
    match error {
        wonderland_net::ModelError::Transient { status, retry_after } => ProviderError::Transient { status, retry_after },
        wonderland_net::ModelError::Indeterminate => ProviderError::Indeterminate,
        wonderland_net::ModelError::Invalid(reason) => ProviderError::Invalid(reason),
        wonderland_net::ModelError::Config(reason) => ProviderError::Config(reason),
    }
}

/// 建一次新作业，返回作业 id。
fn create_job(dir: &TestDir, prepared: &engine::Prepared, limits: JobLimits) -> i64 {
    let store = Store::open(dir.path()).unwrap();
    let drafts =
        engine::collect_units(prepared, Some(&store), &AtomicBool::new(false), false).unwrap();
    job::create_job(
        &store,
        &prepared.preflight.source_version.sha256,
        "fingerprint",
        (
            prepared.preflight.memory_version,
            prepared.preflight.glossary_version,
        ),
        limits,
        &drafts,
        &settings("http://127.0.0.1:1/v1", 30).capabilities(),
    )
    .unwrap()
}

async fn run_job(
    dir: &TestDir,
    adapter: Arc<dyn TranslatorProvider>,
    job_id: i64,
    limits: JobLimits,
    sources: ContextSources,
    control: Arc<JobControl>,
    progress: impl FnMut(crate::JobProgress) + Send,
) -> crate::JobProgress {
    let store = Store::open(dir.path()).unwrap();
    job::run(
        store,
        adapter,
        job_id,
        limits,
        sources,
        control,
        progress,
        |_| {},
    )
    .await
    .unwrap()
}

/// 逐格读回 `(记录序号, 状态, 译文, QA 结论)`。
fn unit_rows(dir: &TestDir) -> Vec<(i64, String, String, String)> {
    let store = Store::open(dir.path()).unwrap();
    let mut statement = store
        .conn()
        .prepare("SELECT row_number,status,COALESCE(target_text,''),COALESCE(qa_state,'') FROM job_units ORDER BY row_number")
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn usage_of(dir: &TestDir, job_id: i64) -> (u64, u64, u64) {
    let store = Store::open(dir.path()).unwrap();
    store
        .conn()
        .query_row(
            "SELECT requests,input_tokens,output_tokens FROM job_usage WHERE job_id=?1",
            [job_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )
        .unwrap()
}

fn queued_count(dir: &TestDir) -> usize {
    unit_rows(dir)
        .iter()
        .filter(|row| row.1 == "queued")
        .count()
}

/// 导出 CSV 的数据记录（不含表头）。
fn exported_rows(path: &str) -> Vec<csv::StringRecord> {
    let mut reader = crate::csvio::open_reader(Path::new(path)).unwrap();
    crate::csvio::headers(&mut reader).unwrap();
    reader.records().map(Result::unwrap).collect()
}

fn export_with(
    dir: &TestDir,
    prepared: &engine::Prepared,
    mode: engine::ExportMode,
) -> crate::TranslationExport {
    let store = Store::open(dir.path()).unwrap();
    engine::export_with_store(
        prepared,
        dir.path(),
        &AtomicBool::new(false),
        Some(&store),
        mode,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[tokio::test]
async fn job_succeeds_and_records_usage_and_cost() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n");
    let server = FakeServer::start(vec![Reply::ok(&model_body(
        "First line",
        "en",
        Some((120, 30)),
    ))])
    .await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.status, JobStatus::Succeeded);
    assert_eq!((progress.total, progress.done, progress.failed), (1, 1, 0));
    assert_eq!(progress.requests, 1);
    assert_eq!((progress.input_tokens, progress.output_tokens), (120, 30));
    // 用量与费用都按服务返回值记账，不在本地按字数猜。
    let expected = 120.0 / 1_000_000.0 + 30.0 / 1_000_000.0 * 2.0;
    assert!((progress.estimated_cost.unwrap() - expected).abs() < 1e-12);
    assert_eq!(progress.currency.as_deref(), Some("USD"));
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    let rows = unit_rows(&dir);
    assert_eq!(rows[0].1, "succeeded");
    assert_eq!(rows[0].2, "First line");
    assert_eq!(rows[0].3, "ok");
    assert_eq!(usage_of(&dir, job_id), (1, 120, 30));
    let store = Store::open(dir.path()).unwrap();
    let mut changed = settings(&server.base_url(), 30);
    changed.price_per_million_input_tokens = Some(99.0);
    changed.price_per_million_output_tokens = Some(99.0);
    changed.currency = Some("CNY".into());
    store.save_provider(&changed).unwrap();
    let historical = job::progress_for(&store, job_id).unwrap();
    assert!((historical.estimated_cost.unwrap() - expected).abs() < 1e-12);
    assert_eq!(historical.currency.as_deref(), Some("USD"));
}

#[tokio::test]
async fn duplicate_cells_share_one_request_and_fan_out() {
    let dir = TestDir::new();
    // 两行源文完全相同：预检只算一个唯一片段，派发也只该发一次请求。
    let prepared = prepare(&dir, "重复行,\n重复行,\n");
    assert_eq!(prepared.preflight.unique_units, 1);
    let server = FakeServer::start(vec![Reply::ok(&model_body(
        "Repeated",
        "en",
        Some((100, 20)),
    ))])
    .await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.status, JobStatus::Succeeded);
    // 两个格都完成，但只花了一次请求的 token。
    assert_eq!((progress.total, progress.done), (2, 2));
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    assert_eq!(usage_of(&dir, job_id), (1, 100, 20));
    let rows = unit_rows(&dir);
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row.1 == "succeeded" && row.2 == "Repeated"),
        "去重组的译文应扇出到每个对应单元格：{rows:?}"
    );
}

#[tokio::test]
async fn batch_packs_multiple_units_into_one_request() {
    let dir = TestDir::new();
    // 三行互不相同：去重后是三个片段，按批 3 打包应只发一次请求。
    let prepared = prepare(&dir, "一,\n二,\n三,\n");
    assert_eq!(prepared.preflight.unique_units, 3);
    let server = FakeServer::start(vec![Reply::ok(&batch_body(
        &[(1, "One"), (2, "Two"), (3, "Three")],
        "en",
        Some((60, 30)),
    ))])
    .await;
    let batched = JobLimits {
        batch_size: 3,
        ..limits(1)
    };
    let job_id = create_job(&dir, &prepared, batched);
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        batched,
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.status, JobStatus::Succeeded);
    assert_eq!((progress.total, progress.done, progress.failed), (3, 3, 0));
    // 三次片段合一次请求：请求数按网络调用算，token 按服务端的整批返回值记账。
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    assert_eq!(usage_of(&dir, job_id), (1, 60, 30));
    let rows = unit_rows(&dir);
    assert_eq!(
        rows.iter().map(|row| row.2.as_str()).collect::<Vec<_>>(),
        vec!["One", "Two", "Three"],
        "响应必须按序号回填到对应片段：{rows:?}"
    );
}

#[tokio::test]
async fn missing_batch_item_fails_only_that_unit() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "一,\n二,\n");
    // 响应只回了第 1 条：第 2 条按 id 判为失败，不影响已回填的那条。
    let server = FakeServer::start(vec![Reply::ok(&batch_body(
        &[(1, "One")],
        "en",
        Some((10, 5)),
    ))])
    .await;
    let batched = JobLimits {
        batch_size: 2,
        ..limits(1)
    };
    let job_id = create_job(&dir, &prepared, batched);
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        batched,
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.status, JobStatus::Failed);
    assert_eq!((progress.done, progress.failed), (1, 1));
    let rows = unit_rows(&dir);
    assert_eq!(rows[0].1, "succeeded");
    assert_eq!(rows[0].2, "One");
    assert_eq!(rows[1].1, "failed");
    assert!(rows[1].3.contains("缺少该片段"), "{}", rows[1].3);
}

#[tokio::test]
async fn job_log_reports_model_returned_translations() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n第二行,\n");
    let server = FakeServer::start(vec![Reply::ok(&batch_body(
        &[(1, "One"), (2, "Two")],
        "en",
        None,
    ))])
    .await;
    let batched = JobLimits {
        batch_size: 2,
        ..limits(1)
    };
    let job_id = create_job(&dir, &prepared, batched);
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&logs);
    let store = Store::open(dir.path()).unwrap();
    let progress = job::run(
        store,
        provider(&server.base_url(), 30),
        job_id,
        batched,
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
        move |lines: Vec<String>| captured.lock().unwrap().extend(lines),
    )
    .await
    .unwrap();
    assert_eq!(progress.status, JobStatus::Succeeded);
    let text = logs.lock().unwrap().join("\n");
    // 日志必须能回答"模型这次回了什么"：逐格给出译文本身，而不只是成功计数。
    assert!(text.contains("第 2 行 · en 完成（1 格）：One"), "{text}");
    assert!(text.contains("第 3 行 · en 完成（1 格）：Two"), "{text}");
    assert!(
        text.contains("批次完成：片段 成功 2、失败 0，共结算 2 格"),
        "{text}"
    );
}

#[tokio::test]
async fn job_log_keeps_the_model_response_when_parsing_fails() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n");
    // 模型回了自由文本而不是约定的 JSON：日志里要能看到它到底说了什么，否则无从排障。
    let server = FakeServer::start(vec![Reply::ok(
        "{\"choices\":[{\"message\":{\"content\":\"抱歉，我无法完成这个请求\"}}]}",
    )])
    .await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&logs);
    let store = Store::open(dir.path()).unwrap();
    let progress = job::run(
        store,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
        move |lines: Vec<String>| captured.lock().unwrap().extend(lines),
    )
    .await
    .unwrap();
    assert_eq!(progress.failed, 1);
    let text = logs.lock().unwrap().join("\n");
    assert!(text.contains("无法完成这个请求"), "{text}");
}

#[tokio::test]
async fn prompt_carries_keep_terms_and_the_json_contract() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "欢迎来到Wonderland,\n");
    let server = FakeServer::start(vec![Reply::ok(&model_body(
        "Welcome to Wonderland",
        "en",
        Some((1, 1)),
    ))])
    .await;
    let store = Store::open(dir.path()).unwrap();
    // 不可译词：句中出现时必须照抄。只做整格精确匹配挡不住它，必须进提示词。
    store
        .save_term(crate::TermInput {
            id: None,
            kind: crate::TermKind::DoNotTranslate,
            source_locale: Locale {
                tag: "zh-Hans".into(),
            },
            target_locale: Locale { tag: "en".into() },
            source_text: "Wonderland".into(),
            target_text: String::new(),
            aliases: vec![],
            context: None,
            resource_key: None,
            disambiguation: "品牌".into(),
            expected_version: None,
        })
        .unwrap();
    let mut sources = ContextSources::default();
    sources
        .terms
        .insert("en".into(), store.list_terms().unwrap());

    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        sources,
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.done, 1);

    let body = server.bodies().join("\n");
    assert!(body.contains("保持原文、不翻译"), "{body}");
    // 不可译词只能出现在"照抄"列表里，不能混进译名映射。
    assert!(!body.contains("Wonderland → "), "{body}");
    // 硬规则与 JSON 契约必须真的发出去，而不只是写在代码里。
    assert!(body.contains("花括号和尖括号"), "{body}");
    assert!(body.contains("translations"), "{body}");
    assert!(body.contains("1. 欢迎来到Wonderland"), "{body}");
}

#[tokio::test]
async fn cancelled_job_can_still_be_resumed() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n第二行,\n");
    let server = FakeServer::start(vec![Reply::ok(&model_body("Done", "en", Some((1, 1))))]).await;
    let job_id = create_job(&dir, &prepared, limits(1));

    // 取一格、发出去、然后取消：剩下那格必须仍然可续跑，否则"未完成的格保留为待处理"是句空话。
    let control = Arc::new(JobControl::default());
    let cancelling = Arc::clone(&control);
    let cancelled = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::clone(&control),
        move |_| cancelling.cancel(),
    )
    .await;
    assert_eq!(cancelled.status, JobStatus::Cancelled);
    assert_eq!(cancelled.done, 1);
    assert_eq!(queued_count(&dir), 1);

    let store = Store::open(dir.path()).unwrap();
    let work_id: i64 = 7;
    store
        .conn()
        .execute(
            "INSERT INTO work_records(id,file_name,input_hash,mapping_json,created_at) VALUES(?1,'cancel.csv',?2,'{}',0)",
            rusqlite::params![work_id, prepared.preflight.source_version.sha256],
        )
        .unwrap();
    store
        .conn()
        .execute("UPDATE jobs SET work_id=?1 WHERE id=?2", [work_id, job_id])
        .unwrap();
    assert!(
        job::resumable_exact_job(
            &store,
            job_id,
            work_id,
            &prepared.preflight.source_version.sha256
        )
        .unwrap(),
        "已取消的作业仍有未结算的格，必须能续跑"
    );

    let resumed = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(resumed.status, JobStatus::Succeeded);
    assert_eq!(resumed.done, 2);
}

#[tokio::test]
async fn missing_usage_counts_requests_only() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n");
    let server = FakeServer::start(vec![Reply::ok(&model_body("First", "en", None))]).await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.done, 1);
    // 服务没给 token 数时只累计请求数，不按字数猜用量。
    assert_eq!(
        (
            progress.requests,
            progress.input_tokens,
            progress.output_tokens
        ),
        (1, 0, 0)
    );
    assert_eq!(usage_of(&dir, job_id), (1, 0, 0));
}

#[tokio::test]
async fn rate_limited_request_is_retried_then_succeeds() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n");
    let server = FakeServer::start(vec![
        Reply::status(429, "").header("retry-after", "0"),
        Reply::ok(&model_body("First", "en", Some((10, 5)))),
    ])
    .await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.status, JobStatus::Succeeded);
    assert_eq!(progress.done, 1);
    // 请求数含重试：费用口径必须与实际发出的次数一致。
    assert_eq!(progress.requests, 2);
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn timeout_pauses_the_job_and_leaves_units_pending() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n");
    // 每次响应都晚于 1 秒超时，重试次数用尽后判定为服务整体不可用。
    let server = FakeServer::start(vec![
        Reply::ok(&model_body("Late", "en", None)).delayed(5_000),
    ])
    .await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 1),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    // 持续超时说明"现在没法翻"，不是"这一格坏了"：不记失败、整批退回待处理、停在可续跑的暂停态。
    assert_eq!(progress.status, JobStatus::Paused);
    assert_eq!(progress.done, 0);
    assert_eq!(progress.failed, 0);
    assert!(progress.note.as_deref().unwrap().contains("不可用"));
    assert_eq!(queued_count(&dir), 1);
    // 3 次尝试（首次 + 2 次重试）都超时，请求数照实入账。
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);
    assert_eq!(progress.requests, 3);
}

#[tokio::test]
async fn invalid_json_and_wrong_language_fail_without_success() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n第二行,\n");
    let server = FakeServer::start(vec![
        Reply::ok("{\"choices\":[]}"),
        Reply::ok(&model_body("Wrong", "ja", Some((10, 5)))),
    ])
    .await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.done, 0);
    assert_eq!(progress.failed, 2);
    let rows = unit_rows(&dir);
    assert_eq!(rows[0].1, "failed");
    assert_eq!(rows[1].1, "failed");
    // 错语言的原因要落进 QA 结论，便于在界面上定位。
    assert!(rows[1].3.contains("ja"));
    // 无效响应不重试，但请求确实已经打到服务端，必须计入请求数；token 仍只认成功响应。
    assert_eq!(progress.requests, 2);
}

#[tokio::test]
async fn truncated_response_is_retried_then_recorded_as_failed() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n");
    let server = FakeServer::start(vec![Reply::Partial {
        declared: 4096,
        body: "{\"choices\":[".into(),
    }])
    .await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    // 响应读了一半就断：远端可能已处理，但纯机械化流程不留人工核对口子，
    // 按重试规则重发，用尽次数后记为失败。
    assert_eq!(progress.status, JobStatus::Failed);
    assert_eq!(progress.done, 0);
    assert_eq!(progress.failed, 1);
    assert_eq!(unit_rows(&dir)[0].1, "failed");
    // 首次 + 2 次重试都因响应不完整失败。
    assert_eq!(progress.requests, 3);
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn connection_failure_pauses_the_job_and_resumes_when_service_returns() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n");
    // 绑一次再释放：端口上没有任何监听者，连接会被拒绝。
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let job_id = create_job(&dir, &prepared, limits(1));
    let paused = run_job(
        &dir,
        provider(&format!("http://{addr}/v1"), 2),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    // 连不上是"服务不可用"而不是"这一格失败"：不记失败，格退回待处理。
    assert_eq!(paused.status, JobStatus::Paused);
    assert_eq!(paused.done, 0);
    assert_eq!(paused.failed, 0);
    assert_eq!(queued_count(&dir), 1);
    assert!(paused.note.as_deref().unwrap().contains("继续"));

    // 服务恢复后原作业直接续跑：同一批格没有任何损失，也不要求用户重建任务。
    let server = FakeServer::start(vec![Reply::ok(&model_body("First", "en", Some((1, 1))))]).await;
    let resumed = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(resumed.status, JobStatus::Succeeded);
    assert_eq!(resumed.done, 1);
    assert_eq!(resumed.failed, 0);
}

#[tokio::test]
async fn overlong_unit_narrows_the_batch_and_stays_whole() {
    let dir = TestDir::new();
    // 一条 7000 字的记录：超过单批的源文预算，只能独占一批。
    let long = "长".repeat(7_000);
    let prepared = prepare(&dir, &format!("{long},\n短句一,\n短句二,\n"));
    let server = FakeServer::start(vec![
        Reply::ok(&batch_body(&[(1, "Long line")], "en", Some((1, 1)))),
        Reply::ok(&batch_body(
            &[(1, "Short one"), (2, "Short two")],
            "en",
            Some((1, 1)),
        )),
    ])
    .await;
    let batched = JobLimits {
        include_false_rows: false,
        concurrency: 1,
        batch_size: 3,
        input_token_budget: 0,
        output_token_budget: 0,
    };
    let job_id = create_job(&dir, &prepared, batched);
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        batched,
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.status, JobStatus::Succeeded);
    assert_eq!(progress.done, 3);

    // 固定"每批 3 行"在这里会给出一份塞不下的提示词；按字符数收缩后超长行独占一批。
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2, "超长行应独占一批，其余两行合批");
    assert!(bodies[0].contains("请逐条翻译下列 1 个片段"));
    // 源文一字不少：最小翻译单元绝不被截断。
    assert!(bodies[0].contains(&long));
    assert!(bodies[1].contains("请逐条翻译下列 2 个片段"));
}

#[tokio::test]
async fn stale_running_unit_is_requeued_and_translated() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n第二行,\n");
    let job_id = create_job(&dir, &prepared, limits(1));
    // 模拟上次进程在请求发出后崩溃：一格留在 running。
    {
        let store = Store::open(dir.path()).unwrap();
        store
            .conn()
            .execute(
                "UPDATE job_units SET status='running' WHERE row_number=2",
                [],
            )
            .unwrap();
    }
    let store = Store::open(dir.path()).unwrap();
    // 全自动流程不判断"上次到底发出去了没有"：直接退回待处理，由作业重发。
    assert_eq!(job::requeue_stale_running(&store, job_id).unwrap(), 1);
    assert_eq!(queued_count(&dir), 2);

    let server = FakeServer::start(vec![Reply::ok(&model_body("Done", "en", Some((1, 1))))]).await;
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    // 两格都被补上：遗留在 running 的那格没有被跳过。
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
    assert_eq!(progress.done, 2);
    assert_eq!(progress.status, JobStatus::Succeeded);
}

#[tokio::test]
async fn pause_stops_dispatch_and_resume_finishes_remaining() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n第二行,\n第三行,\n");
    let server = FakeServer::start(vec![Reply::ok(&model_body("Done", "en", Some((1, 1))))]).await;
    let job_id = create_job(&dir, &prepared, limits(1));

    let control = Arc::new(JobControl::default());
    let pausing = Arc::clone(&control);
    let paused = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::clone(&control),
        move |_| pausing.pause(),
    )
    .await;
    assert_eq!(paused.status, JobStatus::Paused);
    assert_eq!(paused.done, 1);
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);

    let resumed = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(resumed.status, JobStatus::Succeeded);
    assert_eq!(resumed.done, 3);
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn cancellation_stops_before_any_request_completes() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n第二行,\n");
    let server = FakeServer::start(vec![Reply::ok(&model_body("Done", "en", Some((1, 1))))]).await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let control = Arc::new(JobControl::default());
    control.cancel();
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::clone(&control),
        |_| {},
    )
    .await;
    assert_eq!(progress.status, JobStatus::Cancelled);
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
    // 未处理的格保持待处理，续跑时还能接着做。
    assert_eq!(queued_count(&dir), 2);
}

#[tokio::test]
async fn budget_limit_pauses_before_spending_more() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n第二行,\n");
    let server =
        FakeServer::start(vec![Reply::ok(&model_body("Done", "en", Some((10, 10))))]).await;
    let capped = JobLimits {
        include_false_rows: false,
        concurrency: 1,
        batch_size: 1,
        input_token_budget: 1,
        output_token_budget: 1,
    };
    let job_id = create_job(&dir, &prepared, capped);
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        capped,
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.status, JobStatus::Paused);
    assert_eq!(progress.done, 1);
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    assert!(progress.note.as_deref().unwrap().contains("预算"));
}

#[tokio::test]
async fn terms_reach_prompt_and_placeholders_survive() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "欢迎来到{1:s.昵称},\n");
    let server = FakeServer::start(vec![Reply::ok(&model_body(
        "Welcome, {1:s.昵称}",
        "en",
        Some((1, 1)),
    ))])
    .await;
    let store = Store::open(dir.path()).unwrap();
    store
        .save_term(crate::TermInput {
            id: None,
            kind: crate::TermKind::Ordinary,
            source_locale: Locale {
                tag: "zh-Hans".into(),
            },
            target_locale: Locale { tag: "en".into() },
            source_text: "欢迎".into(),
            target_text: "Welcome".into(),
            aliases: vec![],
            context: None,
            resource_key: None,
            disambiguation: String::new(),
            expected_version: None,
        })
        .unwrap();
    // 词条不设"已审核"闸门：启用即作为句内约束进入提示词。
    let mut sources = ContextSources::default();
    sources
        .terms
        .insert("en".into(), store.list_terms().unwrap());

    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        sources,
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.done, 1);
    let rows = unit_rows(&dir);
    assert_eq!(rows[0].1, "succeeded");
    assert_eq!(rows[0].2, "Welcome, {1:s.昵称}");
    // 占位符保留完整，格式检查通过。
    assert_eq!(rows[0].3, "ok");
}

#[tokio::test]
async fn configured_concurrency_really_overlaps_requests() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "一,\n二,\n三,\n四,\n");
    // 每个响应延迟 300ms：服务器侧观察到的并发峰值才有意义。
    let server = FakeServer::start(vec![
        Reply::ok(&model_body("X", "en", Some((1, 1)))).delayed(300),
    ])
    .await;
    let job_id = create_job(&dir, &prepared, limits(4));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(4),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.done, 4);
    // 旧实现把请求起点锁成固定间隔，峰值恒为 1；这条断言正是那次修复的回归防线。
    assert!(
        server.peak() >= 2,
        "并发 4 应出现多个同时在飞的请求，实际峰值 {}",
        server.peak()
    );
}

#[tokio::test]
async fn draft_export_writes_results_without_touching_existing() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "第一行,\n第二行,已有\n");
    let server = FakeServer::start(vec![Reply::ok(&model_body(
        "First line",
        "en",
        Some((1, 1)),
    ))])
    .await;
    let job_id = create_job(&dir, &prepared, limits(1));
    run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;

    let exported = export_with(&dir, &prepared, engine::ExportMode::DraftsFor(job_id));
    // 草稿导出与正式导出必须落在不同目录，避免"这份导出到底是什么"变成隐含行为。
    assert!(exported.directory.contains("drafts-"));
    let rows = exported_rows(&exported.csv_path);
    assert_eq!(rows[0].get(1), Some("First line"));
    // 已有译文原样保留，草稿不会覆盖它。
    assert_eq!(rows[1].get(1), Some("已有"));

    // 正式导出策略未被放宽：草稿不写进正式结果。
    let approved = export_with(&dir, &prepared, engine::ExportMode::Approved);
    assert!(approved.directory.contains("translation-"));
    assert_eq!(exported_rows(&approved.csv_path)[0].get(1), Some(""));
}

#[tokio::test]
async fn draft_with_broken_placeholders_is_withheld_from_export() {
    let dir = TestDir::new();
    let prepared = prepare(&dir, "你好{1:s.昵称},\n");
    // 模型丢掉了变量：作业仍记为成功（草稿确实是模型输出），但导出必须拦住它。
    let server = FakeServer::start(vec![Reply::ok(&model_body("Hello", "en", Some((1, 1))))]).await;
    let job_id = create_job(&dir, &prepared, limits(1));
    let progress = run_job(
        &dir,
        provider(&server.base_url(), 30),
        job_id,
        limits(1),
        ContextSources::default(),
        Arc::new(JobControl::default()),
        |_| {},
    )
    .await;
    assert_eq!(progress.done, 1);
    assert_eq!(unit_rows(&dir)[0].3, "blocking");

    let exported = export_with(&dir, &prepared, engine::ExportMode::DraftsFor(job_id));
    assert_eq!(exported_rows(&exported.csv_path)[0].get(1), Some(""));
}
