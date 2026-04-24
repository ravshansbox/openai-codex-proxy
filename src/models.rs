use axum::Json;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

use crate::proxy::AppState;
use crate::proxy::require_proxy_api_key;

const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.124.0";
const UPSTREAM_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const GITHUB_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";
const CODEX_CLIENT_VERSION_CACHE_FILE: &str = "codex-client-version-cache.json";
const CODEX_CLIENT_VERSION_CACHE_TTL_SECS: u64 = 60 * 60 * 24;

/// OpenAI-compatible empty response.
const EMPTY_RESPONSE: &str = r#"{"object":"list","data":[]}"#;

#[derive(Debug, Deserialize)]
struct GithubLatestRelease {
    tag_name: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct CachedCodexClientVersion {
    version: String,
    fetched_at: u64,
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

    let client_version = resolve_codex_client_version(&state).await;

    let response = state
        .client
        .get(format!(
            "{UPSTREAM_MODELS_URL}?client_version={client_version}"
        ))
        .headers(upstream_headers)
        .send()
        .await;

    match response {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(body) => {
                let upstream_models = body
                    .get("models")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let data = rewrite_models_for_openai_compatibility(upstream_models);
                let count = data.as_array().map(|a| a.len()).unwrap_or(0);
                tracing::info!(count, "fetched models from upstream");
                (
                    StatusCode::OK,
                    Json(serde_json::json!({ "object": "list", "data": data })),
                )
                    .into_response()
            }
            Err(err) => {
                warn!(error = %err, "failed to parse upstream models response");
                (StatusCode::OK, EMPTY_RESPONSE).into_response()
            }
        },
        Ok(resp) => {
            warn!(status = %resp.status(), "upstream models request failed");
            (StatusCode::OK, EMPTY_RESPONSE).into_response()
        }
        Err(err) => {
            warn!(error = %err, "upstream models request transport error");
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

    let mut slugs: Vec<String> = upstream_models
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|m| {
                    m.get("visibility").and_then(|v| v.as_str()) == Some("list")
                })
                .filter_map(|m| m.get("slug")?.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    slugs.sort();
    Ok(slugs)
}

/// Rewrite Codex-format model entries to be OpenAI-compatible.
/// - Maps `slug` → `id`
/// - Adds `object: "model"`
/// - Filters out non-list (hidden) models
async fn resolve_codex_client_version(state: &crate::proxy::AppState) -> String {
    let cache_path = state
        .accounts
        .data_dir()
        .join(CODEX_CLIENT_VERSION_CACHE_FILE);

    if let Some(version) = read_cached_codex_client_version(&cache_path).await {
        tracing::info!(version, path = %cache_path.display(), "using cached Codex CLI version");
        return version;
    }

    tracing::info!(url = GITHUB_LATEST_RELEASE_URL, path = %cache_path.display(), "fetching latest Codex CLI version from GitHub");
    match fetch_and_cache_latest_codex_client_version(state, &cache_path).await {
        Ok(version) => version,
        Err(err) => {
            warn!(error = %err, default = DEFAULT_CODEX_CLIENT_VERSION, "failed to refresh Codex CLI version from GitHub; using default");
            DEFAULT_CODEX_CLIENT_VERSION.to_string()
        }
    }
}

async fn read_cached_codex_client_version(cache_path: &Path) -> Option<String> {
    let raw = tokio::fs::read(cache_path).await.ok()?;
    let mut cached = serde_json::from_slice::<CachedCodexClientVersion>(&raw).ok()?;
    let now = now_unix_seconds();
    if now.saturating_sub(cached.fetched_at) > CODEX_CLIENT_VERSION_CACHE_TTL_SECS {
        return None;
    }

    let normalized = normalize_codex_client_version(&cached.version)?;
    if normalized != cached.version {
        cached.version = normalized.clone();
        if let Ok(bytes) = serde_json::to_vec(&cached) {
            let _ = tokio::fs::write(cache_path, bytes).await;
        }
    }

    Some(normalized)
}

async fn fetch_and_cache_latest_codex_client_version(
    state: &crate::proxy::AppState,
    cache_path: &Path,
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
        anyhow::bail!("GitHub latest release tag_name was invalid: {}", release.tag_name);
    };

    let payload = CachedCodexClientVersion {
        version: version.clone(),
        fetched_at: now_unix_seconds(),
    };
    let bytes = serde_json::to_vec(&payload)?;
    tokio::fs::write(cache_path, bytes).await?;
    tracing::info!(version, path = %cache_path.display(), "refreshed cached Codex CLI version from GitHub");

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

    if candidate.is_empty()
        || !candidate
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
    {
        return None;
    }

    Some(candidate.to_string())
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs()
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
