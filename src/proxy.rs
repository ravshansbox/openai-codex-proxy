use crate::accounts::AccountRegistry;
use crate::accounts::ResolvedUpstreamAuth;
use crate::accounts::RouteError;
use crate::logins::LoginManager;
use crate::proxy_auth::ProxyAuth;
use axum::Json;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::Response;
use axum::http::StatusCode;
use axum::http::header;
use axum::http::header::ACCEPT;
use axum::http::header::AUTHORIZATION;
use axum::http::header::CONNECTION;
use axum::http::header::CONTENT_ENCODING;
use axum::http::header::CONTENT_LENGTH;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::HOST;
use axum::response::IntoResponse;
use codex_login::default_client::DEFAULT_ORIGINATOR;
use futures_util::TryStreamExt;
use serde_json::json;
use std::io;
use std::sync::Arc;
use std::sync::RwLock;
use thiserror::Error;
use uuid::Uuid;

const ACCOUNT_ID_HEADER: &str = "x-codex-account-id";
const SELECTED_ACCOUNT_ID_HEADER: &str = "x-openai-codex-proxy-account-id";
const SELECTED_ACCOUNT_SCORE_HEADER: &str = "x-openai-codex-proxy-account-score";
const CHATGPT_ACCOUNT_ID_HEADER: &str = "chatgpt-account-id";
const FEDRAMP_HEADER: &str = "x-openai-fedramp";
const ORIGINATOR_HEADER: &str = "originator";
const SESSION_ID_HEADER: &str = "session_id";
const VERSION_HEADER: &str = "version";
const X_CLIENT_REQUEST_ID_HEADER: &str = "x-client-request-id";
const X_CODEX_INSTALLATION_ID_HEADER: &str = "x-codex-installation-id";
const UPSTREAM_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Clone)]
pub struct AppState {
    pub client: reqwest::Client,
    pub accounts: AccountRegistry,
    pub logins: LoginManager,
    pub installation_id: String,
    pub proxy_auth: ProxyAuth,
    /// Cached set of valid upstream model slugs, refreshed periodically.
    pub valid_models: Arc<RwLock<Vec<String>>>,
}

#[derive(serde::Serialize)]
struct HealthResponse {
    status: &'static str,
    upstream_responses_url: &'static str,
}

pub async fn health(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        upstream_responses_url: UPSTREAM_RESPONSES_URL,
    })
}

pub async fn proxy_responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, ApiError> {
    require_proxy_api_key(&state, &headers)?;
    let requested_account_id = header_value(&headers, ACCOUNT_ID_HEADER)?;
    let session_id = headers
        .get(SESSION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let content_encoding = headers
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok());
    let valid_models = state
        .valid_models
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let (body, rewritten_model) =
        rewrite_body_for_upstream(&state.installation_id, body, content_encoding, &valid_models);
    let mut excluded_account_ids = Vec::new();

    loop {
        let selected = state
            .accounts
            .select_account(
                requested_account_id.as_deref(),
                Some(session_id.as_str()),
                &excluded_account_ids,
            )
            .await?;
        let account_id = selected.lease.account_id();
        let account_score = selected.lease.score().to_string();
        let upstream_headers = build_upstream_headers(&headers, &selected.auth, &session_id)?;

        tracing::info!(
            account_id = account_id,
            score = selected.lease.score(),
            rewritten_model = rewritten_model.as_deref(),
            excluded_accounts = ?excluded_account_ids,
            "proxying responses request"
        );

        let upstream_response = state
            .client
            .post(UPSTREAM_RESPONSES_URL)
            .headers(upstream_headers)
            .body(body.clone())
            .send()
            .await
            .map_err(ApiError::UpstreamTransport)?;

        if upstream_response.status().is_success() {
            selected.lease.note_success();
            let status = upstream_response.status();
            let response_headers = upstream_response.headers().clone();
            let lease = selected.lease;
            let stream = upstream_response.bytes_stream().map_err(io::Error::other);
            let stream = stream.inspect_ok(move |_| {
                let _lease_guard = &lease;
            });
            let body = Body::from_stream(stream);

            let mut builder = Response::builder().status(status);
            let mut saw_content_type = false;
            for (name, value) in &response_headers {
                if name == CONTENT_LENGTH || name == CONNECTION {
                    continue;
                }
                if value.as_bytes().is_empty() {
                    continue;
                }
                if name == CONTENT_TYPE {
                    saw_content_type = true;
                }
                builder = builder.header(name, value);
            }
            if !saw_content_type {
                builder = builder.header(CONTENT_TYPE, "text/event-stream");
            }

            builder = builder.header(SELECTED_ACCOUNT_ID_HEADER, account_id);
            builder = builder.header(SELECTED_ACCOUNT_SCORE_HEADER, account_score);

            return builder.body(body).map_err(|err| {
                ApiError::Internal(format!("failed to build downstream response: {err}"))
            });
        }

        let status = upstream_response.status();
        let response_headers = upstream_response.headers().clone();
        let body_bytes = upstream_response.bytes().await.unwrap_or_default();
        let response_body = String::from_utf8_lossy(&body_bytes).to_string();

        if status == StatusCode::TOO_MANY_REQUESTS {
            let resets_at = parse_i64_header(&response_headers, "x-codex-primary-reset-at");
            let used_percent = parse_u8_header(&response_headers, "x-codex-primary-used-percent");
            state
                .accounts
                .mark_rate_limited(
                    &account_id,
                    Some(session_id.as_str()),
                    resets_at,
                    used_percent,
                )
                .await;
            excluded_account_ids.push(account_id.clone());
            tracing::warn!(
                account_id = account_id,
                resets_at = resets_at,
                used_percent = used_percent,
                "account hit usage limit, trying next account"
            );
            continue;
        }

        selected.lease.note_failure();
        tracing::error!(
            status = %status,
            response_headers = ?response_headers,
            response_body = %response_body,
            "upstream returned non-success status"
        );
        let detail = response_body.trim();
        let message = if detail.is_empty() {
            format!("upstream returned {status}")
        } else {
            format!("upstream returned {status}: {detail}")
        };
        return Err(ApiError::Internal(message));
    }
}

pub(crate) fn require_proxy_api_key(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if !state.proxy_auth.is_configured() {
        return Err(ApiError::ProxyAuthNotConfigured);
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(ApiError::ProxyAuthMissing)?;
    if state.proxy_auth.verify_bearer_token(bearer) {
        Ok(())
    } else {
        Err(ApiError::ProxyAuthInvalid)
    }
}

fn header_value(headers: &HeaderMap, header_name: &str) -> Result<Option<String>, ApiError> {
    headers
        .get(header_name)
        .map(|value| {
            value
                .to_str()
                .map(|text| text.trim().to_string())
                .map_err(|err| ApiError::InvalidHeaderValue {
                    header_name: header_name.to_string(),
                    message: err.to_string(),
                })
        })
        .transpose()
        .map(|value| value.filter(|text| !text.is_empty()))
}

fn parse_i64_header(headers: &HeaderMap, header_name: &str) -> Option<i64> {
    headers
        .get(header_name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
}

fn parse_u8_header(headers: &HeaderMap, header_name: &str) -> Option<u8> {
    headers
        .get(header_name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u8>().ok())
}

fn build_upstream_headers(
    incoming_headers: &HeaderMap,
    auth: &ResolvedUpstreamAuth,
    session_id: &str,
) -> Result<HeaderMap, ApiError> {
    let mut upstream_headers = HeaderMap::new();

    for (name, value) in incoming_headers {
        if should_skip_request_header(name) {
            continue;
        }
        upstream_headers.insert(name.clone(), value.clone());
    }

    if !upstream_headers.contains_key(ACCEPT) {
        upstream_headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    }
    upstream_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if !upstream_headers.contains_key(ORIGINATOR_HEADER) {
        upstream_headers.insert(
            HeaderName::from_static(ORIGINATOR_HEADER),
            HeaderValue::from_static(DEFAULT_ORIGINATOR),
        );
    }
    if !upstream_headers.contains_key(SESSION_ID_HEADER) {
        let value = HeaderValue::from_str(session_id).map_err(|err| {
            ApiError::Internal(format!("failed to build session_id header: {err}"))
        })?;
        upstream_headers.insert(HeaderName::from_static(SESSION_ID_HEADER), value);
    }
    if !upstream_headers.contains_key(X_CLIENT_REQUEST_ID_HEADER) {
        let value = HeaderValue::from_str(session_id).map_err(|err| {
            ApiError::Internal(format!("failed to build x-client-request-id header: {err}"))
        })?;
        upstream_headers.insert(HeaderName::from_static(X_CLIENT_REQUEST_ID_HEADER), value);
    }
    if !upstream_headers.contains_key(VERSION_HEADER) {
        let value = HeaderValue::from_str("0.121.0")
            .map_err(|err| ApiError::Internal(format!("failed to build version header: {err}")))?;
        upstream_headers.insert(HeaderName::from_static(VERSION_HEADER), value);
    }

    let auth_header =
        HeaderValue::from_str(&format!("Bearer {}", auth.bearer_token)).map_err(|err| {
            ApiError::Internal(format!("failed to build authorization header: {err}"))
        })?;
    upstream_headers.insert(AUTHORIZATION, auth_header);

    if let Some(chatgpt_account_id) = auth.chatgpt_account_id.as_ref() {
        let value = HeaderValue::from_str(chatgpt_account_id).map_err(|err| {
            ApiError::Internal(format!("failed to build ChatGPT account id header: {err}"))
        })?;
        upstream_headers.insert(HeaderName::from_static(CHATGPT_ACCOUNT_ID_HEADER), value);
    }

    if auth.is_fedramp_account {
        upstream_headers.insert(
            HeaderName::from_static(FEDRAMP_HEADER),
            HeaderValue::from_static("true"),
        );
    }

    Ok(upstream_headers)
}

fn rewrite_body_for_upstream(
    installation_id: &str,
    body: Bytes,
    content_encoding: Option<&str>,
    valid_models: &[String],
) -> (Bytes, Option<String>) {
    if content_encoding.is_some_and(is_zstd_encoding) {
        let Ok(decoded) = zstd::stream::decode_all(std::io::Cursor::new(&body)) else {
            return (body, None);
        };
        let (rewritten, model) =
            rewrite_json_body(installation_id, Bytes::from(decoded), valid_models);
        match zstd::stream::encode_all(std::io::Cursor::new(rewritten), 3) {
            Ok(encoded) => (Bytes::from(encoded), model),
            Err(_) => (body, None),
        }
    } else {
        rewrite_json_body(installation_id, body, valid_models)
    }
}

fn rewrite_json_body(
    installation_id: &str,
    body: Bytes,
    valid_models: &[String],
) -> (Bytes, Option<String>) {
    let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (body, None);
    };

    // Rewrite the model slug if it is not in the valid upstream models list.
    let mut rewritten_model = None;
    let model_value = json.get("model").and_then(|m| m.as_str()).map(String::from);
    if let Some(model) = model_value {
        if !valid_models.is_empty() && !valid_models.iter().any(|m| m == &model) {
            if let Some(replacement) = pick_best_replacement(&model, valid_models) {
                rewritten_model = Some(format!("{model} -> {replacement}"));
                json["model"] = serde_json::Value::String(replacement);
            }
        }
    }

    lift_input_instructions(&mut json);
    normalize_for_codex_upstream(&mut json);
    let client_metadata = json.as_object_mut().and_then(|root| {
        root.entry("client_metadata")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
    });
    if let Some(client_metadata) = client_metadata {
        client_metadata
            .entry(X_CODEX_INSTALLATION_ID_HEADER.to_string())
            .or_insert_with(|| serde_json::Value::String(installation_id.to_string()));
    }
    match serde_json::to_vec(&json) {
        Ok(updated) => (Bytes::from(updated), rewritten_model),
        Err(_) => (body, None),
    }
}

fn normalize_for_codex_upstream(json: &mut serde_json::Value) {
    let Some(root) = json.as_object_mut() else {
        return;
    };

    root.insert("store".to_string(), serde_json::Value::Bool(false));
    root.insert("stream".to_string(), serde_json::Value::Bool(true));
    root.remove("temperature");
    root.remove("max_output_tokens");

    let needs_reasoning_include = root.get("reasoning").is_some();
    if needs_reasoning_include {
        let include = root
            .entry("include".to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if !include.is_array() {
            *include = serde_json::Value::Array(Vec::new());
        }
        if let Some(items) = include.as_array_mut() {
            let required = serde_json::Value::String("reasoning.encrypted_content".to_string());
            if !items.iter().any(|item| item == &required) {
                items.push(required);
            }
        }
    }

    let client_metadata = root
        .entry("client_metadata".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !client_metadata.is_object() {
        *client_metadata = serde_json::json!({});
    }
}

fn lift_input_instructions(json: &mut serde_json::Value) {
    if json.get("instructions").is_some() {
        return;
    }
    let Some(input) = json
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    let mut instruction_parts = Vec::new();
    let mut first_non_instruction = 0usize;

    for item in input.iter() {
        let Some(role) = item.get("role").and_then(serde_json::Value::as_str) else {
            break;
        };
        if role != "developer" && role != "system" {
            break;
        }
        if let Some(text) = extract_message_text(item) {
            instruction_parts.push(text);
        }
        first_non_instruction += 1;
    }

    if instruction_parts.is_empty() {
        return;
    }

    input.drain(0..first_non_instruction);
    json["instructions"] = serde_json::Value::String(instruction_parts.join("\n\n"));
}

fn extract_message_text(message: &serde_json::Value) -> Option<String> {
    if let Some(content) = message.get("content") {
        if let Some(text) = content.as_str() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(items) = content.as_array() {
            let texts = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>();
            if !texts.is_empty() {
                return Some(texts.join("\n\n"));
            }
        }
    }
    None
}

fn pick_best_replacement(requested_model: &str, valid_models: &[String]) -> Option<String> {
    // Try exact suffix match first: e.g. "codex-mini-latest" might match a model
    // ending in "-mini". Then fall back to the last model in the list (typically
    // the most capable one).
    let lower = requested_model.to_ascii_lowercase();

    // Prefer models containing "codex" if the requested model contained it.
    if lower.contains("codex") {
        if let Some(m) = valid_models.iter().find(|m| m.to_ascii_lowercase().contains("codex")) {
            return Some(m.clone());
        }
    }

    // Prefer models containing "mini" if the request was for a mini variant.
    if lower.contains("mini") {
        if let Some(m) = valid_models.iter().find(|m| m.to_ascii_lowercase().contains("mini")) {
            return Some(m.clone());
        }
    }

    // Fall back to the first model (the most capable / default listed
    // by the upstream).
    valid_models.first().cloned()
}

fn is_zstd_encoding(value: &str) -> bool {
    value
        .split(',')
        .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
}

fn should_skip_request_header(header_name: &HeaderName) -> bool {
    header_name == HOST
        || header_name == CONTENT_LENGTH
        || header_name == CONNECTION
        || header_name == AUTHORIZATION
        || header_name.as_str().eq_ignore_ascii_case(ACCOUNT_ID_HEADER)
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("proxy API key is not configured")]
    ProxyAuthNotConfigured,
    #[error("missing proxy API key")]
    ProxyAuthMissing,
    #[error("invalid proxy API key")]
    ProxyAuthInvalid,
    #[error("invalid value for {header_name}: {message}")]
    InvalidHeaderValue {
        header_name: String,
        message: String,
    },
    #[error("upstream transport error: {0}")]
    UpstreamTransport(reqwest::Error),
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Route(#[from] RouteError),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = match &self {
            Self::ProxyAuthNotConfigured => StatusCode::SERVICE_UNAVAILABLE,
            Self::ProxyAuthMissing | Self::ProxyAuthInvalid => StatusCode::UNAUTHORIZED,
            Self::InvalidHeaderValue { .. } => StatusCode::BAD_REQUEST,
            Self::Route(RouteError::NoAccountsConfigured) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Route(RouteError::AccountAuthFailed { .. }) => StatusCode::BAD_GATEWAY,
            Self::Route(RouteError::NoEligibleAccounts { .. }) => StatusCode::TOO_MANY_REQUESTS,
            Self::UpstreamTransport(_) | Self::Internal(_) => StatusCode::BAD_GATEWAY,
        };

        let body = Json(json!({
            "error": {
                "message": self.to_string(),
                "type": "proxy_error"
            }
        }));

        (status, body).into_response()
    }
}
