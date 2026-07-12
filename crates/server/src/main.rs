//! kitewright: browser automation over MCP Streamable HTTP, as a single
//! small binary. The heavy lifting lives in the `kitewright-engine` crate so the
//! same core can later power Node (napi-rs) and Python (PyO3) bindings.
//!
//! Each MCP session gets its own [`BrowserSession`]: a persistent page inside
//! a dedicated browser context (cookie isolation), so agents can log in once
//! and keep interacting. The /mcp endpoint supports optional bearer auth
//! (`MCP_AUTH_TOKEN`) and per-IP rate limiting (`MCP_RATE_LIMIT_PER_MINUTE`).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, ErrorData as McpError, Implementation, ServerCapabilities,
        ServerInfo,
    },
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpService,
    },
    ServerHandler, ServiceExt,
};

use kitewright_engine::{BrowserSession, Engine, EngineConfig, PdfOptions};

mod install;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct NavigateParams {
    /// URL to open
    url: String,
    /// "Lite mode": block images/media/fonts + common ad/analytics hosts for a
    /// faster DOM-ready on heavy pages (pixels are dropped — do not use before a
    /// screenshot). Sticky for this session until changed. Omit to keep the
    /// current session default (off).
    lite: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ScreenshotParams {
    /// Optional URL to navigate to first; omit to capture the current page
    url: Option<String>,
    /// Capture the full scrollable page instead of the viewport
    #[serde(default)]
    full_page: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ExtractParams {
    /// Optional URL to navigate to first; omit to extract from the current page
    url: Option<String>,
    /// CSS selector to match elements
    selector: String,
    /// Extract this attribute instead of the element text (e.g. "href")
    attribute: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ClickParams {
    /// CSS selector of the element to click (first match)
    selector: String,
    /// Actionability timeout in milliseconds (default 5000, max 30000)
    timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TypeParams {
    /// CSS selector of the input/textarea/contenteditable to type into
    selector: String,
    /// Text to type
    text: String,
    /// Clear the existing value first
    #[serde(default)]
    clear: bool,
    /// Press Enter after typing (submit forms)
    #[serde(default)]
    press_enter: bool,
    /// Actionability timeout in milliseconds (default 5000, max 30000)
    timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PressKeyParams {
    /// DOM key value, e.g. Enter, Tab, Escape, Backspace, ArrowDown, PageDown
    key: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WaitForParams {
    /// Wait until this CSS selector matches an element
    selector: Option<String>,
    /// Wait until this text appears in the page body
    text: Option<String>,
    /// Timeout in milliseconds (default 10000, max 30000)
    timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FormField {
    /// CSS / text= / role= selector of the input
    selector: String,
    /// Value to type (replaces the existing value)
    value: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FillFormParams {
    /// Inputs to fill in order
    fields: Vec<FormField>,
    /// Actionability timeout in milliseconds per field (default 5000, max 30000)
    timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SelectOptionParams {
    /// Selector of the <select> element
    selector: String,
    /// Select the option whose value equals this
    value: Option<String>,
    /// Select the option whose visible label matches this (used if `value` is absent)
    label: Option<String>,
    /// Actionability timeout in milliseconds (default 5000, max 30000)
    timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct HoverParams {
    /// CSS / text= / role= selector of the element to hover
    selector: String,
    /// Actionability timeout in milliseconds (default 5000, max 30000)
    timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct HandleDialogParams {
    /// Accept (true) or dismiss (false) the next dialog(s)
    accept: bool,
    /// Text to type into a prompt() dialog before accepting
    prompt_text: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RestoreStateParams {
    /// A state JSON string previously returned by browser_save_state
    state: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AssertParams {
    /// Selector that must (not) match; CSS / text= / role=
    condition_selector: Option<String>,
    /// Body text that must (not) appear
    condition_text: Option<String>,
    /// Assert presence (true, default) or absence (false)
    #[serde(default = "default_true")]
    should_exist: bool,
    /// Timeout in milliseconds (default 10000, max 30000)
    timeout_ms: Option<u64>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SnapshotParams {
    /// Return only what changed since the previous snapshot in this session
    /// (added/removed role+name lines) instead of the full tree. The first call
    /// returns the full tree as a baseline.
    #[serde(default)]
    diff: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PdfParams {
    /// Optional URL to navigate to first; omit to print the current page
    url: Option<String>,
    /// Paper size: A4 (default), Letter, Legal, A3
    format: Option<String>,
    /// Landscape orientation (default portrait)
    #[serde(default)]
    landscape: bool,
    /// Print background graphics/colors
    #[serde(default)]
    print_background: bool,
    /// Render header/footer templates into the PDF (puppeteer displayHeaderFooter)
    #[serde(default)]
    display_header_footer: bool,
    /// HTML header template (CDP class hooks: date/title/url/pageNumber/totalPages).
    /// Only rendered when display_header_footer is true.
    header_template: Option<String>,
    /// HTML footer template (same class hooks). Only rendered when
    /// display_header_footer is true. This is how invoice-service stamps legal
    /// text + page numbers on every page.
    footer_template: Option<String>,
    /// Top margin as a CSS length (e.g. "20px", "1cm"). Defaults to 0.
    margin_top: Option<String>,
    /// Bottom margin as a CSS length (e.g. "35px"). Defaults to 0.
    margin_bottom: Option<String>,
    /// Left margin as a CSS length. Defaults to 0.
    margin_left: Option<String>,
    /// Right margin as a CSS length. Defaults to 0.
    margin_right: Option<String>,
    /// Scale of the page rendering (default 1.0)
    scale: Option<f64>,
    /// Prefer the CSS @page size over `format` (puppeteer preferCSSPageSize)
    #[serde(default)]
    prefer_css_page_size: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SetContentParams {
    /// Raw HTML document to load into the current page (puppeteer page.setContent).
    /// Handles large documents.
    html: String,
    /// When to consider the content loaded: "load" (default), "domcontentloaded",
    /// or "networkidle0" (approximated as load + a short settle).
    wait_until: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ExtractMarkdownParams {
    /// Optional URL to navigate to first; omit to convert the current page
    url: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ConsoleParams {
    /// Empty the console buffer after returning the messages
    #[serde(default)]
    clear: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct NetworkParams {
    /// Empty the network buffer after returning the requests
    #[serde(default)]
    clear: bool,
    /// Only return requests whose URL contains this substring
    filter: Option<String>,
}

#[derive(Clone)]
struct BrowserMcp {
    session: BrowserSession,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

fn err(e: anyhow::Error) -> McpError {
    // {:#} prints the full anyhow context chain, e.g.
    // "failed to launch Chromium ...: No such file or directory"
    McpError::internal_error(format!("{e:#}"), None)
}

fn json_text(payload: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )])
}

#[tool_router]
impl BrowserMcp {
    fn new(session: BrowserSession) -> Self {
        Self {
            session,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Open a URL in this session's page and return the page title, final URL and visible text content (capped for LLM context). The page persists across calls: cookies and state survive until the session ends. Pass lite:true to block images/media/fonts + ad/analytics hosts for a faster load on heavy pages (text-only; not for screenshots)."
    )]
    async fn browser_navigate(
        &self,
        Parameters(NavigateParams { url, lite }): Parameters<NavigateParams>,
    ) -> Result<CallToolResult, McpError> {
        let info = self.session.navigate_with(&url, lite).await.map_err(err)?;
        Ok(json_text(serde_json::json!({
            "title": info.title, "url": info.url, "text": info.text
        })))
    }

    #[tool(
        description = "Return a PNG screenshot of the current page. Pass `url` to navigate first."
    )]
    async fn browser_screenshot(
        &self,
        Parameters(ScreenshotParams { url, full_page }): Parameters<ScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        let png = self
            .session
            .screenshot(url.as_deref(), full_page)
            .await
            .map_err(err)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        Ok(CallToolResult::success(vec![ContentBlock::image(
            b64,
            "image/png",
        )]))
    }

    #[tool(
        description = "Extract text (or an attribute) from up to 50 elements matching a CSS selector on the current page. Pass `url` to navigate first."
    )]
    async fn browser_extract(
        &self,
        Parameters(ExtractParams {
            url,
            selector,
            attribute,
        }): Parameters<ExtractParams>,
    ) -> Result<CallToolResult, McpError> {
        let values = self
            .session
            .extract(url.as_deref(), &selector, attribute.as_deref())
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({
            "selector": selector, "matches": values.len(), "values": values
        })))
    }

    #[tool(
        description = "Accessibility-tree snapshot of the current page: an indented outline of roles, names and states (focused/checked/...), designed for LLM consumption. Use this to discover what is on the page before clicking or typing. Pass diff:true to get only what changed since the previous snapshot in this session (great for \"what changed after I clicked\")."
    )]
    async fn browser_snapshot(
        &self,
        Parameters(SnapshotParams { diff }): Parameters<SnapshotParams>,
    ) -> Result<CallToolResult, McpError> {
        let snapshot = if diff {
            self.session.snapshot_diff().await.map_err(err)?
        } else {
            self.session.snapshot().await.map_err(err)?
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(snapshot)]))
    }

    #[tool(
        description = "Load a raw HTML string into the current page (puppeteer page.setContent) via CDP Page.setDocumentContent. `wait_until` is \"load\" (default), \"domcontentloaded\", or \"networkidle0\". Pair with browser_pdf to render HTML→PDF (the invoice-service flow) with no server round-trip. Handles large documents."
    )]
    async fn browser_set_content(
        &self,
        Parameters(SetContentParams { html, wait_until }): Parameters<SetContentParams>,
    ) -> Result<CallToolResult, McpError> {
        self.session
            .set_content(&html, wait_until.as_deref())
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({
            "ok": true,
            "bytes": html.len(),
            "wait_until": wait_until.unwrap_or_else(|| "load".into()),
        })))
    }

    #[tool(
        description = "Print the current page to PDF (CDP Page.printToPDF). Pass `url` to navigate first (or use browser_set_content for raw HTML). `format` is A4 (default) / Letter / Legal / A3. Supports the full puppeteer option set: display_header_footer + header_template/footer_template (legal text, page numbers), margin_top/bottom/left/right (CSS lengths like \"35px\"/\"20mm\"), landscape, print_background, scale, prefer_css_page_size. Returns a JSON envelope {format, bytes, base64} where `base64` is the standard-base64-encoded PDF (MCP has no native PDF content type — decode `base64` to get the file)."
    )]
    async fn browser_pdf(
        &self,
        Parameters(PdfParams {
            url,
            format,
            landscape,
            print_background,
            display_header_footer,
            header_template,
            footer_template,
            margin_top,
            margin_bottom,
            margin_left,
            margin_right,
            scale,
            prefer_css_page_size,
        }): Parameters<PdfParams>,
    ) -> Result<CallToolResult, McpError> {
        let opts = PdfOptions {
            format: format.clone(),
            landscape,
            print_background,
            display_header_footer,
            header_template,
            footer_template,
            margin_top,
            margin_bottom,
            margin_left,
            margin_right,
            scale,
            prefer_css_page_size,
        };
        let bytes = self.session.pdf(url.as_deref(), opts).await.map_err(err)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(json_text(serde_json::json!({
            "format": format.unwrap_or_else(|| "A4".into()),
            "bytes": bytes.len(),
            "base64": b64,
        })))
    }

    #[tool(
        description = "Convert the current page's main content to Markdown for LLM consumption (\"readability\" mode): picks the best content root, strips nav/script/style/aside, and renders headings/paragraphs/links/lists/code/tables. Capped at ~20k chars. Pass `url` to navigate first."
    )]
    async fn browser_extract_markdown(
        &self,
        Parameters(ExtractMarkdownParams { url }): Parameters<ExtractMarkdownParams>,
    ) -> Result<CallToolResult, McpError> {
        let md = self
            .session
            .extract_markdown(url.as_deref())
            .await
            .map_err(err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(md)]))
    }

    #[tool(
        description = "Return console messages (log/warn/error/info/...) captured on this session's page since the last call. Set clear:true to empty the buffer after returning. Useful for debugging what a page logged after an interaction."
    )]
    async fn browser_console(
        &self,
        Parameters(ConsoleParams { clear }): Parameters<ConsoleParams>,
    ) -> Result<CallToolResult, McpError> {
        let messages = self.session.console(clear).await.map_err(err)?;
        Ok(json_text(serde_json::json!({
            "count": messages.len(), "messages": messages
        })))
    }

    #[tool(
        description = "Return network requests (method, url, status, resourceType) captured on this session's page. `filter` substring-matches the URL; set clear:true to empty the buffer after returning. Doubles as an AI-QA debugging surface."
    )]
    async fn browser_network(
        &self,
        Parameters(NetworkParams { clear, filter }): Parameters<NetworkParams>,
    ) -> Result<CallToolResult, McpError> {
        let requests = self
            .session
            .network(clear, filter.as_deref())
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({
            "count": requests.len(), "requests": requests
        })))
    }

    #[tool(
        description = "Click the first element matching a CSS selector on the current page (scrolled into view first)."
    )]
    async fn browser_click(
        &self,
        Parameters(ClickParams {
            selector,
            timeout_ms,
        }): Parameters<ClickParams>,
    ) -> Result<CallToolResult, McpError> {
        self.session
            .click(&selector, timeout_ms)
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({ "clicked": selector })))
    }

    #[tool(
        description = "Type text into the element matching a CSS selector (clicks to focus first). Set `clear` to replace the existing value and `press_enter` to submit afterwards."
    )]
    async fn browser_type(
        &self,
        Parameters(TypeParams {
            selector,
            text,
            clear,
            press_enter,
            timeout_ms,
        }): Parameters<TypeParams>,
    ) -> Result<CallToolResult, McpError> {
        self.session
            .type_text(&selector, &text, clear, press_enter, timeout_ms)
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({
            "typed": text, "selector": selector, "pressed_enter": press_enter
        })))
    }

    #[tool(
        description = "Send a keyboard key (Enter, Tab, Escape, ArrowDown, ...) to the focused element on the current page."
    )]
    async fn browser_press_key(
        &self,
        Parameters(PressKeyParams { key }): Parameters<PressKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        self.session.press_key(&key).await.map_err(err)?;
        Ok(json_text(serde_json::json!({ "pressed": key })))
    }

    #[tool(
        description = "Wait until a CSS selector matches and/or text appears in the page body, polling every 100ms (default timeout 10s, max 30s). Returns the elapsed milliseconds."
    )]
    async fn browser_wait_for(
        &self,
        Parameters(WaitForParams {
            selector,
            text,
            timeout_ms,
        }): Parameters<WaitForParams>,
    ) -> Result<CallToolResult, McpError> {
        let elapsed_ms = self
            .session
            .wait_for(selector.as_deref(), text.as_deref(), timeout_ms)
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({ "elapsed_ms": elapsed_ms })))
    }

    #[tool(
        description = "Fill multiple inputs in one call (each cleared then typed). Good for login/checkout forms. Returns a per-field ok/error summary; never aborts on the first failure."
    )]
    async fn browser_fill_form(
        &self,
        Parameters(FillFormParams { fields, timeout_ms }): Parameters<FillFormParams>,
    ) -> Result<CallToolResult, McpError> {
        let pairs: Vec<(String, String)> =
            fields.into_iter().map(|f| (f.selector, f.value)).collect();
        let outcomes = self
            .session
            .fill_form(&pairs, timeout_ms)
            .await
            .map_err(err)?;
        let filled = outcomes.iter().filter(|o| o.ok).count();
        Ok(json_text(serde_json::json!({
            "filled": filled, "total": outcomes.len(), "fields": outcomes
        })))
    }

    #[tool(
        description = "Select an <option> in a <select> by `value` or visible `label` (dispatches a change event so frameworks react). Errors if neither matches."
    )]
    async fn browser_select_option(
        &self,
        Parameters(SelectOptionParams {
            selector,
            value,
            label,
            timeout_ms,
        }): Parameters<SelectOptionParams>,
    ) -> Result<CallToolResult, McpError> {
        let selected = self
            .session
            .select_option(&selector, value.as_deref(), label.as_deref(), timeout_ms)
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({
            "selector": selector, "selected_value": selected
        })))
    }

    #[tool(
        description = "Hover the element matching a selector (moves the mouse to its center, revealing CSS :hover menus). Accepts CSS / text= / role= selectors."
    )]
    async fn browser_hover(
        &self,
        Parameters(HoverParams {
            selector,
            timeout_ms,
        }): Parameters<HoverParams>,
    ) -> Result<CallToolResult, McpError> {
        self.session
            .hover(&selector, timeout_ms)
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({ "hovered": selector })))
    }

    #[tool(
        description = "Navigate back one entry in the session page's history and return the new title + URL."
    )]
    async fn browser_navigate_back(&self) -> Result<CallToolResult, McpError> {
        let info = self.session.navigate_back().await.map_err(err)?;
        Ok(json_text(serde_json::json!({
            "title": info.title, "url": info.url
        })))
    }

    #[tool(
        description = "Arm handling of the NEXT JS dialog(s) (alert/confirm/prompt/beforeunload) on this page: auto-accept or dismiss, optionally filling a prompt. Must be called BEFORE the action that triggers the dialog (dialogs block JS). Arming persists for subsequent dialogs until changed."
    )]
    async fn browser_handle_dialog(
        &self,
        Parameters(HandleDialogParams {
            accept,
            prompt_text,
        }): Parameters<HandleDialogParams>,
    ) -> Result<CallToolResult, McpError> {
        self.session
            .handle_dialog(accept, prompt_text)
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({
            "armed": true, "accept": accept
        })))
    }

    #[tool(
        description = "Capture this session's storage state (cookies + localStorage for the current origin + URL) as a JSON string you can persist and later restore — enabling \"log in once, reuse across sessions\"."
    )]
    async fn browser_save_state(&self) -> Result<CallToolResult, McpError> {
        let state = self.session.save_state().await.map_err(err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(state)]))
    }

    #[tool(
        description = "Restore a storage state produced by browser_save_state: cookies are set immediately; localStorage is applied on/after the next navigation to its origin. Call before or after navigating."
    )]
    async fn browser_restore_state(
        &self,
        Parameters(RestoreStateParams { state }): Parameters<RestoreStateParams>,
    ) -> Result<CallToolResult, McpError> {
        self.session.restore_state(&state).await.map_err(err)?;
        Ok(json_text(serde_json::json!({ "restored": true })))
    }

    #[tool(
        description = "Assert the presence (or, with should_exist=false, absence) of a selector and/or body text within a timeout. Returns a structured {passed, checked, found, elapsed_ms} result instead of erroring, so an agent can gate a feature test on it."
    )]
    async fn browser_assert(
        &self,
        Parameters(AssertParams {
            condition_selector,
            condition_text,
            should_exist,
            timeout_ms,
        }): Parameters<AssertParams>,
    ) -> Result<CallToolResult, McpError> {
        let outcome = self
            .session
            .assert(
                condition_selector.as_deref(),
                condition_text.as_deref(),
                should_exist,
                timeout_ms,
            )
            .await
            .map_err(err)?;
        Ok(json_text(serde_json::json!({
            "passed": outcome.passed,
            "checked": outcome.checked,
            "found": outcome.found,
            "elapsed_ms": outcome.elapsed_ms,
        })))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BrowserMcp {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo (InitializeResult) and Implementation are #[non_exhaustive]
        // in rmcp 2.x, so build them via the constructor + builder methods
        // rather than a struct literal.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kitewright", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Lightweight browser automation. Each MCP session owns one persistent page \
                 (cookies isolated per session): navigate, then snapshot/click/type/wait on it. \
                 Selectors accept CSS (default), `text=<visible text>`, and \
                 `role=<role>[name=\"<accessible name>\"]`. Use browser_save_state / \
                 browser_restore_state to reuse a login across sessions, and browser_assert to \
                 gate feature checks. The browser launches lazily and is reaped when idle.",
            )
    }
}

// -- HTTP hardening: bearer auth + per-IP rate limiting ---------------------------

#[derive(Clone)]
struct HttpGuard {
    /// When set, requests must carry `Authorization: Bearer <token>`.
    token: Option<Arc<str>>,
    /// Fixed 60s window per client IP.
    rate: Arc<Mutex<HashMap<IpAddr, (Instant, u32)>>>,
    limit_per_minute: u32,
    /// Extra allowed `Origin` values beyond loopback (from MCP_ALLOWED_ORIGINS).
    allowed_origins: Arc<[String]>,
}

impl HttpGuard {
    fn from_env() -> Self {
        let token = match std::env::var("MCP_AUTH_TOKEN") {
            Ok(t) if !t.is_empty() => Some(Arc::from(t.as_str())),
            _ => {
                tracing::warn!(
                    "MCP_AUTH_TOKEN not set — the /mcp endpoint accepts unauthenticated requests"
                );
                None
            }
        };
        let limit_per_minute = std::env::var("MCP_RATE_LIMIT_PER_MINUTE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let allowed_origins: Arc<[String]> = std::env::var("MCP_ALLOWED_ORIGINS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        Self {
            token,
            rate: Arc::new(Mutex::new(HashMap::new())),
            limit_per_minute,
            allowed_origins,
        }
    }

    /// DNS-rebinding protection. A cross-origin webpage in a victim's browser
    /// sends an `Origin` header; legitimate MCP clients (Claude Code, curl,
    /// the SDK) do not. So: no Origin → allow (non-browser); Origin present →
    /// it must be a loopback origin or explicitly allow-listed. Mitigates
    /// CVE-2026-42559 without depending on the transport crate's own fix.
    fn origin_allowed(&self, origin: Option<&str>) -> bool {
        let Some(origin) = origin else { return true };
        if self.allowed_origins.iter().any(|a| a == origin) {
            return true;
        }
        // Loopback origins: http(s)://localhost | 127.0.0.1 | [::1] (any port).
        let host = origin
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(origin);
        let host = host.split('/').next().unwrap_or(host);
        let hostname = host.rsplit_once(':').map_or(host, |(h, _)| h);
        matches!(hostname, "localhost" | "127.0.0.1" | "[::1]" | "::1")
    }

    /// Fixed-window counter. Returns false when the client exceeded the limit.
    fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.rate.lock().expect("rate limiter poisoned");
        // Opportunistic cleanup so the map can't grow unbounded under
        // many-IP traffic (each entry is ~40 bytes; 1024 is plenty).
        if map.len() > 1024 {
            map.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(120));
        }
        let entry = map.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= Duration::from_secs(60) {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.limit_per_minute
    }
}

/// Constant-time comparison (no early exit on the first differing byte).
/// Length is not secret: a mismatch fails fast, matching common practice.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "error": { "code": code, "message": message },
        "id": null
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn guard_middleware(
    State(guard): State<HttpGuard>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    // DNS-rebinding protection: reject cross-origin browser requests before
    // anything else. Non-browser clients send no Origin and pass through.
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    if !guard.origin_allowed(origin) {
        return rpc_error(
            StatusCode::FORBIDDEN,
            -32003,
            "forbidden: cross-origin request rejected (DNS-rebinding protection); \
             set MCP_ALLOWED_ORIGINS to allow-list an origin",
        );
    }
    if !guard.allow(addr.ip()) {
        return rpc_error(
            StatusCode::TOO_MANY_REQUESTS,
            -32000,
            "rate limit exceeded — retry later",
        );
    }
    if let Some(token) = &guard.token {
        let authorized = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|presented| ct_eq(presented.as_bytes(), token.as_bytes()))
            .unwrap_or(false);
        if !authorized {
            return rpc_error(
                StatusCode::UNAUTHORIZED,
                -32001,
                "unauthorized: missing or invalid bearer token",
            );
        }
    }
    next.run(req).await
}

#[tokio::main]
async fn main() -> Result<()> {
    // Always log to stderr: in stdio mode, stdout IS the MCP protocol channel
    // and must carry nothing else. stderr is correct for the HTTP mode too.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // `kite install`: download a headless Chromium into the cache and exit
    // (never starts a server). Must be the first positional argument.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("install") {
        return install::run(&args[1..]).await;
    }

    // stdio transport (local, no server) vs Streamable HTTP (networked, default).
    // Local MCP clients (Claude Desktop, Cursor, Claude Code) spawn the binary
    // and talk over stdin/stdout — no port, no auth. Select with `kite --stdio`
    // / `kite stdio`, or KITE_STDIO=1.
    let stdio = std::env::args()
        .skip(1)
        .any(|a| a == "--stdio" || a == "stdio")
        || std::env::var_os("KITE_STDIO").is_some();
    if stdio {
        run_stdio().await
    } else {
        run_http().await
    }
}

/// Serve one MCP session over stdin/stdout for local, single-user use.
async fn run_stdio() -> Result<()> {
    let engine = Engine::new(EngineConfig::default());
    // Warm the browser in the background while the client handshakes.
    {
        let e = engine.clone();
        tokio::spawn(async move {
            if let Err(err) = e.prewarm().await {
                tracing::debug!("stdio prewarm failed (will retry on first call): {err:#}");
            }
        });
    }
    tracing::info!("kitewright serving over stdio");
    let service = BrowserMcp::new(engine.create_session())
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    engine.shutdown().await;
    Ok(())
}

/// Serve MCP over Streamable HTTP (networked; supports many sessions + auth).
async fn run_http() -> Result<()> {
    let bind = std::env::var("MCP_HTTP_BIND").unwrap_or_else(|_| "0.0.0.0:8090".to_string());
    let engine = Engine::new(EngineConfig::default());
    let engine_for_shutdown = engine.clone();
    let guard = HttpGuard::from_env();

    // Opt-in boot-time warm: pay the browser launch at startup, not on the
    // first request. (The idle reaper still applies afterwards.)
    if std::env::var("BROWSER_PREWARM").is_ok() {
        let e = engine.clone();
        tokio::spawn(async move {
            if let Err(err) = e.prewarm().await {
                tracing::warn!("boot prewarm failed: {err:#}");
            }
        });
    }

    let service = StreamableHttpService::new(
        move || {
            // Session-time warm: launch the browser in the background while
            // the MCP handshake completes, so the first tool call finds a
            // running browser instead of paying ~1–2s launch cost.
            let e = engine.clone();
            tokio::spawn(async move {
                if let Err(err) = e.prewarm().await {
                    tracing::debug!("session prewarm failed (will retry on first call): {err:#}");
                }
            });
            // One persistent page + browser context per MCP session; cleaned
            // up when the session's service instance is dropped.
            Ok(BrowserMcp::new(engine.create_session()))
        },
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(guard, guard_middleware));
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("kitewright listening on http://{bind}/mcp");

    // ConnectInfo is required so the rate limiter can key on the client IP.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        engine_for_shutdown.shutdown().await;
    })
    .await?;
    Ok(())
}

/// Resolve on SIGINT (Ctrl-C) or SIGTERM (docker stop / kill) so the browser
/// child is always closed and never orphaned.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

#[cfg(test)]
mod tests {
    use super::ct_eq;

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secreT"));
        assert!(!ct_eq(b"secret", b"secre"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }
}
