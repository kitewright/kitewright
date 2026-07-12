//! The axum HTTP surface: `POST /render` (JSON in, `application/pdf` out) and
//! `GET /healthz`.
//!
//! NOTE: this render service ships with NO authentication by default. It is
//! meant to run on a trusted network / behind a gateway. TODO: optional bearer
//! auth + per-IP rate limiting, mirroring the `kitewright` server's HttpGuard,
//! if this is ever exposed publicly.

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use crate::request::{RenderError, RenderRequest};
use crate::AppState;

/// Build the router for the render service.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/render", post(render_handler))
        .route("/healthz", get(healthz))
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

async fn render_handler(State(state): State<AppState>, Json(req): Json<RenderRequest>) -> Response {
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

/// Bind and serve the render service. Address comes from `KITE_PDF_BIND`
/// (default `0.0.0.0:8091`).
pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let bind = std::env::var("KITE_PDF_BIND").unwrap_or_else(|_| "0.0.0.0:8091".to_string());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(
        "kite-pdf listening on http://{bind}  (backends: {})",
        compiled_backends().join("+")
    );
    axum::serve(listener, router(state)).await?;
    Ok(())
}
