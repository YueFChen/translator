use crate::config::ProviderSettings;
use crate::qa;
use crate::{GlossaryTerm, Locale, MemoryEntry, TermInput, TermKind};
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::Duration;

/// 当前 schema 版本。版本不符的旧库不迁移：改名备份后新建一份空的。
const SCHEMA_VERSION: i64 = 8;

/// 写锁等待上限。界面命令与作业各持一条连接，多等一会儿比把作业判成数据库错误划算。
const BUSY_TIMEOUT_SECONDS: u64 = 5;

/// 幂等的建表语句。表与索引都在时不会产生写入，因此可以放在每次 `open` 的常规路径上。
const SCHEMA: &str = "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
   CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
   CREATE TABLE IF NOT EXISTS memory (id INTEGER PRIMARY KEY, source_locale TEXT NOT NULL, target_locale TEXT NOT NULL, source_text TEXT NOT NULL, target_text TEXT NOT NULL, context TEXT NOT NULL, resource_key TEXT NOT NULL, signature TEXT NOT NULL, source_kind TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, input_hash TEXT, row_number INTEGER, qa_blocking INTEGER NOT NULL, version INTEGER NOT NULL DEFAULT 1);
   CREATE UNIQUE INDEX IF NOT EXISTS memory_import ON memory(input_hash,row_number,target_locale) WHERE source_kind='external_history';
   CREATE INDEX IF NOT EXISTS memory_lookup ON memory(source_locale,target_locale,source_text,context,resource_key,signature,enabled);
   CREATE TABLE IF NOT EXISTS terms (id INTEGER PRIMARY KEY,kind TEXT NOT NULL,source_locale TEXT NOT NULL,target_locale TEXT NOT NULL,source_text TEXT NOT NULL,target_text TEXT NOT NULL,aliases TEXT NOT NULL,context TEXT,resource_key TEXT,disambiguation TEXT NOT NULL,enabled INTEGER NOT NULL DEFAULT 1,version INTEGER NOT NULL DEFAULT 1);
   CREATE TABLE IF NOT EXISTS provider_config (id INTEGER PRIMARY KEY CHECK(id=1), base_url TEXT NOT NULL, model TEXT NOT NULL, allow_loopback INTEGER NOT NULL, timeout_seconds INTEGER NOT NULL, requests_per_minute INTEGER, price_input REAL, price_output REAL, currency TEXT);
   CREATE TABLE IF NOT EXISTS work_records (id INTEGER PRIMARY KEY, file_name TEXT NOT NULL, title TEXT, input_hash TEXT NOT NULL, mapping_json TEXT NOT NULL, created_at INTEGER NOT NULL);
   CREATE TABLE IF NOT EXISTS provider_profiles (id INTEGER PRIMARY KEY, name TEXT NOT NULL, config_json TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 0);
   CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY, work_id INTEGER REFERENCES work_records(id), input_hash TEXT NOT NULL, provider_fingerprint TEXT NOT NULL, memory_version INTEGER NOT NULL, glossary_version INTEGER NOT NULL, status TEXT NOT NULL, limits_json TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
   CREATE TABLE IF NOT EXISTS job_units (id INTEGER PRIMARY KEY, job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE, row_number INTEGER NOT NULL, target_column INTEGER NOT NULL, target_locale TEXT NOT NULL, source_locale TEXT NOT NULL, source_text TEXT NOT NULL, context TEXT NOT NULL, resource_key TEXT NOT NULL, signature TEXT NOT NULL, group_key TEXT NOT NULL, status TEXT NOT NULL, attempt INTEGER NOT NULL DEFAULT 0, target_text TEXT, qa_state TEXT, updated_at INTEGER NOT NULL, UNIQUE(job_id,row_number,target_column));
   CREATE INDEX IF NOT EXISTS job_units_pick ON job_units(job_id,status,id);
   CREATE INDEX IF NOT EXISTS job_units_group ON job_units(job_id,group_key,status,id);
   CREATE TABLE IF NOT EXISTS job_usage (job_id INTEGER PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE, requests INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0);
   CREATE TABLE IF NOT EXISTS job_pricing (job_id INTEGER PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE, price_input REAL, price_output REAL, currency TEXT);";

pub struct Store {
    db: Connection,
}

fn err(e: impl std::fmt::Display) -> String {
    format!("翻译记忆数据库错误：{e}")
}
fn kind(value: &str) -> TermKind {
    match value {
        "do_not_translate" => TermKind::DoNotTranslate,
        "forbidden" => TermKind::Forbidden,
        _ => TermKind::Ordinary,
    }
}
fn kind_str(value: TermKind) -> &'static str {
    match value {
        TermKind::Ordinary => "ordinary",
        TermKind::DoNotTranslate => "do_not_translate",
        TermKind::Forbidden => "forbidden",
    }
}

impl Store {
    pub fn begin_import(&self) -> Result<(), String> {
        self.db.execute_batch("BEGIN IMMEDIATE").map_err(err)
    }
    pub fn finish_import(&self) -> Result<(), String> {
        self.db.execute_batch("COMMIT").map_err(err)
    }
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let dir = data_dir.join("translator");
        std::fs::create_dir_all(&dir).map_err(err)?;
        let path = dir.join("project.sqlite");
        if path.exists() {
            let old = Connection::open(&path).map_err(err)?;
            let version: i64 = old
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .map_err(err)?;
            if version != SCHEMA_VERSION {
                old.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
                    .map_err(err)?;
                drop(old);
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(err)?
                    .as_millis();
                std::fs::rename(
                    &path,
                    dir.join(format!("project-v{version}-{stamp}.sqlite")),
                )
                .map_err(err)?;
            }
        }
        let db = Connection::open(&path).map_err(err)?;
        // 界面命令与跑着的作业各持一条连接，撞上写锁时应当等一会儿再重试，而不是立刻报
        // `database is locked`——后者会把一个正常作业判成"数据库错误"而中断。
        db.busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECONDS))
            .map_err(err)?;
        // 建表语句是幂等的：表与索引都在时只是解析一遍，不产生写入。
        db.execute_batch(SCHEMA).map_err(err)?;
        // 只有版本不符（新库或刚改名备份过）才写 meta 初值与库头里的版本号。
        // 这两条都是写入语句：放在 `open` 的常规路径上，会让"只读"的界面命令也去抢写锁。
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(err)?;
        if version != SCHEMA_VERSION {
            db.execute_batch(&format!(
                "INSERT OR IGNORE INTO meta VALUES ('memory',0),('glossary',0); PRAGMA user_version={SCHEMA_VERSION};"
            ))
            .map_err(err)?;
        }
        Ok(Self { db })
    }

    /// 作业模块需要直接读写 jobs/job_units/job_usage；连接仍由本类型独占持有。
    pub(crate) fn conn(&self) -> &Connection {
        &self.db
    }
    fn bump(&self, key: &str) -> Result<(), String> {
        self.db
            .execute("UPDATE meta SET value=value+1 WHERE key=?1", [key])
            .map_err(err)?;
        Ok(())
    }
    pub fn versions(&self) -> Result<(u64, u64), String> {
        let get = |key| {
            self.db
                .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| {
                    r.get::<_, i64>(0).map(|v| v as u64)
                })
                .map_err(err)
        };
        Ok((get("memory")?, get("glossary")?))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn import(
        &mut self,
        source_locale: &str,
        target_locale: &str,
        source: &str,
        target: &str,
        context: &str,
        resource: &str,
        signature: &str,
        input_hash: &str,
        row: u64,
        blocking: bool,
    ) -> Result<(), String> {
        let changed=self.db.execute("INSERT OR IGNORE INTO memory(source_locale,target_locale,source_text,target_text,context,resource_key,signature,source_kind,input_hash,row_number,qa_blocking) VALUES(?1,?2,?3,?4,?5,?6,?7,'external_history',?8,?9,?10)",params![source_locale,target_locale,source,target,context,resource,signature,input_hash,row as i64,blocking]).map_err(err)?;
        if changed > 0 {
            self.bump("memory")?
        }
        Ok(())
    }
    /// 记忆复用**不设人工审批闸门**：只要启用、且格式校验通过，就按唯一性规则直接复用。
    ///
    /// 同一键下存在多种译法时返回 `None`——多条候选无法机械地择一，交给模型重译。
    pub fn lookup(
        &self,
        source_locale: &str,
        target_locale: &str,
        source: &str,
        context: &str,
        resource: &str,
        signature: &str,
    ) -> Result<Option<String>, String> {
        let mut stmt=self.db.prepare("SELECT DISTINCT target_text FROM memory WHERE source_locale=?1 AND target_locale=?2 AND source_text=?3 AND context=?4 AND resource_key=?5 AND signature=?6 AND enabled=1 AND qa_blocking=0 LIMIT 2").map_err(err)?;
        let rows = stmt
            .query_map(
                params![
                    source_locale,
                    target_locale,
                    source,
                    context,
                    resource,
                    signature
                ],
                |r| r.get::<_, String>(0),
            )
            .map_err(err)?;
        let values = rows.collect::<Result<Vec<_>, _>>().map_err(err)?;
        Ok(if values.len() == 1 {
            values.into_iter().next()
        } else {
            None
        })
    }
    pub fn list_memory(&self) -> Result<Vec<MemoryEntry>, String> {
        let mut stmt=self.db.prepare("SELECT id,source_locale,target_locale,source_text,target_text,context,resource_key,signature,source_kind,enabled,input_hash,row_number,qa_blocking,version FROM memory ORDER BY id DESC").map_err(err)?;
        stmt.query_map([], |r| {
            Ok(MemoryEntry {
                id: r.get(0)?,
                source_locale: Locale { tag: r.get(1)? },
                target_locale: Locale { tag: r.get(2)? },
                source_text: r.get(3)?,
                target_text: r.get(4)?,
                context: r.get(5)?,
                resource_key: r.get(6)?,
                placeholder_signature: r.get(7)?,
                source_kind: r.get(8)?,
                enabled: r.get(9)?,
                input_hash: r.get(10)?,
                row_number: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
                qa_blocking: r.get(12)?,
                version: r.get::<_, i64>(13)? as u64,
            })
        })
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)
    }
    pub fn disable_memory(&self, id: i64) -> Result<(), String> {
        let n = self
            .db
            .execute(
                "UPDATE memory SET enabled=0,version=version+1 WHERE id=?1 AND enabled=1",
                [id],
            )
            .map_err(err)?;
        if n == 0 {
            return Err("译文记录不存在或已禁用".into());
        }
        self.bump("memory")
    }
    pub fn list_terms(&self) -> Result<Vec<GlossaryTerm>, String> {
        let mut stmt=self.db.prepare("SELECT id,kind,source_locale,target_locale,source_text,target_text,aliases,context,resource_key,disambiguation,enabled,version FROM terms ORDER BY id DESC").map_err(err)?;
        stmt.query_map([], |r| {
            let raw: String = r.get(6)?;
            Ok(GlossaryTerm {
                id: r.get(0)?,
                kind: kind(&r.get::<_, String>(1)?),
                source_locale: Locale { tag: r.get(2)? },
                target_locale: Locale { tag: r.get(3)? },
                source_text: r.get(4)?,
                target_text: r.get(5)?,
                aliases: serde_json::from_str(&raw).unwrap_or_default(),
                context: r.get(7)?,
                resource_key: r.get(8)?,
                disambiguation: r.get(9)?,
                enabled: r.get(10)?,
                version: r.get::<_, i64>(11)? as u64,
            })
        })
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)
    }
    pub fn save_term(&self, input: TermInput) -> Result<GlossaryTerm, String> {
        if input.source_text.trim().is_empty()
            || (input.kind == TermKind::Ordinary && input.target_text.trim().is_empty())
            || input.source_locale.tag.is_empty()
            || input.target_locale.tag.is_empty()
        {
            return Err("词条源文、目标语言或译名不能为空".into());
        }
        let aliases: Vec<String> = input
            .aliases
            .into_iter()
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty())
            .collect();
        let json = serde_json::to_string(&aliases).map_err(err)?;
        let id = if let Some(id) = input.id {
            let expected = input.expected_version.ok_or("编辑词条缺少版本")?;
            let n=self.db.execute("UPDATE terms SET kind=?1,source_locale=?2,target_locale=?3,source_text=?4,target_text=?5,aliases=?6,context=?7,resource_key=?8,disambiguation=?9,enabled=1,version=version+1 WHERE id=?10 AND version=?11",params![kind_str(input.kind),input.source_locale.tag,input.target_locale.tag,input.source_text,input.target_text,json,input.context,input.resource_key,input.disambiguation,id,expected as i64]).map_err(err)?;
            if n == 0 {
                return Err("词条版本已变化，请刷新后重试".into());
            }
            id
        } else {
            self.db.execute("INSERT INTO terms(kind,source_locale,target_locale,source_text,target_text,aliases,context,resource_key,disambiguation) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![kind_str(input.kind),input.source_locale.tag,input.target_locale.tag,input.source_text,input.target_text,json,input.context,input.resource_key,input.disambiguation]).map_err(err)?;
            self.db.last_insert_rowid()
        };
        self.bump("glossary")?;
        self.list_terms()?
            .into_iter()
            .find(|t| t.id == id)
            .ok_or("词条保存后读取失败".into())
    }
    pub fn disable_term(&self, id: i64) -> Result<(), String> {
        let n = self
            .db
            .execute(
                "UPDATE terms SET enabled=0,version=version+1 WHERE id=?1 AND enabled=1",
                [id],
            )
            .map_err(err)?;
        if n == 0 {
            return Err("词条不存在或已禁用".into());
        }
        self.bump("glossary")
    }

    /// 读取模型服务配置；未配置时返回默认值（地址与模型名皆空）。
    pub fn load_provider(&self) -> Result<ProviderSettings, String> {
        let row = self
            .db
            .query_row(
                "SELECT base_url,model,allow_loopback,timeout_seconds,requests_per_minute,price_input,price_output,currency FROM provider_config WHERE id=1",
                [],
                |r| {
                    Ok(ProviderSettings {
                        base_url: r.get(0)?,
                        model: r.get(1)?,
                        allow_loopback: r.get(2)?,
                        timeout_seconds: r.get::<_, i64>(3)? as u32,
                        requests_per_minute: r.get::<_, Option<i64>>(4)?.map(|v| v as u32),
                        price_per_million_input_tokens: r.get(5)?,
                        price_per_million_output_tokens: r.get(6)?,
                        currency: r.get(7)?,
                    })
                },
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(err(other)),
            })?;
        Ok(row.unwrap_or_default())
    }

    pub fn save_provider(&self, settings: &ProviderSettings) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO provider_config(id,base_url,model,allow_loopback,timeout_seconds,requests_per_minute,price_input,price_output,currency) VALUES(1,?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(id) DO UPDATE SET base_url=?1,model=?2,allow_loopback=?3,timeout_seconds=?4,requests_per_minute=?5,price_input=?6,price_output=?7,currency=?8",
                params![
                    settings.base_url,
                    settings.model,
                    settings.allow_loopback,
                    settings.timeout_seconds as i64,
                    settings.requests_per_minute.map(|v| v as i64),
                    settings.price_per_million_input_tokens,
                    settings.price_per_million_output_tokens,
                    settings.currency,
                ],
            )
            .map_err(err)?;
        Ok(())
    }
}

pub fn safe_translation(source: &str, target: &str) -> bool {
    let (Ok(a), Ok(b)) = (qa::parse(source), qa::parse(target)) else {
        return false;
    };
    !qa::compare(&a, &b, 0, &Locale { tag: String::new() })
        .iter()
        .any(|i| i.severity == crate::IssueSeverity::Blocking)
}
