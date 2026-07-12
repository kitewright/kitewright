//! Puppeteer-compatible Node.js bindings over `kitewright-engine`, built with
//! napi-rs. The addon exposes just enough surface for HTML→PDF services (the
//! invoice-service subset): `launch` → `Browser` → `createBrowserContext` /
//! `newPage` → `Page` with `setContent` / `evaluate` / `pdf` / `goto` / `close`.
//!
//! Mapping to the engine:
//! - A `Browser` (and each `BrowserContext`) holds a shared [`Engine`] (cheap
//!   Arc clone). Chromium is launched lazily on first real use and idle-reaped.
//! - Every `Page` owns one [`BrowserSession`] — a persistent page in its OWN
//!   Chromium browser context, which is exactly the per-invoice cookie/render
//!   isolation `browser.createBrowserContext()` promises in puppeteer.
//!
//! Async engine methods run on the addon-owned tokio runtime (the `tokio_rt`
//! feature), so callers never manage a runtime. The Arc-backed `Engine`/
//! `BrowserSession` are cloned at the top of each async method so no borrow of
//! `&self` is held across an `await`.

use kitewright_engine::{BrowserSession, Engine, EngineConfig, PdfOptions};
use napi::bindgen_prelude::Buffer;
use napi_derive::napi;

fn to_napi(e: anyhow::Error) -> napi::Error {
    napi::Error::from_reason(format!("{e:#}"))
}

/// puppeteer `launch(options)`.
#[napi(object)]
pub struct LaunchOptions {
    /// Headless mode. kitewright always runs headless; a `false` here is
    /// accepted but ignored (documented in the compatibility matrix).
    pub headless: Option<bool>,
    /// Chromium args. `--no-sandbox` is honored; other args are ignored.
    pub args: Option<Vec<String>>,
    /// Path to a Chrome/Chromium/chrome-headless-shell binary. Falls back to the
    /// `BROWSER_EXECUTABLE` env var, then to auto-detection.
    pub executable_path: Option<String>,
}

/// puppeteer `page.setContent(html, options)`.
#[napi(object)]
pub struct SetContentOptions {
    /// "load" (default) | "domcontentloaded" | "networkidle0".
    pub wait_until: Option<String>,
    /// Accepted for API-compatibility; the engine time-boxes internally.
    pub timeout: Option<f64>,
}

/// puppeteer `page.goto(url, options)`.
#[napi(object)]
pub struct GotoOptions {
    pub wait_until: Option<String>,
    pub timeout: Option<f64>,
}

/// puppeteer margin object (CSS lengths like "20px", "1cm").
#[napi(object)]
pub struct PdfMargin {
    pub top: Option<String>,
    pub bottom: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
}

/// puppeteer `page.pdf(options)`.
#[napi(object)]
pub struct NodePdfOptions {
    /// "a4" (default) | "letter" | "legal" | "a3".
    pub format: Option<String>,
    pub landscape: Option<bool>,
    pub print_background: Option<bool>,
    pub display_header_footer: Option<bool>,
    pub header_template: Option<String>,
    pub footer_template: Option<String>,
    pub margin: Option<PdfMargin>,
    pub scale: Option<f64>,
    pub prefer_css_page_size: Option<bool>,
}

/// A launched browser. Cheap wrapper over a shared [`Engine`].
#[napi]
pub struct Browser {
    engine: Engine,
}

#[napi]
impl Browser {
    /// puppeteer `browser.newPage()` — a page in a fresh isolated context.
    #[napi]
    pub fn new_page(&self) -> Page {
        Page {
            session: self.engine.create_session(),
        }
    }

    /// puppeteer `browser.createBrowserContext()` — an isolated context. Each
    /// page created under it already gets its own engine-level browser context,
    /// so isolation is preserved.
    #[napi]
    pub fn create_browser_context(&self) -> BrowserContext {
        BrowserContext {
            engine: self.engine.clone(),
        }
    }

    /// puppeteer `browser.close()` — shut Chromium down.
    #[napi]
    pub async fn close(&self) {
        self.engine.shutdown().await;
    }
}

/// An isolated browsing context (puppeteer `BrowserContext`).
#[napi]
pub struct BrowserContext {
    engine: Engine,
}

#[napi]
impl BrowserContext {
    /// puppeteer `context.newPage()`.
    #[napi]
    pub fn new_page(&self) -> Page {
        Page {
            session: self.engine.create_session(),
        }
    }

    /// puppeteer `context.close()`. Pages under this context each own their
    /// engine session and are closed via `page.close()`; this is a no-op kept
    /// for API compatibility.
    #[napi]
    pub async fn close(&self) {}
}

/// A page = one persistent [`BrowserSession`] in its own Chromium context.
#[napi]
pub struct Page {
    session: BrowserSession,
}

#[napi]
impl Page {
    /// puppeteer `page.setContent(html, options)`.
    #[napi]
    pub async fn set_content(
        &self,
        html: String,
        options: Option<SetContentOptions>,
    ) -> napi::Result<()> {
        let session = self.session.clone();
        let wait_until = options.and_then(|o| o.wait_until);
        session
            .set_content(&html, wait_until.as_deref())
            .await
            .map_err(to_napi)
    }

    /// puppeteer `page.evaluate(fn)`. Receives a JS expression string (the JS
    /// facade stringifies a passed function) and returns the result as a JSON
    /// string, awaiting any returned promise (`() => document.fonts.ready`).
    #[napi]
    pub async fn evaluate(&self, script: String) -> napi::Result<String> {
        let session = self.session.clone();
        let value = session.evaluate(&script).await.map_err(to_napi)?;
        Ok(value.to_string())
    }

    /// puppeteer `page.goto(url, options)`.
    #[napi]
    pub async fn goto(&self, url: String, _options: Option<GotoOptions>) -> napi::Result<()> {
        let session = self.session.clone();
        session.navigate(&url).await.map(|_| ()).map_err(to_napi)
    }

    /// puppeteer `page.pdf(options)` — returns a Node Buffer of PDF bytes.
    #[napi]
    pub async fn pdf(&self, options: Option<NodePdfOptions>) -> napi::Result<Buffer> {
        let session = self.session.clone();
        let opts = options.map(map_pdf_options).unwrap_or_default();
        let bytes = session.pdf(None, opts).await.map_err(to_napi)?;
        Ok(Buffer::from(bytes))
    }

    /// puppeteer `page.close()`.
    #[napi]
    pub async fn close(&self) {
        self.session.close().await;
    }
}

fn map_pdf_options(o: NodePdfOptions) -> PdfOptions {
    let margin = o.margin;
    PdfOptions {
        format: o.format,
        landscape: o.landscape.unwrap_or(false),
        print_background: o.print_background.unwrap_or(false),
        display_header_footer: o.display_header_footer.unwrap_or(false),
        header_template: o.header_template,
        footer_template: o.footer_template,
        margin_top: margin.as_ref().and_then(|m| m.top.clone()),
        margin_bottom: margin.as_ref().and_then(|m| m.bottom.clone()),
        margin_left: margin.as_ref().and_then(|m| m.left.clone()),
        margin_right: margin.as_ref().and_then(|m| m.right.clone()),
        scale: o.scale,
        prefer_css_page_size: o.prefer_css_page_size.unwrap_or(false),
    }
}

/// puppeteer `puppeteer.launch(options)`.
#[napi]
pub async fn launch(options: Option<LaunchOptions>) -> napi::Result<Browser> {
    let mut config = EngineConfig::default();
    // Puppeteer runs HEADLESS by default; the engine now defaults to headed, so
    // override to match Puppeteer semantics (and don't inherit KITE_HEADLESS).
    // Only go headed when the caller explicitly passes `headless: false`.
    config.headful = matches!(options.as_ref().and_then(|o| o.headless), Some(false));
    if !config.headful {
        // Headless service defaults regardless of the ambient KITE_HEADLESS env.
        config.idle_ttl = std::time::Duration::from_secs(120);
    }
    if let Some(opts) = options {
        if let Some(exe) = opts.executable_path {
            config.executable = Some(exe);
        }
        if let Some(args) = &opts.args {
            if args.iter().any(|a| a == "--no-sandbox") {
                config.no_sandbox = true;
            }
        }
    }
    // Engine::new spawns the idle reaper via tokio::spawn — safe here because
    // this async fn runs on the addon-owned tokio runtime.
    let engine = Engine::new(config);
    Ok(Browser { engine })
}
