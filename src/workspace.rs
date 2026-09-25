use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::csvio;
use crate::store::Store;
use crate::{ColumnMapping, CsvPage, JobStatus, WorkJob, WorkRecord, WorkTree};

fn err(error: impl std::fmt::Display) -> String {
    format!("翻译工作记录错误：{error}")
}

pub fn add_record(
    store: &Store,
    file_name: &str,
    hash: &str,
    mapping: &ColumnMapping,
) -> Result<i64, String> {
    let mapping = serde_json::to_string(mapping).map_err(err)?;
    store.conn().execute(
        "INSERT INTO work_records(file_name,input_hash,mapping_json,created_at) VALUES(?1,?2,?3,strftime('%s','now'))",
        params![file_name, hash, mapping],
    ).map_err(err)?;
    Ok(store.conn().last_insert_rowid())
}

pub fn record(store: &Store, id: i64) -> Result<(WorkRecord, ColumnMapping), String> {
    let (file_name, title, input_hash, mapping_json, created_at): (String, Option<String>, String, String, i64) = store.conn().query_row(
        "SELECT file_name,title,input_hash,mapping_json,created_at FROM work_records WHERE id=?1",
        [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    ).map_err(err)?;
    let mapping = serde_json::from_str(&mapping_json).map_err(err)?;
    Ok((
        WorkRecord {
            id,
            file_name,
            title,
            input_hash,
            created_at,
        },
        mapping,
    ))
}

pub fn tree(store: &Store) -> Result<WorkTree, String> {
    let mut records_stmt = store
        .conn()
        .prepare(
            "SELECT id,file_name,title,input_hash,created_at FROM work_records ORDER BY id DESC",
        )
        .map_err(err)?;
    let records = records_stmt
        .query_map([], |row| {
            Ok(WorkRecord {
                id: row.get(0)?,
                file_name: row.get(1)?,
                title: row.get(2)?,
                input_hash: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    let mut jobs_stmt = store.conn().prepare("SELECT id,work_id,status,created_at FROM jobs WHERE work_id IS NOT NULL ORDER BY id DESC").map_err(err)?;
    let jobs = jobs_stmt
        .query_map([], |row| {
            let status: String = row.get(2)?;
            Ok(WorkJob {
                id: row.get(0)?,
                work_id: row.get(1)?,
                status: match status.as_str() {
                    "queued" => JobStatus::Queued,
                    "running" => JobStatus::Running,
                    "paused" => JobStatus::Paused,
                    "failed" => JobStatus::Failed,
                    "cancelled" => JobStatus::Cancelled,
                    _ => JobStatus::Succeeded,
                },
                created_at: row.get(3)?,
            })
        })
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    Ok(WorkTree { records, jobs })
}

pub fn rename_record(store: &Store, id: i64, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err("工作名称需为 1–120 个字符".into());
    }
    if store
        .conn()
        .execute(
            "UPDATE work_records SET title=?1 WHERE id=?2",
            params![name, id],
        )
        .map_err(err)?
        != 1
    {
        return Err("CSV 工作记录不存在".into());
    }
    Ok(())
}

pub fn csv_page(
    store: &Store,
    data_dir: &Path,
    work_id: i64,
    job_id: Option<i64>,
    offset: u64,
    limit: u64,
) -> Result<CsvPage, String> {
    let (record, _) = record(store, work_id)?;
    if let Some(job_id) = job_id {
        let owner: Option<i64> = store
            .conn()
            .query_row("SELECT work_id FROM jobs WHERE id=?1", [job_id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(err)?;
        if owner != Some(work_id) {
            return Err("翻译任务不属于选中的 CSV".into());
        }
    }
    let path = data_dir
        .join("translator")
        .join("inputs")
        .join(format!("{}.csv", record.input_hash));
    let mut reader = csvio::open_reader(&path)?;
    let headers = csvio::headers(&mut reader)?;
    let limit = limit.clamp(1, 200);
    let mut rows = Vec::new();
    for row in reader
        .records()
        .skip(offset as usize)
        .take(limit as usize + 1)
    {
        rows.push(
            row.map_err(err)?
                .iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        );
    }
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    if let Some(job_id) = job_id {
        let mut statement = store.conn().prepare("SELECT row_number,target_column,target_text FROM job_units WHERE job_id=?1 AND status='succeeded' AND target_text IS NOT NULL AND row_number BETWEEN ?2 AND ?3").map_err(err)?;
        let translated = statement
            .query_map(
                params![
                    job_id,
                    offset as i64 + 2,
                    offset as i64 + rows.len() as i64 + 1
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        for (row_number, column, value) in translated {
            if let Some(cell) = rows
                .get_mut((row_number - offset as i64 - 2) as usize)
                .and_then(|row| row.get_mut(column as usize))
                && cell.is_empty()
            {
                *cell = value;
            }
        }
    }
    Ok(CsvPage {
        headers,
        rows,
        offset,
        has_more,
    })
}
