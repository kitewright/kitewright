//! Typst backend: data-driven invoice → PDF, entirely browser-free.
#![cfg(feature = "typst")]

use std::process::Command;

fn chrome_process_count() -> usize {
    // Count any running headless-shell / chromium processes. The Typst path must
    // never spawn one.
    let out = Command::new("pgrep")
        .args(["-f", "chrome-headless-shell"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count(),
        // pgrep missing (unlikely on CI/mac/linux): treat as 0 and rely on the
        // PDF assertions.
        Err(_) => 0,
    }
}

#[test]
fn renders_invoice_pdf_without_spawning_a_browser() {
    let template = include_str!("../testdata/invoice.typ").to_string();
    let data = serde_json::json!({
        "number": "INV-2026-014",
        "customer": "Acme Corp",
        "items": [
            { "name": "Widget", "qty": 3, "unit": 10 },
            { "name": "Gadget", "qty": 2, "unit": 25 }
        ],
        "total": 80
    });
    let data_json = serde_json::to_string(&data).unwrap();

    let before = chrome_process_count();
    let pdf = kite_pdf::typst_backend::compile_to_pdf(template, data_json)
        .expect("typst compile should succeed");
    let after = chrome_process_count();

    // Valid, non-trivial PDF.
    assert!(pdf.starts_with(b"%PDF-"), "output must be a PDF");
    assert!(pdf.len() > 2048, "expected >2KB, got {}", pdf.len());
    assert!(
        pdf.windows(5).any(|w| w == b"%%EOF"),
        "PDF should have an %%EOF trailer"
    );

    // The whole point of the Typst backend: no browser process.
    assert!(
        after <= before,
        "Typst render spawned a chrome-headless-shell process (before={before}, after={after})"
    );
}

#[test]
fn compile_error_is_reported_as_bad_request() {
    // A template referencing a missing input field should fail to compile and be
    // surfaced as a client (bad-request) error, not a panic.
    let template = "#let d = json(bytes(sys.inputs.data))\n= #d.missing_field".to_string();
    let data_json = "{}".to_string();
    let err =
        kite_pdf::typst_backend::compile_to_pdf(template, data_json).expect_err("should fail");
    assert!(matches!(err, kite_pdf::RenderError::BadRequest(_)));
}
