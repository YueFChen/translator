use rusqlite::{OptionalExtension, params};

use crate::config::ProviderSettings;
use crate::secrets::SecretStore;
use crate::store::Store;
use crate::{ProviderConfig, ProviderConfigInput, ProviderProfile};

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn to_view(settings: &ProviderSettings, has_key: bool) -> ProviderConfig {
    ProviderConfig {
        base_url: settings.base_url.clone(),
        model: settings.model.clone(),
        remark: String::new(),
        allow_loopback: settings.allow_loopback,
        timeout_seconds: settings.timeout_seconds,
        requests_per_minute: settings.requests_per_minute,
        price_per_million_input_tokens: settings.price_per_million_input_tokens,
        price_per_million_output_tokens: settings.price_per_million_output_tokens,
        currency: settings.currency.clone(),
        has_key,
    }
}

fn settings(input: &ProviderConfigInput) -> Result<ProviderSettings, String> {
    let value = ProviderSettings {
        base_url: input.base_url.trim().into(),
        model: input.model.trim().into(),
        allow_loopback: input.allow_loopback,
        timeout_seconds: input.timeout_seconds,
        requests_per_minute: input.requests_per_minute.filter(|rate| *rate > 0),
        price_per_million_input_tokens: input.price_per_million_input_tokens,
        price_per_million_output_tokens: input.price_per_million_output_tokens,
        currency: input.currency.as_ref().map(|value| value.trim().into()),
    };
    value.validate()?;
    Ok(value)
}

pub fn active_id(store: &Store) -> Result<i64, String> {
    store
        .conn()
        .query_row(
            "SELECT id FROM provider_profiles WHERE active=1 ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .map_err(err)
}

pub fn list(store: &Store, secret_store: &dyn SecretStore) -> Result<Vec<ProviderProfile>, String> {
    let mut statement = store
        .conn()
        .prepare("SELECT id,name,config_json,active FROM provider_profiles ORDER BY id")
        .map_err(err)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    rows.into_iter()
        .map(|(id, name, json, active)| {
            let mut config: ProviderConfig = serde_json::from_str(&json).map_err(err)?;
            config.has_key = secret_store.has(id).map_err(err)?;
            Ok(ProviderProfile {
                id,
                name,
                config,
                active,
            })
        })
        .collect()
}

pub fn save(
    store: &Store,
    secret_store: &dyn SecretStore,
    id: Option<i64>,
    name: &str,
    input: ProviderConfigInput,
) -> Result<ProviderProfile, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err("服务名称需为 1–80 个字符".into());
    }
    let remark = input.remark.trim();
    if remark.chars().count() > 200 {
        return Err("备注不能超过 200 个字符".into());
    }
    let settings = settings(&input)?;
    let mut view = to_view(&settings, false);
    view.remark = remark.into();
    let config = serde_json::to_string(&view).map_err(err)?;
    let id = if let Some(id) = id {
        if store
            .conn()
            .execute(
                "UPDATE provider_profiles SET name=?1,config_json=?2 WHERE id=?3",
                params![name, config, id],
            )
            .map_err(err)?
            != 1
        {
            return Err("模型服务不存在".into());
        }
        id
    } else {
        store
            .conn()
            .execute(
                "INSERT INTO provider_profiles(name,config_json,active) VALUES(?1,?2,0)",
                params![name, config],
            )
            .map_err(err)?;
        store.conn().last_insert_rowid()
    };
    if let Some(key) = input.api_key.as_deref() {
        if key.trim().is_empty() {
            secret_store.delete(id)?;
        } else {
            secret_store.set(id, key.trim())?;
        }
    }
    let active: bool = store
        .conn()
        .query_row(
            "SELECT active FROM provider_profiles WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .map_err(err)?;
    if active {
        store.save_provider(&settings)?;
    }
    Ok(ProviderProfile {
        id,
        name: name.into(),
        config: ProviderConfig {
            has_key: secret_store.has(id).map_err(err)?,
            ..view
        },
        active,
    })
}

pub fn activate(store: &Store, secret_store: &dyn SecretStore, id: i64) -> Result<ProviderProfile, String> {
    let json: String = store
        .conn()
        .query_row(
            "SELECT config_json FROM provider_profiles WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .map_err(err)?;
    let config: ProviderConfig = serde_json::from_str(&json).map_err(err)?;
    let input = ProviderConfigInput {
        base_url: config.base_url,
        model: config.model,
        remark: config.remark,
        allow_loopback: config.allow_loopback,
        timeout_seconds: config.timeout_seconds,
        requests_per_minute: config.requests_per_minute,
        price_per_million_input_tokens: config.price_per_million_input_tokens,
        price_per_million_output_tokens: config.price_per_million_output_tokens,
        currency: config.currency,
        api_key: None,
    };
    let settings = settings(&input)?;
    store
        .conn()
        .execute("UPDATE provider_profiles SET active=0", [])
        .map_err(err)?;
    store
        .conn()
        .execute("UPDATE provider_profiles SET active=1 WHERE id=?1", [id])
        .map_err(err)?;
    store.save_provider(&settings)?;
    list(store, secret_store)?
        .into_iter()
        .find(|profile| profile.id == id)
        .ok_or("模型服务不存在".into())
}

pub fn delete(store: &Store, secret_store: &dyn SecretStore, id: i64) -> Result<(), String> {
    let active: Option<bool> = store
        .conn()
        .query_row(
            "SELECT active FROM provider_profiles WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(err)?;
    if active.is_none() {
        return Err("模型服务不存在".into());
    }
    if active == Some(true) {
        return Err("请先启用另一个模型服务".into());
    }
    store
        .conn()
        .execute("DELETE FROM provider_profiles WHERE id=?1", [id])
        .map_err(err)?;
    secret_store.delete(id)
}
