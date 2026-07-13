//! The axum HTTP surface: `POST /render` (JSON in, `application/pdf` out) and
//! `GET /healthz`.
//!
//! Security defaults: binds loopback (`127.0.0.1:8091`) so it isn't network-
//! exposed out of the box; set `KITE_PDF_AUTH_TOKEN` to require a bearer token on
//! `/render`; refuses to start if bound to a non-loopback address without a token
//! (override `KITE_PDF_INSECURE=1`). Concurrent renders are capped
//! (`KITE_PDF_MAX_CONCURRENCY`), and caller URLs are SSRF-filtered (see
//! `chromium::assert_public_url`).

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use crate::request::{RenderError, RenderRequest};
use crate::AppState;

/// Default request-body cap. axum's built-in default is only 2 MB, which
/// rejects routine PDF inputs (HTML with base64-embedded images/fonts) with an
/// opaque 413 before the handler runs. Override via KITE_PDF_MAX_BODY_MB.
const DEFAULT_MAX_BODY_MB: usize = 32;

fn max_body_bytes() -> usize {
    std::env::var("KITE_PDF_MAX_BODY_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|mb| *mb > 0)
        .unwrap_or(DEFAULT_MAX_BODY_MB)
        * 1024
        * 1024
}

/// Build the router for the render service.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/render", post(render_handler))
        .route("/healthz", get(healthz))
        .layer(DefaultBodyLimit::max(max_body_bytes()))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    let backends = compiled_backends();
    Json(serde_json::json!({ "status": "ok", "backends": backends }))
}

/// The backends compiled into this binary (surfaced on `/healthz`).
// The pushes are cfg-gated, so the `vec![]` macro can't express this.
#[allow(clippy::vec_init_then_push)]
pub fn compiled_backends() -> Vec<&'static str> {
    let mut v = Vec::new();
    #[cfg(feature = "chromium")]
    v.push("chromium");
    #[cfg(feature = "typst")]
    v.push("typst");
    v
}

fn error_response(err: RenderError) -> Response {
    let status = match err {
        RenderError::BadRequest(_) => StatusCode::BAD_REQUEST,
        RenderError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(serde_json::json!({ "error": err.message() }))).into_response()
}

async fn render_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RenderRequest>,
) -> Response {
    if let Some(resp) = check_auth(&headers) {
        return resp;
    }
    // Cap concurrency: hold a permit for the whole render so a burst can't
    // exhaust Chromium tabs / Typst threads. Released on drop.
    let _permit = match state.render_semaphore.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "server shutting down" })),
            )
                .into_response()
        }
    };
    match crate::render(&state, &req).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/pdf")],
            bytes,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!("render failed: {}", e.message());
            error_response(e)
        }
    }
}

/// Bearer token required on `/render` when `KITE_PDF_AUTH_TOKEN` is set; `/healthz`
/// stays open. No token = open (only reachable on the loopback default bind).
/// Returns `Some(response)` to reject, or `None` when the request is authorized
/// (an Option rather than `Result<(), Response>` — axum's `Response` is large, so
/// a Result would trip clippy's `result_large_err`).
fn check_auth(headers: &HeaderMap) -> Option<Response> {
    let expected = std::env::var("KITE_PDF_AUTH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())?; // no token configured → open (loopback default)
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(tok) if ct_eq(tok.as_bytes(), expected.as_bytes()) => None,
        _ => Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(
                    serde_json::json!({ "error": "unauthorized: missing or invalid bearer token" }),
                ),
            )
                .into_response(),
        ),
    }
}

/// Constant-time byte comparison (avoids leaking the token via timing).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Bind and serve the render service. Address comes from `KITE_PDF_BIND`
/// (default loopback `127.0.0.1:8091`). Refuses to start network-exposed without
/// a token (the endpoint can SSRF/fetch on the caller's behalf).
pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let bind = std::env::var("KITE_PDF_BIND").unwrap_or_else(|_| "127.0.0.1:8091".to_string());
    let has_auth = std::env::var("KITE_PDF_AUTH_TOKEN")
        .ok()
        .is_some_and(|t| !t.is_empty());
    let exposed = if bind.starts_with("localhost:") {
        false
    } else {
        bind.parse::<std::net::SocketAddr>()
            .map(|a| !a.ip().is_loopback())
            .unwrap_or(true)
    };
    if exposed && !has_auth && std::env::var("KITE_PDF_INSECURE").is_err() {
        anyhow::bail!(
            "Refusing to start: KITE_PDF_BIND={bind} exposes the render service on the network \
             with no KITE_PDF_AUTH_TOKEN — a caller could drive fetches/SSRF from your network. \
             Set KITE_PDF_AUTH_TOKEN, bind loopback, or set KITE_PDF_INSECURE=1 to override."
        );
    }
    if !has_auth {
        tracing::warn!(
            "KITE_PDF_AUTH_TOKEN not set — /render is UNAUTHENTICATED (bound to {bind})."
        );
    }
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(
        "kite-pdf listening on http://{bind}  (backends: {})",
        compiled_backends().join("+")
    );
    axum::serve(listener, router(state)).await?;
    Ok(())
}
