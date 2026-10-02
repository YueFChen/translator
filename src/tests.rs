use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::SecretStore;
use crate::csvio;
use crate::engine;
use crate::qa;
use crate::store::Store;
use crate::{IssueSeverity, Locale};
use crate::{profiles, workspace};

#[test]
fn false_row_option_selects_only_false_markers_and_exports_job_draft() {
    let dir = TestDir::new();
    let path = dir.write(
        "selection.csv",
        "Source,Translate,English\n甲,TRUE,\n乙,FALSE,\n丙,OTHER,\n".as_bytes(),
    );
    let mut mapping = csvio::inspect(&path).unwrap().default_mapping;
    mapping.source_text_column = 0;
    mapping.selection_column = Some(1);
    mapping.context_column = None;
    mapping.resource_key_column = None;
    mapping.target_columns = vec![crate::TargetColumn {
        column: 2,
        locale: Locale { tag: "en".into() },
    }];
    let prepared = engine::prepare(&path, dir.path(), mapping, &AtomicBool::new(false)).unwrap();
    let store = Store::open(dir.path()).unwrap();
    let default_units =
        engine::collect_units(&prepared, Some(&store), &AtomicBool::new(false), false).unwrap();
    assert_eq!(default_units.len(), 1);
    let enabled_units =
        engine::collect_units(&prepared, Some(&store), &AtomicBool::new(false), true).unwrap();
    assert_eq!(
        enabled_units
            .iter()
            .map(|unit| unit.row_number)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    let limits = crate::JobLimits {
        concurrency: 1,
        batch_size: 1,
        include_false_rows: true,
        input_token_budget: 0,
        output_token_budget: 0,
    };
    let capabilities = crate::config::ProviderSettings {
        base_url: "https://example.com/v1".into(),
        model: "test".into(),
        allow_loopback: false,
        timeout_seconds: 120,
        requests_per_minute: None,
        price_per_million_input_tokens: None,
        price_per_million_output_tokens: None,
        currency: None,
    }
    .capabilities();
    let job_id = crate::job::create_job(
        &store,
        &prepared.preflight.source_version.sha256,
        "test",
        (0, 0),
        limits,
        &enabled_units,
        &capabilities,
    )
    .unwrap();
    assert!(
        crate::job::limits_for_job(&store, job_id)
            .unwrap()
            .include_false_rows
    );
    store.conn().execute(
        "UPDATE job_units SET status='succeeded',target_text='Second',qa_state='ok' WHERE job_id=?1 AND row_number=3",
        [job_id],
    ).unwrap();
    let exported = engine::export_with_store(
        &prepared,
        dir.path(),
        &AtomicBool::new(false),
        Some(&store),
        engine::ExportMode::DraftsFor(job_id),
    )
    .unwrap();
    let mut reader = csvio::open_reader(Path::new(&exported.csv_path)).unwrap();
    assert_eq!(
        csvio::headers(&mut reader)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec!["Source", "Translate", "English"]
    );
    let rows = reader.records().map(Result::unwrap).collect::<Vec<_>>();
    assert_eq!(
        rows[1].iter().collect::<Vec<_>>(),
        vec!["乙", "FALSE", "Second"]
    );
    assert_eq!(rows[2].get(2), Some(""));
}

#[test]
fn work_history_tracks_repeat_csv_and_scopes_table_to_job() {
    let dir = TestDir::new();
    let path = dir.write("repeat.csv", b"Source,English\nHello,\nWorld,Existing\n");
    let mut mapping = csvio::inspect(&path).unwrap().default_mapping;
    mapping.source_text_column = 0;
    mapping.target_columns = vec![crate::TargetColumn {
        column: 1,
        locale: Locale { tag: "en".into() },
    }];
    let prepared =
        engine::prepare(&path, dir.path(), mapping.clone(), &AtomicBool::new(false)).unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = workspace::add_record(
        &store,
        "repeat.csv",
        &prepared.preflight.source_version.sha256,
        &mapping,
    )
    .unwrap();
    let second = workspace::add_record(
        &store,
        "repeat.csv",
        &prepared.preflight.source_version.sha256,
        &mapping,
    )
    .unwrap();
    workspace::rename_record(&store, first, "英语剧情首轮").unwrap();
    let tree = workspace::tree(&store).unwrap();
    assert_eq!(tree.records.len(), 2);
    let named = tree
        .records
        .iter()
        .find(|record| record.id == first)
        .unwrap();
    assert_eq!(named.title.as_deref(), Some("英语剧情首轮"));
    assert_eq!(named.file_name, "repeat.csv");
    let page = workspace::csv_page(&store, dir.path(), first, None, 0, 1).unwrap();
    assert_eq!(page.rows[0], ["Hello", ""]);
    assert!(page.has_more);
    store.conn().execute("INSERT INTO jobs(work_id,input_hash,provider_fingerprint,memory_version,glossary_version,status,limits_json,created_at,updated_at) VALUES(?1,?2,'test',0,0,'paused','{}',0,0)", rusqlite::params![first, prepared.preflight.source_version.sha256]).unwrap();
    let job_id = store.conn().last_insert_rowid();
    assert!(
        crate::job::resumable_exact_job(
            &store,
            job_id,
            first,
            &prepared.preflight.source_version.sha256
        )
        .unwrap()
    );
    assert!(
        !crate::job::resumable_exact_job(
            &store,
            job_id,
            second,
            &prepared.preflight.source_version.sha256
        )
        .unwrap()
    );
    store.conn().execute("INSERT INTO job_units(job_id,row_number,target_column,target_locale,source_locale,source_text,context,resource_key,signature,group_key,status,attempt,target_text,qa_state,updated_at) VALUES(?1,2,1,'en','zh-Hans','Hello','','','','test','succeeded',1,'Bonjour','ok',0)", [job_id]).unwrap();
    let translated = workspace::csv_page(&store, dir.path(), first, Some(job_id), 0, 2).unwrap();
    assert_eq!(translated.rows[0][1], "Bonjour");
    assert_eq!(translated.rows[1][1], "Existing");
    assert!(workspace::csv_page(&store, dir.path(), second, Some(job_id), 0, 2).is_err());
    store
        .conn()
        .execute(
            "UPDATE job_units SET status='failed' WHERE job_id=?1",
            [job_id],
        )
        .unwrap();
    // 失败格由续跑机械重排，不需要人工决定"重试还是放弃"。
    assert_eq!(crate::job::requeue_failed_units(&store, job_id).unwrap(), 1);
    assert_eq!(
        store
            .conn()
            .query_row(
                "SELECT status FROM job_units WHERE job_id=?1",
                [job_id],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "queued"
    );
    // 进程中断遗留在 running 的格同样直接退回待处理。
    store
        .conn()
        .execute(
            "UPDATE job_units SET status='running' WHERE job_id=?1",
            [job_id],
        )
        .unwrap();
    assert_eq!(
        crate::job::requeue_stale_running(&store, job_id).unwrap(),
        1
    );
    assert_eq!(
        store
            .conn()
            .query_row(
                "SELECT status FROM job_units WHERE job_id=?1",
                [job_id],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "queued"
    );
}

#[test]
fn old_dev_database_is_backed_up_without_migration() {
    let dir = TestDir::new();
    let store = Store::open(dir.path()).unwrap();
    store
        .conn()
        .execute_batch("PRAGMA user_version=5;")
        .unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let version: i64 = store
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 8);
    assert!(
        std::fs::read_dir(dir.path().join("translator"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("project-v5-"))
    );
    workspace::add_record(
        &store,
        "original.csv",
        "test-hash",
        &crate::ColumnMapping {
            source_text_column: 2,
            source_locale: Locale {
                tag: "zh-Hans".into(),
            },
            selection_column: Some(1),
            context_column: None,
            resource_key_column: None,
            target_columns: vec![],
        },
    )
    .unwrap();
    let record = &workspace::tree(&store).unwrap().records[0];
    assert_eq!(record.file_name, "original.csv");
    assert_eq!(record.title, None);
}

#[test]
fn provider_profiles_keep_separate_active_configurations() {
    let dir = TestDir::new();
    let store = Store::open(dir.path()).unwrap();
    let secrets = FakeSecrets::default();
    let profiles_before = profiles::list(&store, &secrets).unwrap();
    assert!(profiles_before.is_empty());
    let input = crate::ProviderConfigInput {
        base_url: "https://example.com/v1".into(),
        model: "model-a".into(),
        remark: "工作项目".into(),
        allow_loopback: false,
        timeout_seconds: 120,
        requests_per_minute: None,
        price_per_million_input_tokens: None,
        price_per_million_output_tokens: None,
        currency: None,
        api_key: None,
    };
    let saved = profiles::save(&store, &secrets, None, "第二服务", input).unwrap();
    assert!(!saved.active);
    assert_eq!(saved.name, "第二服务");
    assert_eq!(saved.config.model, "model-a");
    assert_eq!(saved.config.remark, "工作项目");
    let active = profiles::activate(&store, &secrets, saved.id).unwrap();
    assert!(active.active);
    assert_eq!(store.load_provider().unwrap().model, "model-a");
    assert_eq!(profiles::list(&store, &secrets).unwrap().len(), 1);
    assert_eq!(
        profiles::list(&store, &secrets).unwrap()[0].config.remark,
        "工作项目"
    );
    secrets.set(saved.id, "first-key").unwrap();
    secrets.set(saved.id, "second-key").unwrap();
    assert!(secrets.has(saved.id).unwrap());
}

#[derive(Default)]
struct FakeSecrets(std::sync::Mutex<std::collections::HashMap<i64, String>>);

impl crate::secrets::SecretStore for FakeSecrets {
    fn has(&self, id: i64) -> Result<bool, String> {
        Ok(self.0.lock().unwrap().contains_key(&id))
    }
    fn set(&self, id: i64, value: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert(id, value.to_owned());
        Ok(())
    }
    fn delete(&self, id: i64) -> Result<(), String> {
        self.0.lock().unwrap().remove(&id);
        Ok(())
    }
}

#[test]
fn p2_memory_is_reused_without_approval_and_ambiguity_blocks_reuse() {
    let dir = TestDir::new();
    let path = dir.write(
        "history.csv",
        b"Source,Context,English\nHello,A,Bonjour\nHello,A,\nHello,B,\n",
    );
    let mut mapping = csvio::inspect(&path).unwrap().default_mapping;
    mapping.source_text_column = 0;
    mapping.context_column = Some(1);
    mapping.target_columns = vec![crate::TargetColumn {
        column: 2,
        locale: Locale { tag: "en".into() },
    }];
    let cancel = AtomicBool::new(false);
    let mut store = Store::open(dir.path()).unwrap();
    let first = engine::prepare_with_store(
        &path,
        dir.path(),
        mapping.clone(),
        &cancel,
        Some(&mut store),
    )
    .unwrap();
    // 导入的既有译文**不需要人工批准**：同一语境下唯一就直接复用。
    assert_eq!(first.preflight.memory_reused_cells, 1);
    assert_eq!(first.preflight.pending_cells, 1);
    // 语境 B 下没有候选，仍需模型翻译。
    assert_eq!(
        store
            .lookup(
                "zh-Hans",
                "en",
                "Hello",
                "B",
                "",
                &qa::parse("Hello").unwrap().signature().canonical
            )
            .unwrap(),
        None
    );
    // 同一键出现两种译法时无法机械择一：两条都不复用，退回模型翻译。
    store
        .import(
            "zh-Hans",
            "en",
            "Hello",
            "Salut",
            "A",
            "",
            &qa::parse("Hello").unwrap().signature().canonical,
            "input-2",
            9,
            false,
        )
        .unwrap();
    let second = engine::prepare_with_store(
        &path,
        dir.path(),
        mapping.clone(),
        &cancel,
        Some(&mut store),
    )
    .unwrap();
    assert_eq!(second.preflight.memory_reused_cells, 0);
    assert_eq!(second.preflight.pending_cells, 2);
    // 记忆变化会让已完成的预检失效，必须重新预检才能导出。
    let entry = store.list_memory().unwrap().remove(0);
    store.disable_memory(entry.id).unwrap();
    assert!(
        engine::export_with_store(
            &second,
            dir.path(),
            &cancel,
            Some(&store),
            crate::engine::ExportMode::Approved,
        )
        .unwrap_err()
        .contains("重新预检")
    );
}

#[test]
fn disabled_memory_stops_reuse() {
    let dir = TestDir::new();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .import(
            "zh-Hans",
            "en",
            "你好",
            "Hello",
            "",
            "",
            &qa::parse("你好").unwrap().signature().canonical,
            "hash",
            2,
            false,
        )
        .unwrap();
    let id = store.list_memory().unwrap()[0].id;
    let path = dir.write("memory.csv", "Source,English\n你好,\n".as_bytes());
    let mut mapping = csvio::inspect(&path).unwrap().default_mapping;
    mapping.source_text_column = 0;
    mapping.target_columns = vec![crate::TargetColumn {
        column: 1,
        locale: Locale { tag: "en".into() },
    }];
    let cancel = AtomicBool::new(false);
    let first = engine::prepare_with_store(
        &path,
        dir.path(),
        mapping.clone(),
        &cancel,
        Some(&mut store),
    )
    .unwrap();
    assert_eq!(first.preflight.memory_reused_cells, 1);
    // 禁用是唯一的"人工闸门"，且它只作用于单条记录：被禁用的记忆不再参与复用。
    store.disable_memory(id).unwrap();
    let second =
        engine::prepare_with_store(&path, dir.path(), mapping, &cancel, Some(&mut store)).unwrap();
    assert_eq!(second.preflight.memory_reused_cells, 0);
    assert_eq!(second.preflight.pending_cells, 1);
}

#[test]
fn p2_signature_and_glossary_exact_only() {
    let dir = TestDir::new();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .import(
            "zh-Hans",
            "en",
            "Hello {name}",
            "Hi {name}",
            "",
            "",
            "signature-a",
            "input",
            2,
            false,
        )
        .unwrap();
    assert_eq!(
        store
            .lookup("zh-Hans", "en", "Hello {name}", "", "", "signature-b")
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .lookup(
                "zh-Hans",
                "en",
                "Hello {name}",
                "different",
                "",
                "signature-a"
            )
            .unwrap(),
        None
    );
    let save = |store: &Store, source: &str, target: &str| {
        store
            .save_term(crate::TermInput {
                id: None,
                kind: crate::TermKind::Ordinary,
                source_locale: Locale {
                    tag: "zh-Hans".into(),
                },
                target_locale: Locale { tag: "en".into() },
                source_text: source.into(),
                target_text: target.into(),
                aliases: vec![],
                context: None,
                resource_key: None,
                disambiguation: String::new(),
                expected_version: None,
            })
            .unwrap()
    };
    let term = save(&store, "苹果", "Apple");
    save(&store, "香蕉", "Banana");
    let path = dir.write(
        "terms.csv",
        "Source,English\n苹果,\n我爱苹果,\n香蕉,\n".as_bytes(),
    );
    let mut mapping = csvio::inspect(&path).unwrap().default_mapping;
    mapping.source_text_column = 0;
    mapping.target_columns = vec![crate::TargetColumn {
        column: 1,
        locale: Locale { tag: "en".into() },
    }];
    let cancel = AtomicBool::new(false);
    let first = engine::prepare_with_store(
        &path,
        dir.path(),
        mapping.clone(),
        &cancel,
        Some(&mut store),
    )
    .unwrap();
    // 词条不设"已审核"闸门，启用即参与：一条命中源文时按词条精确填格。
    assert_eq!(first.preflight.glossary_exact_cells, 2);
    assert_eq!(first.preflight.pending_cells, 1);
    assert!(
        first
            .preflight
            .term_hit_preview
            .iter()
            .any(|h| h.row_number == 3 && h.start_char == 2 && h.end_char == 4)
    );
    save(&store, "苹果", "Pomme");
    let second =
        engine::prepare_with_store(&path, dir.path(), mapping, &cancel, Some(&mut store)).unwrap();
    // 同源词出现两种译名后不再"精确命中"，交回模型处理。
    assert_eq!(second.preflight.glossary_exact_cells, 1);
    store.disable_term(term.id).unwrap();
    assert!(
        engine::export_with_store(
            &second,
            dir.path(),
            &cancel,
            Some(&store),
            crate::engine::ExportMode::Approved,
        )
        .is_err()
    );
}

#[test]
fn p2_imported_fixture_memory_is_enabled_without_approval() {
    let dir = TestDir::new();
    let mut store = Store::open(dir.path()).unwrap();
    let mapping = csvio::inspect(fixture()).unwrap().default_mapping;
    let prepared = engine::prepare_with_store(
        fixture(),
        dir.path(),
        mapping,
        &AtomicBool::new(false),
        Some(&mut store),
    )
    .unwrap();
    assert_eq!(prepared.preflight.pending_cells, 15);
    assert_eq!(prepared.preflight.memory_reused_cells, 0);
    // 导入的既有译文直接可用（enabled），没有"未审核"这种需要人工放行的中间态。
    let entries = store.list_memory().unwrap();
    assert!(!entries.is_empty());
    assert!(entries.iter().all(|entry| entry.enabled));
}

#[test]
fn p2_do_not_translate_alias_and_forbidden_qa() {
    let dir = TestDir::new();
    let mut store = Store::open(dir.path()).unwrap();
    let base = crate::TermInput {
        id: None,
        kind: crate::TermKind::DoNotTranslate,
        source_locale: Locale {
            tag: "zh-Hans".into(),
        },
        target_locale: Locale { tag: "en".into() },
        source_text: "Wonderland".into(),
        target_text: String::new(),
        aliases: vec!["WL".into()],
        context: None,
        resource_key: None,
        disambiguation: "品牌".into(),
        expected_version: None,
    };
    store.save_term(base.clone()).unwrap();
    store
        .save_term(crate::TermInput {
            kind: crate::TermKind::Forbidden,
            source_text: "苹果".into(),
            target_text: "BadApple".into(),
            aliases: vec![],
            ..base
        })
        .unwrap();
    let path = dir.write(
        "kinds.csv",
        b"Source,English\nWL,\n\xe8\x8b\xb9\xe6\x9e\x9c,BadApple\n",
    );
    let mut mapping = csvio::inspect(&path).unwrap().default_mapping;
    mapping.source_text_column = 0;
    mapping.target_columns = vec![crate::TargetColumn {
        column: 1,
        locale: Locale { tag: "en".into() },
    }];
    let prepared = engine::prepare_with_store(
        &path,
        dir.path(),
        mapping,
        &AtomicBool::new(false),
        Some(&mut store),
    )
    .unwrap();
    assert_eq!(prepared.preflight.glossary_exact_cells, 1);
    assert!(
        prepared
            .preflight
            .issue_preview
            .iter()
            .any(|i| i.code == "forbidden_term")
    );
}

#[test]
fn orphan_and_internal_error_never_leave_a_job_running() {
    let dir = TestDir::new();
    let store = Store::open(dir.path()).unwrap();
    store.conn().execute("INSERT INTO jobs(input_hash,provider_fingerprint,memory_version,glossary_version,status,limits_json,created_at,updated_at) VALUES('hash','fp',0,0,'running','{}',0,0)", []).unwrap();
    let job_id = store.conn().last_insert_rowid();
    store.conn().execute("INSERT INTO job_units(job_id,row_number,target_column,target_locale,source_locale,source_text,context,resource_key,signature,group_key,status,attempt,updated_at) VALUES(?1,2,1,'en','zh-Hans','Hello','','','','key','running',1,0)", [job_id]).unwrap();
    let job_status = |store: &Store, job_id: i64| -> String {
        store
            .conn()
            .query_row("SELECT status FROM jobs WHERE id=?1", [job_id], |row| {
                row.get(0)
            })
            .unwrap()
    };

    // 进程被杀或作业异常退出后会留下 `running`：界面会显示"运行中"，而暂停必然报
    // "当前没有进行中的翻译作业"。这条兜底把它收成"已暂停"，用户仍可点"继续"。
    assert_eq!(crate::job::pause_orphan_running(&store).unwrap(), 1);
    assert_eq!(job_status(&store, job_id), "paused");
    // 收口后原来的 running 格不算在飞：续跑会把它们退回待处理。
    assert_eq!(
        crate::job::requeue_stale_running(&store, job_id).unwrap(),
        1
    );

    // 调度自己出错时的兜底：作业与未结算的格一起记为失败，绝不留运行中。
    store
        .conn()
        .execute("UPDATE jobs SET status='running' WHERE id=?1", [job_id])
        .unwrap();
    crate::job::settle_after_error(&store, job_id, "测试用内部错误").unwrap();
    assert_eq!(job_status(&store, job_id), "failed");
    let (unit_status, unit_qa): (String, String) = store
        .conn()
        .query_row(
            "SELECT status,COALESCE(qa_state,'') FROM job_units WHERE job_id=?1",
            [job_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(unit_status, "failed");
    assert_eq!(unit_qa, "测试用内部错误");
    // 失败格照旧能由"继续"重排，兜底不会把用户的路堵死。
    assert_eq!(crate::job::requeue_failed_units(&store, job_id).unwrap(), 1);
}

#[test]
fn reopening_store_repairs_missing_tables_without_touching_the_version() {
    let dir = TestDir::new();
    {
        let store = Store::open(dir.path()).unwrap();
        // 建表语句每次 open 都跑（幂等），所以被删掉的表应当被补回来。
        store.conn().execute_batch("DROP TABLE terms;").unwrap();
    }
    let store = Store::open(dir.path()).unwrap();
    store
        .save_term(crate::TermInput {
            id: None,
            kind: crate::TermKind::Ordinary,
            source_locale: Locale {
                tag: "zh-Hans".into(),
            },
            target_locale: Locale { tag: "en".into() },
            source_text: "苹果".into(),
            target_text: "Apple".into(),
            aliases: vec![],
            context: None,
            resource_key: None,
            disambiguation: String::new(),
            expected_version: None,
        })
        .unwrap();
    let version: i64 = store
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 8);
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// P1–P3 测试共用的临时目录；P3 用例见 `tests_p3`。
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "wonderland-translator-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
    pub(crate) fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let inputs = self.0.join("translator").join("inputs");
        #[cfg(windows)]
        if let Ok(files) = fs::read_dir(inputs) {
            for file in files.flatten() {
                if let Ok(meta) = file.metadata() {
                    let mut permissions = meta.permissions();
                    // Windows 测试快照必须清掉只读位才能移除；此分支不在 Unix 编译。
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                    let _ = fs::set_permissions(file.path(), permissions);
                }
            }
        }
        #[cfg(not(windows))]
        let _ = inputs;
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/micro.csv"))
}

#[test]
fn micro_fixture_preflight_and_export_preserve_cells() {
    let dir = TestDir::new();
    let original = fs::read(fixture()).unwrap();
    let inspection = csvio::inspect(fixture()).unwrap();
    assert!(inspection.has_bom);
    assert_eq!(inspection.headers.len(), 7);
    assert_eq!(inspection.default_mapping.target_columns.len(), 2);
    let cancel = AtomicBool::new(false);
    let prepared =
        engine::prepare(fixture(), dir.path(), inspection.default_mapping, &cancel).unwrap();
    let stats = &prepared.preflight;
    assert_eq!(
        (stats.total_rows, stats.selected_rows, stats.pending_cells),
        (9, 8, 15)
    );
    assert_eq!(
        (
            stats.existing_cells,
            stats.empty_source_rows,
            stats.format_rows
        ),
        (1, 0, 2)
    );
    assert_eq!((stats.newline_rows, stats.carriage_return_rows), (1, 1));
    assert_eq!(stats.unique_units, 15);
    assert_eq!(stats.issue_count, 15);
    assert!(
        stats
            .issue_preview
            .iter()
            .all(|issue| issue.code == "untranslated_required")
    );
    let result = engine::export(&prepared, dir.path(), &cancel).unwrap();
    let exported = fs::read(&result.csv_path).unwrap();
    assert!(exported.starts_with(&[0xef, 0xbb, 0xbf]));
    assert_eq!(fs::read(fixture()).unwrap(), original);
    let mut original_reader = csvio::open_reader(fixture()).unwrap();
    let mut output_reader = csvio::open_reader(Path::new(&result.csv_path)).unwrap();
    assert_eq!(
        csvio::headers(&mut original_reader).unwrap(),
        csvio::headers(&mut output_reader).unwrap()
    );
    let input_rows: Vec<_> = original_reader.records().map(Result::unwrap).collect();
    let output_rows: Vec<_> = output_reader.records().map(Result::unwrap).collect();
    assert_eq!(input_rows, output_rows);
    assert_eq!(output_rows[1].get(2), Some("第一行\r\n第二行"));
    assert_eq!(output_rows[2].get(5), Some("保留C"));
    assert_eq!(output_rows[7].get(3), Some("Existing translation"));
    assert_eq!(std::fs::read_dir(&result.directory).unwrap().count(), 1);
}

#[test]
fn rejects_bad_csv_and_missing_targets() {
    let dir = TestDir::new();
    for (name, bytes) in [
        ("empty.csv", &b""[..]),
        ("duplicate.csv", &b"Source,Source,English\na,,\n"[..]),
        ("invalid-utf8.csv", &b"Source,English\n\xff,\n"[..]),
        ("quote.csv", &b"Source,English\na\"b,\n"[..]),
        ("unclosed.csv", &b"Source,English\n\"abc,\n"[..]),
        ("columns.csv", &b"Source,English\na,b,c\n"[..]),
        ("no-rows.csv", &b"Source,English\n"[..]),
    ] {
        let path = dir.write(name, bytes);
        if let Ok(inspection) = csvio::inspect(&path) {
            let mut mapping = inspection.default_mapping;
            mapping.source_text_column = 0;
            mapping.target_columns = vec![crate::TargetColumn {
                column: 1,
                locale: Locale { tag: "en".into() },
            }];
            assert!(
                engine::prepare(&path, dir.path(), mapping, &AtomicBool::new(false)).is_err(),
                "{name}"
            );
        }
    }
    let mut mapping = csvio::inspect(fixture()).unwrap().default_mapping;
    mapping.target_columns.clear();
    let error = engine::prepare(fixture(), dir.path(), mapping, &AtomicBool::new(false))
        .err()
        .unwrap();
    assert!(error.contains("没有目标语言列"));
    let large = format!("Source,English\n{},\n", "a".repeat(1024 * 1024 + 1));
    let path = dir.write("large.csv", large.as_bytes());
    let mut mapping = csvio::inspect(&path).unwrap().default_mapping;
    mapping.target_columns = vec![crate::TargetColumn {
        column: 1,
        locale: Locale { tag: "en".into() },
    }];
    assert!(
        engine::prepare(&path, dir.path(), mapping, &AtomicBool::new(false))
            .err()
            .unwrap()
            .contains("1 MiB")
    );
}

#[test]
fn qa_checks_identity_structure_and_nonblocking_risks() {
    let source = qa::parse("<color=#ff0000>你好{1:s.昵称}</color>\\n").unwrap();
    let reordered = qa::parse("<color=#ff0000>{1:s.昵称}您好</color>\\n").unwrap();
    let locale = Locale { tag: "en".into() };
    assert!(qa::compare(&source, &reordered, 2, &locale).is_empty());
    let wrong = qa::parse("<color=#00ff00>Hi{2:s.昵称}</color>").unwrap();
    let issues = qa::compare(&source, &wrong, 2, &locale);
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == "placeholder_mismatch"
                && issue.severity == IssueSeverity::Blocking)
    );
    assert!(issues.iter().any(|issue| issue.code == "tag_mismatch"));
    assert!(
        issues
            .iter()
            .any(|issue| issue.code == "literal_newline_mismatch")
    );
    assert!(qa::parse("<color=red><b>x</color></b>").is_ok());
    assert!(qa::parse("x{unterminated").is_err());
    assert!(
        qa::parse("<custom>普通文字</custom>").unwrap().signature()
            == qa::parse("普通文字").unwrap().signature()
    );
    assert!(qa::parse("<size=18><i>x</i></size>").is_ok());
    assert!(qa::parse("<color=#12345678>x</color>").is_ok());
}

/// 上游按变量/标签把一句切成多格时，单元格会带上这些形态；普通正文不能被误判。
#[test]
fn fragment_shapes_are_recognized_without_flagging_ordinary_text() {
    for text in [
        "</color>",
        "<color=#FFD780FF>",
        "</color>使用了<color=#FFD780FF>【5】王子",
        "，无事发生",
        "。",
        "{1:s.昵称}",
        "   ",
        "<color=#FFD780FF><size=24>{1:lv.玩家昵称表.0}</size></color>",
    ] {
        assert!(qa::fragment_reason(text).is_some(), "应判为片段：{text}");
    }
    // 半句的漏报是接受的代价：这些判定只看单元格自身，不推断相邻格属于哪一句。
    for text in [
        "手牌较大的玩家将",
        "你好{1:s.昵称}",
        "<color=#FFD780FF>完整句</color>",
        "<custom>普通文字</custom>",
        "a < b > c",
        ".NET 运行时",
        "1 张｜与任意玩家交换手牌。",
    ] {
        assert!(qa::fragment_reason(text).is_none(), "不应判为片段：{text}");
    }
}

/// 预检要指出疑似被切分的片段，但只提示：既不阻断导出，也不改变派发。
#[test]
fn preflight_flags_fragment_shaped_cells_without_blocking() {
    let dir = TestDir::new();
    let path = dir.write(
        "fragments.csv",
        "Source,English\n\
         </color>使用了【5】王子,\n\
         手牌较大的玩家将,\n\
         ，无事发生,\n\
         {1:s.昵称},\n\
         <color=#FFD780FF>【2】牧师</color>揭示了<color=#FFD780FF>,\n\
         <color=#FFD780FF>完整句</color>,\n"
            .as_bytes(),
    );
    let mut mapping = csvio::inspect(&path).unwrap().default_mapping;
    mapping.source_text_column = 0;
    mapping.selection_column = None;
    mapping.target_columns = vec![crate::TargetColumn {
        column: 1,
        locale: Locale { tag: "en".into() },
    }];
    let prepared = engine::prepare(&path, dir.path(), mapping, &AtomicBool::new(false)).unwrap();
    let stats = &prepared.preflight;
    let flagged: Vec<u64> = stats
        .issue_preview
        .iter()
        .filter(|issue| issue.code == "suspected_fragment")
        .map(|issue| issue.row_number)
        .collect();
    assert_eq!(flagged, vec![2, 4, 5, 6]);
    assert!(
        stats
            .issue_preview
            .iter()
            .filter(|issue| issue.code == "suspected_fragment")
            .all(|issue| issue.severity == IssueSeverity::Review)
    );
    // 片段只提示，不改派发：六格照旧都是待填。
    assert_eq!((stats.pending_cells, stats.unique_units), (6, 6));
}

#[test]
fn product_header_accepts_subset_of_languages() {
    let dir = TestDir::new();
    let header = "来源,是否需要翻译,简体中文,繁体中文,英语,韩语,日语,西班牙语,法语,俄语,泰语,越南语,德语,印尼语,葡萄牙语,土耳其语,意大利语\n";
    let path = dir.write("fixed.csv", header.as_bytes());
    let inspection = csvio::inspect(&path).unwrap();
    assert!(csvio::validate_product_format(&inspection).is_ok());
    let english = dir.write(
        "english.csv",
        "来源,是否需要翻译,简体中文,英语\n剧情,TRUE,你好,\n".as_bytes(),
    );
    let inspection = csvio::inspect(&english).unwrap();
    assert!(csvio::validate_product_format(&inspection).is_ok());
    assert_eq!(inspection.default_mapping.target_columns.len(), 1);
    let prepared = engine::prepare(
        &english,
        dir.path(),
        inspection.default_mapping,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(prepared.preflight.pending_cells, 1);
    let result = engine::export(&prepared, dir.path(), &AtomicBool::new(false)).unwrap();
    let mut reader = csvio::open_reader(Path::new(&result.csv_path)).unwrap();
    assert_eq!(
        csvio::headers(&mut reader).unwrap(),
        ["来源", "是否需要翻译", "简体中文", "英语"]
    );
    let changed = header.replacen("来源,是否需要翻译", "是否需要翻译,来源", 1);
    let path = dir.write("changed.csv", changed.as_bytes());
    assert!(csvio::validate_product_format(&csvio::inspect(&path).unwrap()).is_err());
    let unknown = dir.write(
        "unknown.csv",
        "来源,是否需要翻译,简体中文,未知语\n剧情,TRUE,你好,\n".as_bytes(),
    );
    assert!(csvio::validate_product_format(&csvio::inspect(&unknown).unwrap()).is_err());
}

#[test]
fn preflight_reports_existing_translation_format_and_scope_risks() {
    let dir = TestDir::new();
    let data = "简体中文,英语,上下文,是否需要翻译\n\
        你好{1:s.昵称} 2,Hello{2:s.昵称} 3,同一处,TRUE\n\
        你好{1:s.昵称} 2,Hi{1:s.昵称} 2,同一处,TRUE\n\
        <color=#ff0000>x</color>,<color=#0000ff>y</color>,同一处,TRUE\n";
    let path = dir.write("qa.csv", data.as_bytes());
    let inspection = csvio::inspect(&path).unwrap();
    let prepared = engine::prepare(
        &path,
        dir.path(),
        inspection.default_mapping,
        &AtomicBool::new(false),
    )
    .unwrap();
    let codes: Vec<_> = prepared
        .preflight
        .issue_preview
        .iter()
        .map(|issue| issue.code.as_str())
        .collect();
    assert!(codes.contains(&"placeholder_mismatch"));
    assert!(codes.contains(&"number_mismatch"));
    assert!(codes.contains(&"scope_conflict"));
    assert!(codes.contains(&"tag_mismatch"));
    let export = engine::export(&prepared, dir.path(), &AtomicBool::new(false)).unwrap();
    assert!(Path::new(&export.csv_path).exists());
}

#[test]
fn cancellation_and_unwritable_output_do_not_publish_result() {
    let dir = TestDir::new();
    let mapping = csvio::inspect(fixture()).unwrap().default_mapping;
    let cancel = AtomicBool::new(true);
    assert!(
        engine::prepare(fixture(), dir.path(), mapping.clone(), &cancel)
            .err()
            .unwrap()
            .contains("取消")
    );
    cancel.store(false, Ordering::Relaxed);
    let prepared = engine::prepare(fixture(), dir.path(), mapping, &cancel).unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert!(
        engine::export(&prepared, dir.path(), &cancel)
            .err()
            .unwrap()
            .contains("取消")
    );
    cancel.store(false, Ordering::Relaxed);
    fs::write(dir.path().join("翻译导出"), b"occupied").unwrap();
    assert!(
        engine::export(&prepared, dir.path(), &cancel)
            .err()
            .unwrap()
            .contains("输出目录不可写")
    );
}

/// 本机参考文件不进入仓库；设置环境变量后可重跑真实结构基线。
#[test]
fn reference_csv_baseline_if_configured() {
    let Some(path) = std::env::var_os("TRANSLATOR_REFERENCE_CSV") else {
        return;
    };
    let path = PathBuf::from(path);
    let dir = TestDir::new();
    let original = fs::read(&path).unwrap();
    let inspection = csvio::inspect(&path).unwrap();
    let cancel = AtomicBool::new(false);
    let mut store = Store::open(dir.path()).unwrap();
    let prepared = engine::prepare_with_store(
        &path,
        dir.path(),
        inspection.default_mapping,
        &cancel,
        Some(&mut store),
    )
    .unwrap();
    let stats = &prepared.preflight;
    println!(
        "reference baseline: rows={}, selected={}, pending={}, format_rows={}, newline_rows={}, carriage_return_rows={}, issues={}",
        stats.total_rows,
        stats.selected_rows,
        stats.pending_cells,
        stats.format_rows,
        stats.newline_rows,
        stats.carriage_return_rows,
        stats.issue_count
    );
    assert_eq!(
        (stats.total_rows, stats.selected_rows, stats.pending_cells),
        (394, 140, 1960)
    );
    assert_eq!(stats.targets.len(), 14);
    assert_eq!(
        (
            stats.format_rows,
            stats.newline_rows,
            stats.carriage_return_rows
        ),
        (51, 18, 13)
    );
    assert_eq!(stats.memory_reused_cells, 0);
    let result = engine::export_with_store(
        &prepared,
        dir.path(),
        &cancel,
        Some(&store),
        crate::engine::ExportMode::Approved,
    )
    .unwrap();
    let mut source_reader = csvio::open_reader(&path).unwrap();
    let mut result_reader = csvio::open_reader(Path::new(&result.csv_path)).unwrap();
    assert_eq!(
        csvio::headers(&mut source_reader).unwrap(),
        csvio::headers(&mut result_reader).unwrap()
    );
    assert!(
        source_reader
            .records()
            .map(Result::unwrap)
            .eq(result_reader.records().map(Result::unwrap))
    );
    assert_eq!(fs::read(&path).unwrap(), original);
}
