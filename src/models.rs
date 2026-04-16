use axum::Json;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::Response;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use crate::proxy::AppState;
use crate::proxy::require_proxy_api_key;

#[derive(Clone, Debug)]
pub struct ModelsCache {
    path: PathBuf,
}

impl Default for ModelsCache {
    fn default() -> Self {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        Self {
            path: PathBuf::from(home).join(".codex/models_cache.json"),
        }
    }
}

impl ModelsCache {
    pub fn load_json(&self) -> Value {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or_else(|| serde_json::json!({ "models": [] }))
    }

    pub fn models_response_json(&self) -> Value {
        let json = self.load_json();
        let models = json
            .get("models")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        serde_json::json!({ "models": models })
    }

    pub fn best_supported_model_slug(&self) -> Option<String> {
        let json = self.load_json();
        let models = json.get("models")?.as_array()?;
        models
            .iter()
            .filter(|model| model.get("supported_in_api").and_then(Value::as_bool) == Some(true))
            .filter(|model| model.get("visibility").and_then(Value::as_str) != Some("hidden"))
            .filter_map(|model| {
                Some((
                    model.get("priority")?.as_i64()?,
                    model.get("slug")?.as_str()?.to_string(),
                ))
            })
            .min_by_key(|(priority, _)| *priority)
            .map(|(_, slug)| slug)
    }

    pub fn contains_model_slug(&self, slug: &str) -> bool {
        let json = self.load_json();
        let Some(models) = json.get("models").and_then(Value::as_array) else {
            return false;
        };
        models.iter().any(|model| {
            model
                .get("slug")
                .and_then(Value::as_str)
                .is_some_and(|candidate| candidate == slug)
        })
    }

    pub fn etag(&self) -> Option<String> {
        self.load_json()
            .get("etag")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub fn client_version(&self) -> Option<String> {
        self.load_json()
            .get("client_version")
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

pub async fn list_models(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response<axum::body::Body> {
    if let Err(err) = require_proxy_api_key(&state, &headers) {
        return axum::response::IntoResponse::into_response(err);
    }
    let json = state.models_cache.models_response_json();
    let mut response = (StatusCode::OK, Json(json)).into_response();
    if let Some(etag) = state.models_cache.etag()
        && let Ok(header_value) = HeaderValue::from_str(&etag)
    {
        response
            .headers_mut()
            .insert(axum::http::header::ETAG, header_value);
    }
    response
}
