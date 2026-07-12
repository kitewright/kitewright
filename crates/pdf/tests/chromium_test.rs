//! Chromium backend: HTML → PDF with a footer template + margins.
//! Requires a browser; set BROWSER_EXECUTABLE (and BROWSER_NO_SANDBOX on CI).
#![cfg(feature = "chromium")]

use kite_pdf::request::{Margin, RenderRequest};
use kite_pdf::AppState;

#[tokio::test]
async fn renders_html_with_footer_and_margins() {
    let html = include_str!("../testdata/invoice.html").to_string();
    // Reuse the repo-level footer fixture used by the invoice-service flow.
    let footer = include_str!("../../../testdata/invoice-footer.html").to_string();

    let req = RenderRequest {
        html: Some(html),
        display_header_footer: true,
        footer_template: Some(footer),
        print_background: true,
        margin: Some(Margin {
            top: Some("20px".into()),
            bottom: Some("40px".into()),
            left: Some("15px".into()),
            right: Some("15px".into()),
        }),
        ..Default::default()
    };

    let state = AppState::new();
    let pdf = kite_pdf::render(&state, &req)
        .await
        .expect("render should succeed");
    state.engine.shutdown().await;

    assert!(pdf.starts_with(b"%PDF-"), "output must be a PDF");
    assert!(
        pdf.windows(5).any(|w| w == b"%%EOF"),
        "PDF should have an %%EOF trailer"
    );
    assert!(pdf.len() > 1024, "expected a non-trivial PDF");
}
