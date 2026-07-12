//! HTTP surface: start the service on an ephemeral port and exercise /render.

use std::net::SocketAddr;

use kite_pdf::http::router;
use kite_pdf::AppState;

/// Start the render service on 127.0.0.1:0 and return its base URL plus the
/// `AppState` (so a caller can shut the engine down afterwards).
async fn start() -> (String, AppState) {
    let state = AppState::new();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

async fn shutdown(_state: AppState) {
    #[cfg(feature = "chromium")]
    _state.engine.shutdown().await;
}

#[tokio::test]
async fn healthz_lists_backends() {
    let (base, state) = start().await;
    let resp = reqwest::get(format!("{base}/healthz")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["backends"].as_array().unwrap().iter().count() >= 1);
    shutdown(state).await;
}

#[tokio::test]
async fn unknown_engine_is_rejected() {
    let (base, state) = start().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/render"))
        .json(&serde_json::json!({ "engine": "wkhtmltopdf", "html": "<p>hi</p>" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("unknown engine"));
    shutdown(state).await;
}

#[cfg(feature = "typst")]
#[tokio::test]
async fn typst_render_returns_pdf() {
    let (base, state) = start().await;
    let template = include_str!("../testdata/invoice.typ");
    let resp = reqwest::Client::new()
        .post(format!("{base}/render"))
        .json(&serde_json::json!({
            "engine": "typst",
            "template": template,
            "data": {
                "number": "INV-9",
                "customer": "Beta LLC",
                "items": [{ "name": "Service", "qty": 4, "unit": 50 }],
                "total": 200
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/pdf")
    );
    let bytes = resp.bytes().await.unwrap();
    assert!(bytes.starts_with(b"%PDF-"));
    shutdown(state).await;
}

#[cfg(feature = "chromium")]
#[tokio::test]
async fn chromium_render_returns_pdf() {
    // Gated out of the blocking CI job (real browser render). Runs locally and
    // in the non-blocking `browser` job.
    if std::env::var("KITE_SKIP_BROWSER_E2E").is_ok() {
        eprintln!("SKIP: browser e2e disabled (KITE_SKIP_BROWSER_E2E set)");
        return;
    }
    let (base, state) = start().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/render"))
        .json(&serde_json::json!({
            "engine": "chromium",
            "html": "<!doctype html><h1>Hello PDF</h1><p>body</p>"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/pdf")
    );
    let bytes = resp.bytes().await.unwrap();
    assert!(bytes.starts_with(b"%PDF-"));
    shutdown(state).await;
}

/// When a backend is NOT compiled into this build, requesting it must yield a
/// clear 400 (exercised by the chromium-only / typst-only CI feature combos).
#[cfg(not(feature = "typst"))]
#[tokio::test]
async fn disabled_typst_backend_returns_400() {
    let (base, state) = start().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/render"))
        .json(&serde_json::json!({ "engine": "typst", "template": "= hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("typst backend not compiled"));
    shutdown(state).await;
}

#[cfg(not(feature = "chromium"))]
#[tokio::test]
async fn disabled_chromium_backend_returns_400() {
    let (base, state) = start().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/render"))
        .json(&serde_json::json!({ "engine": "chromium", "html": "<p>hi</p>" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("chromium backend not compiled"));
    shutdown(state).await;
}
