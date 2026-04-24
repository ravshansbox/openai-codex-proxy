use axum::Json;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::warn;

use crate::proxy::AppState;
use crate::proxy::require_proxy_api_key;

const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.124.0";
const UPSTREAM_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const GITHUB_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// OpenAI-compatible empty response.
const EMPTY_RESPONSE: &str = r#"{"object":"list","data":[]}"#;

#[derive(Debug, Deserialize)]
struct GithubLatestRelease {
    tag_name: String,
}

#[derive(Clone, Debug)]
pub struct CodexClientVersionCache {
    inner: Arc<Mutex<CodexClientVersionCacheInner>>,
}

#[derive(Debug)]
struct CodexClientVersionCacheInner {
    version: String,
    fetched_at: Option<Instant>,
}

#[derive(Clone, Debug, Default)]
pub struct ModelsCache {
    inner: Arc<Mutex<ModelsCacheInner>>,
}

#[derive(Debug, Default)]
struct ModelsCacheInner {
    slugs: Vec<String>,
    response: Option<Value>,
    fetched_at: Option<Instant>,
}

impl Default for CodexClientVersionCache {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(CodexClientVersionCacheInner {
                version: DEFAULT_CODEX_CLIENT_VERSION.to_string(),
                fetched_at: None,
            })),
        }
    }
}

pub async fn list_models(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response<axum::body::Body> {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }

    // Pick any authenticated account to fetch models on behalf of.
    let accounts = state.accounts.list_summaries().await;
    let authed = accounts.iter().find(|a| a.auth.authenticated);
    let Some(account) = authed else {
        warn!("no authenticated account found for models request");
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({}))).into_response();
    };

    // Resolve bearer token for this account.
    let Some(record) = state.accounts.get_record(&account.id).await else {
        warn!("failed to get record for account {}", account.id);
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({}))).into_response();
    };
    let codex_home = record.codex_home(state.accounts.data_dir());
    let auth_manager = codex_login::AuthManager::new(
        codex_home,
        /*enable_codex_api_key_env*/ false,
        codex_login::AuthCredentialsStoreMode::File,
    );
    let Some(auth) = auth_manager.auth().await else {
        warn!("auth manager returned no auth for account {}", account.id);
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({}))).into_response();
    };
    let Ok(bearer_token) = auth.get_token() else {
        warn!("failed to get token for account {}", account.id);
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({}))).into_response();
    };

    let mut upstream_headers = reqwest::header::HeaderMap::new();
    upstream_headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {bearer_token}")
            .parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    if let Some(chatgpt_account_id) = &account.auth.account_id
        && let Ok(value) = HeaderValue::from_str(chatgpt_account_id)
    {
        upstream_headers.insert("chatgpt-account-id", value);
    }

    match resolve_models(state.as_ref(), upstream_headers).await {
        Ok(models) => Json(models).into_response(),
        Err(err) => {
            warn!(error = %err, "failed to resolve models");
            (StatusCode::OK, EMPTY_RESPONSE).into_response()
        }
    }
}

/// Fetch the list of valid model slugs from the upstream API.
/// Returns just the slug strings for use in request rewriting.
pub async fn fetch_model_slugs(state: &crate::proxy::AppState) -> anyhow::Result<Vec<String>> {
    let accounts = state.accounts.list_summaries().await;
    let authed = accounts.iter().find(|a| a.auth.authenticated);
    let Some(account) = authed else {
        anyhow::bail!("no authenticated account found for models fetch");
    };

    let Some(record) = state.accounts.get_record(&account.id).await else {
        anyhow::bail!("failed to get record for account {}", account.id);
    };
    let codex_home = record.codex_home(state.accounts.data_dir());
    let auth_manager = codex_login::AuthManager::new(
        codex_home,
        /*enable_codex_api_key_env*/ false,
        codex_login::AuthCredentialsStoreMode::File,
    );
    let Some(auth) = auth_manager.auth().await else {
        anyhow::bail!("auth manager returned no auth for account {}", account.id);
    };
    let Ok(bearer_token) = auth.get_token() else {
        anyhow::bail!("failed to get token for account {}", account.id);
    };

    let mut upstream_headers = reqwest::header::HeaderMap::new();
    upstream_headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {bearer_token}")
            .parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    if let Some(chatgpt_account_id) = &account.auth.account_id
        && let Ok(value) = HeaderValue::from_str(chatgpt_account_id)
    {
        upstream_headers.insert("chatgpt-account-id", value);
    }

    let models = resolve_models(state, upstream_headers).await?;
    let slugs = models
        .get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id")?.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Ok(slugs)
}

async fn resolve_models(
    state: &crate::proxy::AppState,
    upstream_headers: reqwest::header::HeaderMap,
) -> anyhow::Result<Value> {
    let now = Instant::now();
    let mut cache = state.models.inner.lock().await;
    if cache
        .fetched_at
        .is_some_and(|fetched_at| now.duration_since(fetched_at) < CACHE_TTL)
    {
        if let Some(response) = cache.response.clone() {
            return Ok(response);
        }
    }

    let client_version = resolve_codex_client_version(state).await;
    let response = state
        .client
        .get(format!(
            "{UPSTREAM_MODELS_URL}?client_version={client_version}"
        ))
        .headers(upstream_headers)
        .send()
        .await?;

    if !response.status().is_success() {
        cache.fetched_at = Some(now);
        anyhow::bail!(
            "upstream models request failed with status {}",
            response.status()
        );
    }

    let body: Value = response.json().await?;
    let upstream_models = body
        .get("models")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let data = rewrite_models_for_openai_compatibility(upstream_models);
    let slugs = model_ids(&data);
    let models = serde_json::json!({ "object": "list", "data": data });
    tracing::info!(count = slugs.len(), "fetched models from upstream");

    cache.slugs = slugs;
    cache.response = Some(models.clone());
    cache.fetched_at = Some(now);
    Ok(models)
}

pub async fn cached_model_slugs(state: &crate::proxy::AppState) -> Vec<String> {
    state.models.inner.lock().await.slugs.clone()
}

/// Rewrite Codex-format model entries to be OpenAI-compatible.
/// - Maps `slug` → `id`
/// - Adds `object: "model"`
/// - Filters out non-list (hidden) models
pub async fn resolve_codex_client_version(state: &crate::proxy::AppState) -> String {
    let now = Instant::now();
    let mut cache = state.codex_client_version.inner.lock().await;
    if cache
        .fetched_at
        .is_some_and(|fetched_at| now.duration_since(fetched_at) < CACHE_TTL)
    {
        return cache.version.clone();
    }

    tracing::info!(
        url = GITHUB_LATEST_RELEASE_URL,
        "fetching latest Codex CLI version from GitHub"
    );
    match fetch_latest_codex_client_version(state).await {
        Ok(version) => {
            tracing::info!(version, "using latest Codex CLI version from GitHub");
            cache.version = version.clone();
            cache.fetched_at = Some(now);
            version
        }
        Err(err) => {
            warn!(error = %err, default = cache.version, "failed to fetch Codex CLI version from GitHub; using cached/default");
            cache.fetched_at = Some(now);
            cache.version.clone()
        }
    }
}

async fn fetch_latest_codex_client_version(
    state: &crate::proxy::AppState,
) -> anyhow::Result<String> {
    let release = state
        .client
        .get(GITHUB_LATEST_RELEASE_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json::<GithubLatestRelease>()
        .await?;

    let Some(version) = normalize_codex_client_version(&release.tag_name) else {
        anyhow::bail!(
            "GitHub latest release tag_name was invalid: {}",
            release.tag_name
        );
    };

    Ok(version)
}

fn normalize_codex_client_version(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let candidate = trimmed
        .rsplit_once('v')
        .map(|(_, version)| version)
        .unwrap_or(trimmed)
        .trim();

    if candidate.is_empty() || !candidate.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }

    Some(candidate.to_string())
}

fn model_ids(models: &Value) -> Vec<String> {
    let mut slugs: Vec<String> = models
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id")?.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    slugs.sort();
    slugs
}

fn rewrite_models_for_openai_compatibility(models: Value) -> Value {
    let Some(models_arr) = models.as_array() else {
        return models;
    };
    let mut rewritten: Vec<Value> = models_arr
        .iter()
        .filter(|m| m.get("visibility").and_then(|v| v.as_str()) == Some("list"))
        .filter_map(|model| {
            let slug = model.get("slug")?.as_str()?.to_string();
            let mut out = serde_json::json!({
                "id": slug,
                "object": "model",
            });
            if let Some(name) = model.get("name") {
                out["name"] = name.clone();
            }
            if let Some(ctx) = model.get("context_window") {
                out["context_length"] = ctx.clone();
            }
            // Map input_modalities for Forge image support detection.
            let input_modalities = model.get("input_modalities").cloned();
            if input_modalities.is_some() {
                out["architecture"] = serde_json::json!({
                    "modality": "multimodal",
                    "tokenizer": "o200k_base",
                    "input_modalities": input_modalities,
                });
            }
            Some(out)
        })
        .collect();
    rewritten.sort_by(|a, b| {
        let a_id = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let b_id = b.get("id").and_then(|v| v.as_str()).unwrap_or("");
        a_id.cmp(b_id)
    });
    Value::Array(rewritten)
}
