//! 翻译插件入口。启动只保留目录路径；导入、导出与翻译作业都由用户动作触发。

mod config;
mod csvio;
mod engine;
mod job;
mod profiles;
mod provider;
mod qa;
mod secrets;
mod store;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_p3;
mod workspace;

pub mod model;
pub use model::*;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use store::Store;
use wonderland_plugin_sdk::PluginFailure;
pub use secrets::SecretStore;
pub use provider::{ModelTransport, ProviderError, TransportFuture};

pub struct Translator {
    data_dir: PathBuf,
    selected: Mutex<Option<PathBuf>>,
    selected_work: Mutex<Option<i64>>,
    prepared: Mutex<Option<engine::Prepared>>,
    busy: AtomicBool,
    cancel: AtomicBool,
    /// 当前作业的暂停/取消开关；作业结束后清空。与 `busy` 一起保证同一时刻只有一个作业。
    job: Mutex<Option<Arc<job::JobControl>>>,
    secrets: Arc<dyn SecretStore>,
    model_transport: Arc<dyn ModelTransport>,
}

struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Translator {
    pub fn new(data_dir: PathBuf, secrets: Arc<dyn SecretStore>, model_transport: Arc<dyn ModelTransport>) -> Self {
        Self {
            data_dir,
            selected: Mutex::new(None),
            selected_work: Mutex::new(None),
            prepared: Mutex::new(None),
            busy: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            job: Mutex::new(None),
            secrets,
            model_transport,
        }
    }

    fn begin(&self) -> Result<BusyGuard<'_>, PluginFailure> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .map_err(|_| PluginFailure::Other("翻译插件已有正在执行的操作".into()))?;
        self.cancel.store(false, Ordering::Release);
        Ok(BusyGuard(&self.busy))
    }

    pub fn work_tree(&self) -> Result<WorkTree, PluginFailure> {
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        self.settle_orphan_jobs(&store)?;
        workspace::tree(&store).map_err(PluginFailure::Other)
    }

    pub fn rename_work(&self, work_id: i64, name: &str) -> Result<(), PluginFailure> {
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        workspace::rename_record(&store, work_id, name).map_err(PluginFailure::Other)
    }

    pub fn open_work(&self, work_id: i64) -> Result<Preflight, PluginFailure> {
        let _guard = self.begin()?;
        let mut store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        let (record, mapping) = workspace::record(&store, work_id).map_err(PluginFailure::Other)?;
        let snapshot = self
            .data_dir
            .join("translator")
            .join("inputs")
            .join(format!("{}.csv", record.input_hash));
        let mut prepared = engine::prepare_with_store(
            &snapshot,
            &self.data_dir,
            mapping,
            &self.cancel,
            Some(&mut store),
        )
        .map_err(PluginFailure::Other)?;
        prepared.preflight.input_name = record.file_name;
        let summary = prepared.preflight.clone();
        *self
            .selected
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = Some(snapshot);
        *self
            .selected_work
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = Some(work_id);
        *self
            .prepared
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = Some(prepared);
        Ok(summary)
    }

    pub fn csv_page(
        &self,
        work_id: i64,
        job_id: Option<i64>,
        offset: u64,
        limit: u64,
    ) -> Result<CsvPage, PluginFailure> {
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        workspace::csv_page(&store, &self.data_dir, work_id, job_id, offset, limit)
            .map_err(PluginFailure::Other)
    }

    pub fn work_job_progress(&self, job_id: i64) -> Result<JobProgress, PluginFailure> {
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        self.settle_orphan_jobs(&store)?;
        job::progress_for(&store, job_id).map_err(PluginFailure::Other)
    }

    /// 界面读任务状态前，先把库里遗留的 `running` 收成 `paused`（见 [`job::pause_orphan_running`]）。
    ///
    /// 只在既没有作业在跑、也没有别的插件操作在飞时动手：`create_job` 与 `job::run` 之间有一小段
    /// 窗口，作业行已经写成 `running` 而控制块还没挂上，此时误判会让界面闪一下"已暂停"。
    fn settle_orphan_jobs(&self, store: &Store) -> Result<(), PluginFailure> {
        let has_job = self
            .job
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)?
            .is_some();
        if has_job || self.busy.load(Ordering::Acquire) {
            return Ok(());
        }
        job::pause_orphan_running(store).map_err(PluginFailure::Other)?;
        Ok(())
    }

    pub fn provider_profiles(&self) -> Result<Vec<ProviderProfile>, PluginFailure> {
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        profiles::list(&store, self.secrets.as_ref()).map_err(PluginFailure::Other)
    }

    pub fn save_profile(
        &self,
        id: Option<i64>,
        name: &str,
        input: ProviderConfigInput,
    ) -> Result<ProviderProfile, PluginFailure> {
        let _guard = self.begin()?;
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        profiles::save(&store, self.secrets.as_ref(), id, name, input).map_err(PluginFailure::Other)
    }

    pub fn activate_profile(&self, id: i64) -> Result<ProviderProfile, PluginFailure> {
        let _guard = self.begin()?;
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        profiles::activate(&store, self.secrets.as_ref(), id).map_err(PluginFailure::Other)
    }

    pub fn delete_profile(&self, id: i64) -> Result<(), PluginFailure> {
        let _guard = self.begin()?;
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        profiles::delete(&store, self.secrets.as_ref(), id).map_err(PluginFailure::Other)
    }

    /// 路径只由主窗口原生文件对话框传入；前端不提供任意路径参数。
    pub fn select_file(&self, path: &Path) -> Result<CsvInspection, PluginFailure> {
        let _guard = self.begin()?;
        let inspection = engine::inspect_file(path).map_err(PluginFailure::Other)?;
        csvio::validate_product_format(&inspection).map_err(PluginFailure::Other)?;
        *self
            .selected
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = Some(path.to_path_buf());
        *self
            .prepared
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = None;
        *self
            .selected_work
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = None;
        Ok(inspection)
    }

    /// Import bytes from a short-lived Core file handle into the plugin-owned data area.
    pub fn select_file_contents(&self, name: &str, bytes: &[u8]) -> Result<CsvInspection, PluginFailure> {
        if !name.to_ascii_lowercase().ends_with(".csv") || bytes.len() > csvio::MAX_FILE_BYTES as usize {
            return Err(PluginFailure::InvalidInput);
        }
        static NEXT_INPUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let incoming = self.data_dir.join("incoming");
        fs::create_dir_all(&incoming).map_err(|error| PluginFailure::Other(error.to_string()))?;
        let path = incoming.join(format!("import-{}.csv", NEXT_INPUT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, bytes).map_err(|error| PluginFailure::Other(error.to_string()))?;
        self.select_file(&path)
    }

    pub fn prepare(&self, mapping: ColumnMapping) -> Result<Preflight, PluginFailure> {
        let _guard = self.begin()?;
        *self
            .prepared
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = None;
        let source = self
            .selected
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)?
            .clone()
            .ok_or_else(|| PluginFailure::Other("请先选择 CSV 文件".into()))?;
        let inspection = engine::inspect_file(&source).map_err(PluginFailure::Other)?;
        csvio::validate_product_format(&inspection).map_err(PluginFailure::Other)?;
        if mapping != inspection.default_mapping {
            return Err(PluginFailure::Other("固定 CSV 格式不允许修改列映射".into()));
        }
        let mut store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        let prepared = engine::prepare_with_store(
            &source,
            &self.data_dir,
            mapping,
            &self.cancel,
            Some(&mut store),
        )
        .map_err(PluginFailure::Other)?;
        let work_id = workspace::add_record(
            &store,
            &prepared.preflight.input_name,
            &prepared.preflight.source_version.sha256,
            &prepared.preflight.mapping,
        )
        .map_err(PluginFailure::Other)?;
        *self
            .selected_work
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = Some(work_id);
        let summary = prepared.preflight.clone();
        *self
            .prepared
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = Some(prepared);
        Ok(summary)
    }

    pub fn export(&self) -> Result<TranslationExport, PluginFailure> {
        let _guard = self.begin()?;
        let prepared = self.prepared()?;
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        engine::export_with_store(
            &prepared,
            &self.data_dir,
            &self.cancel,
            Some(&store),
            engine::ExportMode::Approved,
        )
        .map_err(PluginFailure::Other)
    }

    pub fn export_work_drafts(&self, job_id: i64) -> Result<TranslationExport, PluginFailure> {
        let _guard = self.begin()?;
        let prepared = self.prepared()?;
        let work_id = self
            .selected_work
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)?
            .ok_or_else(|| PluginFailure::Other("请先选择 CSV 工作记录".into()))?;
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        let owner: i64 = store
            .conn()
            .query_row("SELECT work_id FROM jobs WHERE id=?1", [job_id], |row| {
                row.get(0)
            })
            .map_err(|error| PluginFailure::Other(error.to_string()))?;
        if owner != work_id {
            return Err(PluginFailure::Other("翻译任务不属于当前 CSV 工作记录".into()));
        }
        engine::export_with_store(
            &prepared,
            &self.data_dir,
            &self.cancel,
            Some(&store),
            engine::ExportMode::DraftsFor(job_id),
        )
        .map_err(PluginFailure::Other)
    }

    fn prepared(&self) -> Result<engine::Prepared, PluginFailure> {
        self.prepared
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)?
            .clone()
            .ok_or_else(|| PluginFailure::Other("请先完成 CSV 预检".into()))
    }

    fn invalidate(&self) -> Result<(), PluginFailure> {
        *self
            .prepared
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)? = None;
        Ok(())
    }

    pub fn memory_entries(&self) -> Result<Vec<MemoryEntry>, PluginFailure> {
        Store::open(&self.data_dir)
            .and_then(|s| s.list_memory())
            .map_err(PluginFailure::Other)
    }

    pub fn disable_memory(&self, id: i64) -> Result<(), PluginFailure> {
        let _guard = self.begin()?;
        Store::open(&self.data_dir)
            .and_then(|s| s.disable_memory(id))
            .map_err(PluginFailure::Other)?;
        self.invalidate()
    }

    pub fn glossary_terms(&self) -> Result<Vec<GlossaryTerm>, PluginFailure> {
        Store::open(&self.data_dir)
            .and_then(|s| s.list_terms())
            .map_err(PluginFailure::Other)
    }

    pub fn save_term(&self, input: TermInput) -> Result<GlossaryTerm, PluginFailure> {
        let _guard = self.begin()?;
        let term = Store::open(&self.data_dir)
            .and_then(|s| s.save_term(input))
            .map_err(PluginFailure::Other)?;
        self.invalidate()?;
        Ok(term)
    }

    pub fn disable_term(&self, id: i64) -> Result<(), PluginFailure> {
        let _guard = self.begin()?;
        Store::open(&self.data_dir)
            .and_then(|s| s.disable_term(id))
            .map_err(PluginFailure::Other)?;
        self.invalidate()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        if let Ok(job) = self.job.lock()
            && let Some(control) = job.as_ref()
        {
            control.cancel();
        }
    }

    // ---- 模型服务配置 ----

    fn build_provider(&self) -> Result<Arc<dyn provider::TranslatorProvider>, PluginFailure> {
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        let settings = store.load_provider().map_err(PluginFailure::Other)?;
        settings.validate().map_err(PluginFailure::Other)?;
        let profile_id = profiles::active_id(&store).map_err(PluginFailure::Other)?;
        if !self.secrets.has(profile_id).map_err(PluginFailure::Other)? {
            return Err(PluginFailure::Other("请先配置模型服务的 API Key".into()));
        }
        let adapter = provider::OpenAiCompatibleProvider::new(&settings, profile_id, self.model_transport.clone())
            .map_err(|error| PluginFailure::Other(error.to_string()))?;
        Ok(Arc::new(adapter))
    }

    // ---- 作业 ----

    pub fn pause_job(&self) -> Result<(), PluginFailure> {
        let control = self.job.lock().map_err(|_| PluginFailure::NotInitialized)?;
        let control = control
            .as_ref()
            .ok_or_else(|| PluginFailure::Other("当前没有进行中的翻译作业".into()))?;
        control.pause();
        Ok(())
    }

    /// 启动一次新作业。`progress` 是节流后的进度回调，`log` 是逐批的可读日志（一次调用一批行）。
    pub async fn start_job(
        &self,
        limits: JobLimits,
        progress: impl FnMut(JobProgress) + Send,
        log: impl FnMut(Vec<String>) + Send,
    ) -> Result<JobProgress, PluginFailure> {
        self.run_job(limits, true, None, progress, log).await
    }

    pub async fn resume_selected_job(
        &self,
        job_id: i64,
        limits: JobLimits,
        progress: impl FnMut(JobProgress) + Send,
        log: impl FnMut(Vec<String>) + Send,
    ) -> Result<JobProgress, PluginFailure> {
        self.run_job(limits, false, Some(job_id), progress, log)
            .await
    }

    async fn run_job(
        &self,
        limits: JobLimits,
        fresh: bool,
        resume_id: Option<i64>,
        progress: impl FnMut(JobProgress) + Send,
        log: impl FnMut(Vec<String>) + Send,
    ) -> Result<JobProgress, PluginFailure> {
        let _guard = self.begin()?;
        let prepared = self.prepared()?;
        let adapter = self.build_provider()?;
        let store = Store::open(&self.data_dir).map_err(PluginFailure::Other)?;
        let settings = store.load_provider().map_err(PluginFailure::Other)?;
        let fingerprint = settings.fingerprint();
        let hash = prepared.preflight.source_version.sha256.clone();
        // 上下文材料只依赖库，先算出来：放在建作业之前，作业行一旦存在就没有"半路失败留下孤儿"的缺口。
        let sources = self.context_sources(&store)?;

        let job_id = if fresh {
            let drafts = engine::collect_units(
                &prepared,
                Some(&store),
                &self.cancel,
                limits.include_false_rows,
            )
            .map_err(PluginFailure::Other)?;
            if drafts.is_empty() {
                return Err(PluginFailure::Other(
                    "没有需要调用模型的空白格：记忆与词条已覆盖全部待填内容".into(),
                ));
            }
            job::create_job(
                &store,
                &hash,
                &fingerprint,
                (
                    prepared.preflight.memory_version,
                    prepared.preflight.glossary_version,
                ),
                limits,
                &drafts,
                &adapter.capabilities(),
            )
            .map_err(PluginFailure::Other)?
        } else {
            let work_id = self
                .selected_work
                .lock()
                .map_err(|_| PluginFailure::NotInitialized)?
                .ok_or_else(|| PluginFailure::Other("请先选择 CSV 工作记录".into()))?;
            let job_id = resume_id.ok_or_else(|| PluginFailure::Other("请先选择翻译任务".into()))?;
            if !job::resumable_exact_job(&store, job_id, work_id, &hash)
                .map_err(PluginFailure::Other)?
            {
                return Err(PluginFailure::Other(
                    "没有可续跑的作业；请确认当前工作记录与输入快照".into(),
                ));
            }
            // 上次进程遗留在 `running` 的格直接退回待处理，等作业自己重发；不引入人工核对。
            job::requeue_stale_running(&store, job_id).map_err(PluginFailure::Other)?;
            job::requeue_failed_units(&store, job_id).map_err(PluginFailure::Other)?;
            job_id
        };
        // 作业已经建出来了：从这里到真正开跑之间只有"把任务挂到工作记录"这一步，
        // 它失败也必须把作业收口，否则库里会留下一个既暂停不了、也看不出来龙去脉的任务。
        if let Some(work_id) = *self
            .selected_work
            .lock()
            .map_err(|_| PluginFailure::NotInitialized)?
            && let Err(error) = store.conn().execute(
                "UPDATE jobs SET work_id=?1 WHERE id=?2",
                rusqlite::params![work_id, job_id],
            )
        {
            return Err(self.abandon_job(
                &store,
                job_id,
                format!("无法把翻译任务挂到工作记录：{error}"),
            ));
        }

        let control = Arc::new(job::JobControl::default());
        *self.job.lock().map_err(|_| PluginFailure::NotInitialized)? = Some(Arc::clone(&control));

        let outcome = job::run(
            store,
            adapter,
            job_id,
            limits,
            sources,
            Arc::clone(&control),
            progress,
            log,
        )
        .await;
        *self.job.lock().map_err(|_| PluginFailure::NotInitialized)? = None;
        match outcome {
            Ok(progress) => Ok(progress),
            Err(error) => {
                // 作业因内部错误（数据库、任务异常）提前退出时，库里绝不能留下"运行中"：
                // 那种状态会一直显示在界面上，而"暂停"必然报"当前没有进行中的翻译作业"，
                // 用户既看不出发生了什么也没法收场。
                match Store::open(&self.data_dir) {
                    Ok(store) => Err(self.abandon_job(&store, job_id, error)),
                    Err(open_error) => Err(PluginFailure::Other(format!(
                        "作业 #{job_id} 异常结束：{error}；结算作业状态也失败：{open_error}"
                    ))),
                }
            }
        }
    }

    /// 作业存在但没能正常收尾时的统一出口：把它与未结算的格一起记为失败，并返回给界面的说明。
    ///
    /// 收口成 `failed` 而不是 `cancelled`：这不是用户的选择，而是调度自己出错；写成失败
    /// 才如实反映"这批格没有结果"，用户点"继续"即可把失败格重新排上。
    fn abandon_job(&self, store: &Store, job_id: i64, reason: String) -> PluginFailure {
        let suffix = match job::settle_after_error(store, job_id, &reason) {
            Ok(()) => String::new(),
            Err(settle_error) => format!("；结算作业状态也失败：{settle_error}"),
        };
        PluginFailure::Other(format!(
            "作业 #{job_id} 异常结束，已停止并记为失败：{reason}{suffix}"
        ))
    }

    /// 组装提示词上下文：启用词条作强约束，既有译文作少量参考。
    ///
    /// 两者都不设人工审批闸门：词条与已落库的译文直接生效，插件全程不需要人工放行。
    fn context_sources(&self, store: &Store) -> Result<job::ContextSources, PluginFailure> {
        let mut sources = job::ContextSources::default();
        for term in store.list_terms().map_err(PluginFailure::Other)? {
            if !term.enabled {
                continue;
            }
            sources
                .terms
                .entry(term.target_locale.tag.clone())
                .or_default()
                .push(term);
        }
        for entry in store.list_memory().map_err(PluginFailure::Other)? {
            if !entry.enabled || entry.qa_blocking {
                continue;
            }
            let list = sources
                .examples
                .entry(entry.target_locale.tag.clone())
                .or_default();
            // 每个语言只带少量示例：示例是"参考风格"，多了只会挤占源文与词条的位置。
            if list.len() < 5 {
                list.push(provider::MemoryExample {
                    source_text: entry.source_text,
                    target_text: entry.target_text,
                });
            }
        }
        Ok(sources)
    }
}
