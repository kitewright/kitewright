//! kite-pdf: a focused HTML/Typst → PDF render service and CLI.
//!
//! Two backends, selected at build time via Cargo features and at run time via
//! the request's `engine` field (or inferred from its content):
//! - `chromium` — HTML/URL → PDF through the shared `kitewright-engine`
//!   (headless Chromium over CDP). Optional dependency, gated by the feature.
//! - `typst` — browser-free typesetting: a Typst template + JSON data → PDF,
//!   with no process ever spawned.
//!
//! Build shapes (same binary name `kite-pdf`):
//! - default (`chromium` + `typst`) — both backends.
//! - `--no-default-features --features chromium` — HTML-only, needs a browser.
//! - `--no-default-features --features typst` — Typst-only, no browser ever.

pub mod request;

#[cfg(feature = "chromium")]
pub mod chromium;
#[cfg(feature = "typst")]
pub mod typst_backend;

pub mod http;

pub use request::{Backend, Margin, RenderError, RenderRequest};

/// Shared application state. Holds the (lazily-launched) Chromium engine when
/// the `chromium` feature is compiled in; empty otherwise.
#[derive(Clone)]
pub struct AppState {
    #[cfg(feature = "chromium")]
    pub engine: kitewright_engine::Engine,
}

impl AppState {
    pub fn new() -> Self {
        #[cfg(feature = "chromium")]
        {
            AppState {
                engine: kitewright_engine::Engine::new(kitewright_engine::EngineConfig::default()),
            }
        }
        #[cfg(not(feature = "chromium"))]
        {
            AppState {}
        }
    }
}

// `kitewright_engine::Engine` has no `Default`, so provide our own instead of
// deriving it on `AppState`.
impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a request to PDF bytes, dispatching to the resolved backend. Returns a
/// [`RenderError`] carrying the HTTP status it maps to — including a clear 400
/// when the requested backend was not compiled into this build.
pub async fn render(state: &AppState, req: &RenderRequest) -> Result<Vec<u8>, RenderError> {
    match req.backend()? {
        Backend::Chromium => {
            #[cfg(feature = "chromium")]
            {
                chromium::render(&state.engine, req).await
            }
            #[cfg(not(feature = "chromium"))]
            {
                let _ = (state, req);
                Err(RenderError::bad_request(
                    "chromium backend not compiled in this build — use the full or -chromium build",
                ))
            }
        }
        Backend::Typst => {
            #[cfg(feature = "typst")]
            {
                let _ = state;
                typst_backend::render(req).await
            }
            #[cfg(not(feature = "typst"))]
            {
                let _ = (state, req);
                Err(RenderError::bad_request(
                    "typst backend not compiled in this build — use the full or -lite build",
                ))
            }
        }
    }
}
