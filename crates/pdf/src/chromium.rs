//! HTML/URL → PDF via the shared `kitewright-engine` (headless Chromium over
//! CDP). Compiled only with the `chromium` feature.

use kitewright_engine::{BrowserSession, Engine, PdfOptions};

use crate::request::{RenderError, RenderRequest};

/// Map the shared request onto the engine's [`PdfOptions`].
fn pdf_options(req: &RenderRequest) -> PdfOptions {
    let m = req.margin.clone().unwrap_or_default();
    PdfOptions {
        format: req.format.clone(),
        landscape: req.landscape,
        print_background: req.print_background,
        display_header_footer: req.display_header_footer,
        header_template: req.header_template.clone(),
        footer_template: req.footer_template.clone(),
        margin_top: m.top,
        margin_bottom: m.bottom,
        margin_left: m.left,
        margin_right: m.right,
        scale: req.scale,
        prefer_css_page_size: req.prefer_css_page_size,
    }
}

/// Render a request through Chromium, reusing the given (lazily-launched)
/// engine. `html` is loaded via `set_content`; otherwise `url` is navigated.
pub async fn render(engine: &Engine, req: &RenderRequest) -> Result<Vec<u8>, RenderError> {
    // Validate client-supplied ranges up front so a bad value is a 400, not a
    // 500 surfaced from deep inside CDP printToPDF.
    if let Some(scale) = req.scale {
        if !(0.1..=2.0).contains(&scale) {
            return Err(RenderError::bad_request(format!(
                "scale {scale} out of range (Chrome printToPDF accepts 0.1–2.0)"
            )));
        }
    }
    let opts = pdf_options(req);
    let session: BrowserSession = engine.create_session();

    if let Some(html) = &req.html {
        session
            .set_content(html, Some("networkidle0"))
            .await
            .map_err(|e| RenderError::internal(format!("set_content failed: {e:#}")))?;
        session
            .pdf(None, opts)
            .await
            .map_err(|e| RenderError::internal(format!("pdf render failed: {e:#}")))
    } else if let Some(url) = &req.url {
        session
            .pdf(Some(url), opts)
            .await
            .map_err(|e| RenderError::internal(format!("pdf render failed: {e:#}")))
    } else {
        Err(RenderError::bad_request(
            "chromium backend requires `html` or `url`",
        ))
    }
}
