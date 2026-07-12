//! The shared request model and error type used by both backends, the HTTP
//! service, and the CLI.

use serde::Deserialize;

/// Page margins, as CSS length strings (`"20px"`, `"1cm"`, `"0.5in"`). Only the
/// Chromium backend honors these; the Typst backend controls margins from the
/// template's `#set page(margin: ...)`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Margin {
    pub top: Option<String>,
    pub bottom: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
}

/// A single render request. The same shape is accepted on the HTTP `/render`
/// endpoint and assembled by the CLI. Which backend runs is either forced by
/// `engine` or inferred from which content field is present (see
/// [`RenderRequest::backend`]).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RenderRequest {
    /// Force a backend: `"chromium"` or `"typst"`. Omit to infer.
    pub engine: Option<String>,

    // -- chromium inputs --
    /// Raw HTML document to render (Chromium backend).
    pub html: Option<String>,
    /// URL to navigate to and render (Chromium backend).
    pub url: Option<String>,

    // -- typst inputs --
    /// Typst source (Typst backend). The template can read the injected JSON via
    /// `sys.inputs.data` (a string), e.g. `json(bytes(sys.inputs.data))`.
    pub template: Option<String>,
    /// Arbitrary JSON exposed to the Typst template as `sys.inputs.data`.
    pub data: Option<serde_json::Value>,

    // -- shared PDF options (Chromium backend maps these onto CDP printToPDF) --
    /// Paper size: A4 (default) / Letter / Legal / A3.
    pub format: Option<String>,
    #[serde(default)]
    pub landscape: bool,
    #[serde(default)]
    pub print_background: bool,
    #[serde(default)]
    pub display_header_footer: bool,
    pub header_template: Option<String>,
    pub footer_template: Option<String>,
    pub margin: Option<Margin>,
    pub scale: Option<f64>,
    #[serde(default)]
    pub prefer_css_page_size: bool,
}

/// Which backend a request resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Chromium,
    Typst,
}

impl RenderRequest {
    /// Resolve the backend: an explicit `engine` wins; otherwise infer from the
    /// content fields (html/url → Chromium, template → Typst).
    pub fn backend(&self) -> Result<Backend, RenderError> {
        if let Some(engine) = self.engine.as_deref() {
            return match engine.trim().to_ascii_lowercase().as_str() {
                "chromium" | "chrome" | "html" => Ok(Backend::Chromium),
                "typst" => Ok(Backend::Typst),
                other => Err(RenderError::bad_request(format!(
                    "unknown engine {other:?} (expected \"chromium\" or \"typst\")"
                ))),
            };
        }
        if self.html.is_some() || self.url.is_some() {
            return Ok(Backend::Chromium);
        }
        if self.template.is_some() {
            return Ok(Backend::Typst);
        }
        Err(RenderError::bad_request(
            "no engine selected and no content provided: send one of `html`, `url` (chromium) or `template` (typst)",
        ))
    }
}

/// A render failure, carrying the HTTP status it should map to.
#[derive(Debug)]
pub enum RenderError {
    /// Client error → HTTP 400 (bad input, or a backend not compiled in).
    BadRequest(String),
    /// Server error → HTTP 500 (backend failed while rendering).
    Internal(String),
}

impl RenderError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        RenderError::BadRequest(msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        RenderError::Internal(msg.into())
    }
    /// The message a client should see.
    pub fn message(&self) -> &str {
        match self {
            RenderError::BadRequest(m) | RenderError::Internal(m) => m,
        }
    }
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for RenderError {}
