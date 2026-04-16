use axum::Json;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use std::sync::Arc;
use tracing::warn;

use crate::proxy::AppState;
use crate::proxy::require_proxy_api_key;

/// Pretend to be the latest Codex CLI so the upstream returns the full model catalog.
const CODEX_CLIENT_VERSION: &str = "0.121.0";
const UPSTREAM_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";

/// OpenAI-compatible empty response.
const EMPTY_RESPONSE: &str = r#"{"object":"list","data":[]}"#;

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
    if let Some(chatgpt_account_id) = &account.auth.account_id {
        if let Ok(value) = HeaderValue::from_str(chatgpt_account_id) {
            upstream_headers.insert("chatgpt-account-id", value);
        }
    }

    let response = state
        .client
        .get(format!(
            "{UPSTREAM_MODELS_URL}?client_version={CODEX_CLIENT_VERSION}"
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

/// Rewrite Codex-format model entries to be OpenAI-compatible.
/// - Maps `slug` → `id`
/// - Adds `object: "model"`
/// - Filters out non-list (hidden) models
fn rewrite_models_for_openai_compatibility(models: Value) -> Value {
    let Some(models_arr) = models.as_array() else {
        return models;
    };
    let rewritten: Vec<Value> = models_arr
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
    Value::Array(rewritten)
}
