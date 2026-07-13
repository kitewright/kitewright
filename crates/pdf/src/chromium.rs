//! HTML/URL → PDF via the shared `kitewright-engine` (headless Chromium over
//! CDP). Compiled only with the `chromium` feature.

use kitewright_engine::{BrowserSession, Engine, PdfOptions};

use crate::request::{RenderError, RenderRequest};

/// Reject SSRF to internal targets from a caller-supplied `url`. The engine's
/// scheme guard already blocks `file://`; this adds host/IP filtering because the
/// render endpoint is network-facing and returns the fetched content inside the
/// PDF (so a blind SSRF becomes full response exfiltration). Blocks loopback,
/// RFC1918, link-local/cloud-metadata (169.254/16), IPv6 ULA/link-local, and
/// `localhost`/`.local`/`.internal` names. Set `KITE_PDF_ALLOW_PRIVATE_IPS=1` to
/// permit internal targets on a trusted network (e.g. rendering internal
/// dashboards). NOTE: hostnames that DNS-resolve to a private IP aren't caught
/// here — front an exposed deployment with network egress policy too.
fn assert_public_url(url: &str) -> Result<(), RenderError> {
    if std::env::var("KITE_PDF_ALLOW_PRIVATE_IPS").is_ok() {
        return Ok(());
    }
    let parsed = url::Url::parse(url)
        .map_err(|_| RenderError::bad_request(format!("invalid url: {url}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(RenderError::bad_request(format!(
            "only http(s) urls are allowed (got \"{}:\")",
            parsed.scheme()
        )));
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if is_blocked_host(&host) {
        return Err(RenderError::bad_request(format!(
            "navigation to internal/loopback/link-local address \"{host}\" is blocked \
             (set KITE_PDF_ALLOW_PRIVATE_IPS=1 for trusted networks)"
        )));
    }
    Ok(())
}

fn is_blocked_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    if h == "localhost"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
    {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
        }
        Err(_) => false,
    }
}

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
        assert_public_url(url)?;
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

#[cfg(test)]
mod tests {
    use super::{assert_public_url, is_blocked_host};

    #[test]
    fn ssrf_filter_blocks_internal_allows_public() {
        std::env::remove_var("KITE_PDF_ALLOW_PRIVATE_IPS");
        for bad in [
            "http://169.254.169.254/latest/meta-data/", // cloud metadata
            "http://127.0.0.1/x",
            "http://localhost/x",
            "http://10.0.0.5/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://[::1]/",
            "http://svc.internal/",
            "file:///etc/passwd", // non-http scheme
            "ftp://host/x",
        ] {
            assert!(assert_public_url(bad).is_err(), "{bad} should be blocked");
        }
        for ok in [
            "https://example.com/",
            "http://93.184.216.34/",
            "https://api.github.com/repos",
        ] {
            assert!(assert_public_url(ok).is_ok(), "{ok} should be allowed");
        }
        assert!(is_blocked_host("169.254.169.254"));
        assert!(!is_blocked_host("example.com"));
    }
}
