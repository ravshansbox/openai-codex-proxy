use crate::accounts::AccountRegistry;
use crate::accounts::CreateAccountRequest;
use crate::logins::LoginManager;
use crate::proxy::AppState;
use crate::proxy::require_proxy_api_key;
use axum::Json;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use std::sync::Arc;

pub async fn list_accounts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    Json(json!({ "accounts": state.accounts.list_summaries().await })).into_response()
}

pub async fn create_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    maybe_request: Option<Json<CreateAccountRequest>>,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    let request = maybe_request
        .map(|Json(request)| request)
        .unwrap_or_default();
    match state.accounts.create_account(request).await {
        Ok(account) => (StatusCode::CREATED, Json(json!({ "account": account }))).into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "message": err.to_string() } })),
        )
            .into_response(),
    }
}

pub async fn get_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    match state.accounts.get_summary(&account_id).await {
        Some(account) => (StatusCode::OK, Json(json!({ "account": account }))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "message": format!("unknown account {account_id}") } })),
        )
            .into_response(),
    }
}

pub async fn delete_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    match state.accounts.delete_account(&account_id).await {
        Ok(true) => (StatusCode::OK, Json(json!({ "deleted": true }))).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "message": format!("unknown account {account_id}") } })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "message": err.to_string() } })),
        )
            .into_response(),
    }
}

pub async fn start_browser_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    start_login(
        &state.accounts,
        &state.logins,
        &account_id,
        LoginMode::Browser,
    )
    .await
}

pub async fn create_and_start_browser_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    maybe_request: Option<Json<CreateAccountRequest>>,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    let request = maybe_request
        .map(|Json(request)| request)
        .unwrap_or_default();
    create_and_start_login(&state.accounts, &state.logins, request, LoginMode::Browser).await
}

pub async fn start_device_code_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    start_login(
        &state.accounts,
        &state.logins,
        &account_id,
        LoginMode::DeviceCode,
    )
    .await
}

pub async fn create_and_start_device_code_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    maybe_request: Option<Json<CreateAccountRequest>>,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    let request = maybe_request
        .map(|Json(request)| request)
        .unwrap_or_default();
    create_and_start_login(
        &state.accounts,
        &state.logins,
        request,
        LoginMode::DeviceCode,
    )
    .await
}

pub async fn get_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(login_id): Path<String>,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    match state.logins.get(&login_id).await {
        Some(login) => (StatusCode::OK, Json(json!({ "login": login }))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "message": format!("unknown login {login_id}") } })),
        )
            .into_response(),
    }
}

pub async fn cancel_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(login_id): Path<String>,
) -> impl IntoResponse {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return err.into_response();
    }
    match state.logins.cancel(&login_id).await {
        Some(login) => (StatusCode::OK, Json(json!({ "login": login }))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "message": format!("unknown login {login_id}") } })),
        )
            .into_response(),
    }
}

enum LoginMode {
    Browser,
    DeviceCode,
}

async fn start_login(
    accounts: &AccountRegistry,
    logins: &LoginManager,
    account_id: &str,
    mode: LoginMode,
) -> axum::response::Response {
    let Some(account) = accounts.get_record(account_id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "message": format!("unknown account {account_id}") } })),
        )
            .into_response();
    };

    let result = match mode {
        LoginMode::Browser => logins.start_browser_login(&account).await,
        LoginMode::DeviceCode => logins.start_device_code_login(&account).await,
    };

    match result {
        Ok(login) => (StatusCode::OK, Json(json!({ "login": login }))).into_response(),
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": { "message": err.to_string() } })),
        )
            .into_response(),
    }
}

async fn create_and_start_login(
    accounts: &AccountRegistry,
    logins: &LoginManager,
    request: CreateAccountRequest,
    mode: LoginMode,
) -> axum::response::Response {
    let account = match accounts.create_account(request).await {
        Ok(account) => account,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "message": err.to_string() } })),
            )
                .into_response();
        }
    };

    let result = match mode {
        LoginMode::Browser => logins.start_browser_login(&account).await,
        LoginMode::DeviceCode => logins.start_device_code_login(&account).await,
    };

    match result {
        Ok(login) => (
            StatusCode::CREATED,
            Json(json!({ "account": account, "login": login })),
        )
            .into_response(),
        Err(err) => {
            let _ = accounts.delete_account(&account.id).await;
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": { "message": err.to_string() } })),
            )
                .into_response()
        }
    }
}
