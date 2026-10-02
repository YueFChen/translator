use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use base64::Engine as _;
use serde_json::{Value, json};
use wonderland_plugin_sdk::{HostClient, PluginError, PluginFailure, RequestTracker, serve};
use wonderland_translator::{
    JobLimits, ModelTransport, ProviderConfigInput, ProviderError, SecretStore, TermInput,
    TranslationExport, Translator, TransportFuture,
};

const CONTRACT: &str = include_str!("../package/contract.json");
const PICK_CHUNK_BYTES: u64 = 1024 * 1024;

fn main() {
    if let Err(error) = run() {
        eprintln!("translator backend stopped: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let data_dir = std::env::var_os("WONDERLAND_PLUGIN_DATA_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "Core did not provide a plugin data directory".to_owned())?;
    let services = Arc::new(CoreServices::default());
    let secrets: Arc<dyn SecretStore> = services.clone();
    let model: Arc<dyn ModelTransport> = services.clone();
    let translator = Arc::new(Translator::new(data_dir.clone(), secrets, model));
    let active_job = RequestTracker::default();
    let runtime = Arc::new(tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()
        .map_err(|error| error.to_string())?);

    serve("translator", env!("CARGO_PKG_VERSION"), CONTRACT, move |host, method, params, request_id| {
        services.set_host(host.clone());
        dispatch(&host, &translator, &data_dir, &runtime, &active_job, &method, params, request_id)
            .map_err(|error| PluginError::new(
                format!("PLUGIN_TRANSLATOR_{}", error.code().to_ascii_uppercase()),
                error.to_string(),
            ))
    }).map_err(|error| error.to_string())
}

fn dispatch(
    host: &HostClient,
    translator: &Translator,
    data_dir: &Path,
    runtime: &tokio::runtime::Runtime,
    active_job: &RequestTracker,
    method: &str,
    params: Value,
    request_id: Option<String>,
) -> Result<Value, PluginFailure> {
    match method {
        "work_tree" => to_value(translator.work_tree()),
        "rename_work" => {
            translator.rename_work(i64_param(&params, "work_id")?, string_param(&params, "name")?)?;
            Ok(Value::Null)
        }
        "open_work" => to_value(translator.open_work(i64_param(&params, "work_id")?)),
        "csv_page" => {
            let work_id = i64_param(&params, "work_id")?;
            let job_id = optional_i64_param(&params, "job_id")?;
            let offset = u64_param(&params, "offset")?;
            let limit = u64_param(&params, "limit")?;
            to_value(translator.csv_page(work_id, job_id, offset, limit))
        }
        "work_job_progress" => to_value(translator.work_job_progress(i64_param(&params, "job_id")?)),
        "export_work_drafts" => {
            let result = translator.export_work_drafts(i64_param(&params, "job_id")?)?;
            export_through_core(host, data_dir, result)
        }
        "provider_profiles" => to_value(translator.provider_profiles()),
        "save_profile" => {
            let id = optional_i64_param(&params, "id")?;
            let name = string_param(&params, "name")?;
            let input: ProviderConfigInput = value_param(&params, "input")?;
            to_value(translator.save_profile(id, name, input))
        }
        "activate_profile" => to_value(translator.activate_profile(i64_param(&params, "id")?)),
        "delete_profile" => {
            translator.delete_profile(i64_param(&params, "id")?)?;
            Ok(Value::Null)
        }
        "choose_file" => choose_csv(host, translator),
        "prepare" => {
            let mapping = value_param(&params, "mapping")?;
            to_value(translator.prepare(mapping))
        }
        "export" => {
            let result = translator.export()?;
            export_through_core(host, data_dir, result)
        }
        "memory_entries" => to_value(translator.memory_entries()),
        "disable_memory" => {
            translator.disable_memory(i64_param(&params, "id")?)?;
            Ok(Value::Null)
        }
        "glossary_terms" => to_value(translator.glossary_terms()),
        "save_term" => {
            let input: TermInput = value_param(&params, "input")?;
            to_value(translator.save_term(input))
        }
        "disable_term" => {
            translator.disable_term(i64_param(&params, "id")?)?;
            Ok(Value::Null)
        }
        "job_start" => {
            let request_id = request_id.as_deref().ok_or(PluginFailure::InvalidInput)?;
            let _active = active_job.begin(request_id).ok_or(PluginFailure::Busy)?;
            let limits: JobLimits = value_param(&params, "limits")?;
            run_job(host, runtime, translator, limits, None, Some(request_id.to_owned()))
        }
        "job_resume_selected" => {
            let request_id = request_id.as_deref().ok_or(PluginFailure::InvalidInput)?;
            let _active = active_job.begin(request_id).ok_or(PluginFailure::Busy)?;
            let limits: JobLimits = value_param(&params, "limits")?;
            run_job(host, runtime, translator, limits, Some(i64_param(&params, "job_id")?), Some(request_id.to_owned()))
        }
        "job_pause" => {
            translator.pause_job()?;
            Ok(Value::Null)
        }
        "__cancel" => {
            if params.get("requestId").and_then(Value::as_str)
                .is_some_and(|request_id| active_job.matches(request_id))
            {
                translator.cancel();
            }
            Ok(Value::Null)
        }
        _ => Err(PluginFailure::InvalidInput),
    }
}

fn run_job(
    host: &HostClient,
    runtime: &tokio::runtime::Runtime,
    translator: &Translator,
    limits: JobLimits,
    resume_id: Option<i64>,
    request_id: Option<String>,
) -> Result<Value, PluginFailure> {
    let event_host = host.clone();
    let event_id = request_id.clone();
    let progress = move |value| {
        if let Ok(payload) = serde_json::to_value(value) {
            let _ = event_host.emit("job.progress", event_id.as_deref(), payload);
        }
    };
    let event_host = host.clone();
    let event_id = request_id.as_deref();
    let log = move |lines: Vec<String>| {
        let _ = event_host.emit("job.log", event_id, json!({ "lines": lines }));
    };
    let result = match resume_id {
        Some(job_id) => runtime.block_on(translator.resume_selected_job(job_id, limits, progress, log)),
        None => runtime.block_on(translator.start_job(limits, progress, log)),
    }?;
    serde_json::to_value(result).map_err(|_| PluginFailure::InvalidResponse)
}

fn choose_csv(host: &HostClient, translator: &Translator) -> Result<Value, PluginFailure> {
    let picked = host.call_core("core.files.pick", json!({ "extensions": ["csv"], "maxBytes": 134217728 }))
        .map_err(host_error)?;
    if picked.get("cancelled").and_then(Value::as_bool) == Some(true) { return Ok(Value::Null); }
    let handle = picked.get("handle").and_then(Value::as_str).ok_or(PluginFailure::InvalidResponse)?;
    let name = picked.get("name").and_then(Value::as_str).ok_or(PluginFailure::InvalidResponse)?;
    if picked.get("mimeType").and_then(Value::as_str) != Some("text/csv") { return Err(PluginFailure::InvalidInput); }
    let size = picked.get("size").and_then(Value::as_u64).ok_or(PluginFailure::InvalidResponse)?;
    if size > 134217728 { return Err(PluginFailure::InvalidInput); }
    let mut bytes = Vec::with_capacity(size.min(16 * 1024 * 1024) as usize);
    let mut offset = 0_u64;
    while offset < size {
        let chunk = host.call_core("core.files.read", json!({ "handle": handle, "offset": offset, "chunkBytes": PICK_CHUNK_BYTES }))
            .map_err(host_error)?;
        let response_offset = chunk.get("offset").and_then(Value::as_u64).ok_or(PluginFailure::InvalidResponse)?;
        if response_offset != offset { return Err(PluginFailure::InvalidResponse); }
        let content = chunk.get("contentBase64").and_then(Value::as_str).ok_or(PluginFailure::InvalidResponse)?;
        let part = base64::engine::general_purpose::STANDARD.decode(content).map_err(|_| PluginFailure::InvalidResponse)?;
        if part.is_empty() || part.len() as u64 > size - offset { return Err(PluginFailure::InvalidResponse); }
        offset += part.len() as u64;
        bytes.extend_from_slice(&part);
        if chunk.get("eof").and_then(Value::as_bool) == Some(true) && offset != size { return Err(PluginFailure::InvalidResponse); }
    }
    to_value(translator.select_file_contents(name, &bytes))
}

fn export_through_core(host: &HostClient, data_dir: &Path, mut export: TranslationExport) -> Result<Value, PluginFailure> {
    let data_root = data_dir.canonicalize().map_err(|error| PluginFailure::Other(error.to_string()))?;
    let file = Path::new(&export.csv_path).canonicalize().map_err(|error| PluginFailure::Other(error.to_string()))?;
    if !file.starts_with(&data_root) || !file.is_file() { return Err(PluginFailure::InvalidInput); }
    let bytes = fs::read(&file).map_err(|error| PluginFailure::Other(error.to_string()))?;
    if bytes.len() > 20 * 1024 * 1024 { return Err(PluginFailure::Other("导出 CSV 超过 Core 单次托管文件大小上限".into())); }
    let name = file.file_name().and_then(|value| value.to_str()).ok_or(PluginFailure::InvalidInput)?;
    let uploaded = host.call_core("core.files.export", json!({
        "name": format!("translation-{name}"),
        "contentBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
    })).map_err(host_error)?;
    export.csv_path = uploaded.get("path").and_then(Value::as_str).ok_or(PluginFailure::InvalidResponse)?.to_owned();
    let directory = host.call_core("core.files.export_dir", json!({})).map_err(host_error)?;
    export.directory = directory.get("path").and_then(Value::as_str).ok_or(PluginFailure::InvalidResponse)?.to_owned();
    if let Some(parent) = file.parent() { let _ = fs::remove_dir_all(parent); }
    serde_json::to_value(export).map_err(|_| PluginFailure::InvalidResponse)
}

fn to_value<T: serde::Serialize>(result: Result<T, PluginFailure>) -> Result<Value, PluginFailure> {
    serde_json::to_value(result?).map_err(|_| PluginFailure::InvalidResponse)
}

fn value_param<T: serde::de::DeserializeOwned>(params: &Value, key: &str) -> Result<T, PluginFailure> {
    serde_json::from_value(params.get(key).cloned().ok_or(PluginFailure::InvalidInput)?)
        .map_err(|_| PluginFailure::InvalidInput)
}
fn string_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, PluginFailure> {
    params.get(key).and_then(Value::as_str).ok_or(PluginFailure::InvalidInput)
}
fn i64_param(params: &Value, key: &str) -> Result<i64, PluginFailure> {
    params.get(key).and_then(Value::as_i64).ok_or(PluginFailure::InvalidInput)
}
fn u64_param(params: &Value, key: &str) -> Result<u64, PluginFailure> {
    params.get(key).and_then(Value::as_u64).ok_or(PluginFailure::InvalidInput)
}
fn optional_i64_param(params: &Value, key: &str) -> Result<Option<i64>, PluginFailure> {
    match params.get(key) { Some(Value::Null) | None => Ok(None), Some(value) => value.as_i64().map(Some).ok_or(PluginFailure::InvalidInput) }
}

#[derive(Default)]
struct CoreServices { host: RwLock<Option<HostClient>> }

impl CoreServices {
    fn set_host(&self, host: HostClient) { if let Ok(mut current) = self.host.write() { *current = Some(host); } }
    fn host(&self) -> Result<HostClient, String> { self.host.read().ok().and_then(|value| value.clone()).ok_or_else(|| "Core bridge is unavailable".to_owned()) }
    fn secret_key(id: i64) -> String { format!("provider-{id}") }
}

impl SecretStore for CoreServices {
    fn has(&self, profile_id: i64) -> Result<bool, String> {
        let result = self.host()?.call_core("core.secrets.plugin.has", json!({ "key": Self::secret_key(profile_id) })).map_err(|error| error.message)?;
        result.get("exists").and_then(Value::as_bool).ok_or_else(|| "Core returned an invalid secret status".to_owned())
    }
    fn set(&self, profile_id: i64, value: &str) -> Result<(), String> {
        self.host()?.call_core("core.secrets.plugin.set", json!({ "key": Self::secret_key(profile_id), "value": value })).map_err(|error| error.message)?;
        Ok(())
    }
    fn delete(&self, profile_id: i64) -> Result<(), String> {
        self.host()?.call_core("core.secrets.plugin.delete", json!({ "key": Self::secret_key(profile_id) })).map_err(|error| error.message)?;
        Ok(())
    }
}

impl ModelTransport for CoreServices {
    fn post_json<'a>(&'a self, base_url: &'a str, allow_loopback: bool, timeout_seconds: u32, secret_id: i64, path: &'a str, body: &'a str) -> TransportFuture<'a> {
        Box::pin(async move {
            let host = self.host().map_err(ProviderError::Config)?;
            let response = host.call_core("core.network.model", json!({
                "baseUrl": base_url,
                "allowLoopback": allow_loopback,
                "timeoutSeconds": timeout_seconds,
                "secretId": Self::secret_key(secret_id),
                "path": path,
                "body": body,
            })).map_err(|error| match error.code.as_str() {
                "TIMEOUT" => ProviderError::Transient { status: None, retry_after: None },
                code if code.ends_with("TRANSIENT") => ProviderError::Transient { status: None, retry_after: None },
                code if code.ends_with("INDETERMINATE") => ProviderError::Indeterminate,
                "INVALID_INPUT" => ProviderError::Config(error.message),
                _ => ProviderError::Invalid(error.message),
            })?;
            let content = response.get("contentBase64").and_then(Value::as_str)
                .ok_or_else(|| ProviderError::Invalid("Core returned no model response.".into()))?;
            base64::engine::general_purpose::STANDARD.decode(content)
                .map_err(|_| ProviderError::Invalid("Core returned an invalid model response.".into()))
        })
    }
}

fn host_error(error: PluginError) -> PluginFailure {
    match error.code.as_str() {
        "TIMEOUT" => PluginFailure::Timeout,
        "RESOURCE_LIMIT" => PluginFailure::Other(error.message),
        "UNAUTHORIZED" => PluginFailure::InvalidInput,
        _ => PluginFailure::Transport(error.message),
    }
}
