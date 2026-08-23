//! Router-side `/api/plan/*` handlers: thin proxies to the admin gateway.
//!
//! These need NO repo index and NO worker — they forward to the admin gateway
//! (billing/usage/free-trial), exactly as the monolith server did. The only
//! engine-side input is the persisted `machine_id` (for per-machine dedup on
//! checkout / free-trial claim), which the router reads from settings.json the
//! same way. Kept self-contained (the helpers are a few lines) so the router
//! doesn't depend on `server.rs` internals.

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::RouterState;
use crate::config::ensure_dir_and_load;

const PLAN_PROXY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn plan_gateway_unconfigured() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "plan gateway is not configured" })),
    )
        .into_response()
}

fn plan_admin_base() -> Result<String, Response> {
    std::env::var("CONTEXT_ENGINE_ADMIN_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(plan_gateway_unconfigured)
}

fn plan_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(PLAN_PROXY_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// Read the persisted machine_id from settings.json (router has no AppState, so
/// it loads the file). Boot of any process guarantees `Some(...)`; a missing id
/// surfaces as a 500 so a checkout can't proceed without per-machine dedup.
async fn machine_id(state: &RouterState) -> Result<String, Response> {
    let home = state.home_dir.clone();
    let settings = match tokio::task::spawn_blocking(move || ensure_dir_and_load(&home)).await {
        Ok(Ok(s)) => s,
        _ => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not load settings for machine_id" })),
            )
                .into_response());
        }
    };
    match settings.machine_id.filter(|s| !s.is_empty()) {
        Some(id) => Ok(id),
        None => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "machine_id not initialized" })),
        )
            .into_response()),
    }
}

async fn proxy_get(path: &str) -> Response {
    let base = match plan_admin_base() {
        Ok(base) => base,
        Err(response) => return response,
    };
    let url = format!("{base}{path}");
    match plan_http_client().get(&url).send().await {
        Ok(res) => passthrough(res).await,
        Err(e) => gateway_unreachable(e),
    }
}

fn gateway_unreachable(e: reqwest::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": format!("admin gateway unreachable: {e}") })),
    )
        .into_response()
}

async fn passthrough(res: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = res.bytes().await.unwrap_or_default();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// On a successful JSON object response, inject `base_url` = `<admin>/v1` so the
/// UI knows where the issued proxy key points. Mirrors the monolith.
async fn passthrough_with_base_url(res: reqwest::Response, only_if_completed: bool) -> Response {
    let base = match plan_admin_base() {
        Ok(base) => base,
        Err(response) => return response,
    };
    let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = res.bytes().await.unwrap_or_default();
    if status.is_success()
        && let Ok(mut obj) = serde_json::from_slice::<Value>(&bytes)
    {
        let inject =
            !only_if_completed || obj.get("status").and_then(|s| s.as_str()) == Some("COMPLETED");
        if inject {
            obj["base_url"] = Value::String(format!("{base}/v1"));
        }
        return (status, Json(obj)).into_response();
    }
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

pub async fn packages(State(_): State<RouterState>) -> Response {
    proxy_get("/api/packages").await
}

pub async fn payment_methods(State(_): State<RouterState>) -> Response {
    proxy_get("/api/payment-methods").await
}

pub async fn free_trial(State(_): State<RouterState>) -> Response {
    proxy_get("/api/free-trial").await
}

pub async fn order_status(
    State(_): State<RouterState>,
    AxumPath(invoice): AxumPath<String>,
) -> Response {
    let base = match plan_admin_base() {
        Ok(base) => base,
        Err(response) => return response,
    };
    let url = format!("{base}/api/orders/{invoice}/status");
    match plan_http_client().get(&url).send().await {
        Ok(res) => passthrough_with_base_url(res, true).await,
        Err(e) => gateway_unreachable(e),
    }
}

pub async fn usage(headers: HeaderMap) -> Response {
    let base = match plan_admin_base() {
        Ok(base) => base,
        Err(response) => return response,
    };
    let url = format!("{base}/api/usage");
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    match plan_http_client()
        .get(&url)
        .header(header::AUTHORIZATION, &auth)
        .send()
        .await
    {
        Ok(res) => passthrough(res).await,
        Err(e) => gateway_unreachable(e),
    }
}

pub async fn checkout(State(state): State<RouterState>, Json(body): Json<Value>) -> Response {
    let base = match plan_admin_base() {
        Ok(base) => base,
        Err(response) => return response,
    };
    let mid = match machine_id(&state).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let mut body = body;
    if let Value::Object(obj) = &mut body {
        obj.insert("machine_id".to_string(), Value::String(mid));
    } else {
        body = json!({ "machine_id": mid });
    }
    let url = format!("{base}/api/checkout");
    match plan_http_client()
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(res) => passthrough_with_base_url(res, false).await,
        Err(e) => gateway_unreachable(e),
    }
}

pub async fn free_trial_claim(State(state): State<RouterState>) -> Response {
    let base = match plan_admin_base() {
        Ok(base) => base,
        Err(response) => return response,
    };
    let mid = match machine_id(&state).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let url = format!("{base}/api/free-trial/claim");
    match plan_http_client()
        .post(&url)
        .header("content-type", "application/json")
        .json(&json!({ "machine_id": mid }))
        .send()
        .await
    {
        Ok(res) => passthrough_with_base_url(res, false).await,
        Err(e) => gateway_unreachable(e),
    }
}
