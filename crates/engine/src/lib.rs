//! kitewright-engine: the shared core behind the MCP server and (later) the
//! Node/Python bindings.
//!
//! Design goals vs Playwright/Puppeteer for the AI-agent use case:
//! - Lazy lifecycle: Chromium is launched on first use and reaped after an
//!   idle TTL — near-zero idle footprint.
//! - Session-scoped state: each [`BrowserSession`] owns one persistent page
//!   inside its own browser context (cookie isolation), so agents can log in
//!   and then interact. One-shot per-call helpers remain on [`Engine`].
//! - LLM-native output: visible-text extraction and accessibility-tree
//!   snapshots with hard caps instead of raw DOM dumps.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::accessibility as ax;
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType};
use chromiumoxide::cdp::browser_protocol::log::{EnableParams as LogEnableParams, EventEntryAdded};
use chromiumoxide::cdp::browser_protocol::network::{
    EnableParams as NetworkEnableParams, EventRequestWillBeSent, EventResponseReceived,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, EnableParams as PageEnableParams, EventJavascriptDialogOpening,
    HandleJavaScriptDialogParams, PrintToPdfParams, SetDocumentContentParams,
};
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    EnableParams as RuntimeEnableParams, EventConsoleApiCalled, RemoteObject,
};
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const MAX_TEXT_CHARS: usize = 20_000;
const MAX_SNAPSHOT_CHARS: usize = 15_000;
/// Recursion-depth cap for the accessibility-tree walk (stack-overflow guard on
/// pathologically deep pages). Far deeper than any real document nests.
const MAX_AX_DEPTH: usize = 256;
const MAX_MARKDOWN_CHARS: usize = 20_000;
const WAIT_FOR_POLL: Duration = Duration::from_millis(100);
/// Upper bound on how long a single poll's CDP evaluation may block before it is
/// abandoned and retried. A loaded machine can stall one `Runtime.evaluate` for
/// tens of seconds (the CDP request timeout); without this cap a single stuck
/// poll would blow the caller's whole wait window.
const WAIT_FOR_POLL_BUDGET: Duration = Duration::from_secs(3);
/// Floor for the per-poll budget so the last poll before the deadline still gets
/// a fair chance to answer.
const WAIT_FOR_MIN_BUDGET: Duration = Duration::from_millis(250);
/// Default timeout for [`BrowserSession::wait_for`].
pub const WAIT_FOR_DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// Hard cap for [`BrowserSession::wait_for`] so a tool call can never park an
/// MCP client for minutes.
pub const WAIT_FOR_MAX_TIMEOUT_MS: u64 = 30_000;
/// Poll interval for actionability auto-waiting (see [`wait_actionable`]).
const ACTIONABLE_POLL: Duration = Duration::from_millis(100);
/// Default per-op actionability timeout for click/type/hover/select_option.
pub const ACTIONABLE_DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Max console/network entries buffered per session (oldest dropped past this).
const CAPTURE_CAP: usize = 500;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Kill the browser process after this much idle time (default 120s).
    pub idle_ttl: Duration,
    /// Navigation timeout per operation.
    pub nav_timeout: Duration,
    /// Pass --no-sandbox (required in most containers).
    pub no_sandbox: bool,
    /// Launch a visible (headed) browser window. This is the DEFAULT so you can
    /// watch automation as it runs. Set `KITE_HEADLESS` to run headless, which
    /// is REQUIRED on servers, CI, and containers with no display (a headed
    /// Chrome fails to launch there).
    pub headful: bool,
    /// Optional path to a chrome/chromium/chrome-headless-shell binary.
    pub executable: Option<String>,
    /// How many pre-warmed blank browser contexts to keep ready so a NEW
    /// session gets an instantly-usable context+page (zero context-creation
    /// latency). 0 disables the pool. Read from `MCP_CONTEXT_POOL`.
    pub context_pool_size: usize,
    /// Shared on-disk HTTP cache directory (`--disk-cache-dir`). Persists across
    /// launches so repeat fetches of the same assets hit cache. Read from
    /// `KITE_CACHE_DIR`. NOTE: only the browser's default context uses this
    /// on-disk cache; per-session isolated contexts (created for cookie
    /// isolation) use an ephemeral in-memory cache — a deliberate trade-off.
    pub cache_dir: PathBuf,
    /// Optional origin to connect to during [`Engine::prewarm`] so DNS+TLS+
    /// connection is established before the first real navigate. Read from
    /// `KITE_PREWARM_URL`. No-op when unset.
    pub prewarm_url: Option<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        // Headed by default; KITE_HEADLESS opts into headless.
        let headful = std::env::var("KITE_HEADLESS").is_err();
        Self {
            // Headless (server) reaps aggressively to free ~300MB. Headed is
            // driven by a human at human pace — with pauses to read, type
            // credentials, or coordinate — so a 120s reap would kill the window
            // mid-task; give it a much longer idle window.
            idle_ttl: if headful {
                Duration::from_secs(1800)
            } else {
                Duration::from_secs(120)
            },
            nav_timeout: Duration::from_secs(20),
            no_sandbox: std::env::var("BROWSER_NO_SANDBOX").is_ok(),
            headful,
            executable: std::env::var("BROWSER_EXECUTABLE").ok(),
            // The warm-context pool holds blank pages open for instant new
            // sessions — invisible in headless, but in HEADED mode each pooled
            // page is a visible window, so one tool call would pop several
            // windows. Disable the pool when headed unless the user explicitly
            // sets MCP_CONTEXT_POOL, so one call opens exactly one window.
            context_pool_size: std::env::var("MCP_CONTEXT_POOL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(if headful { 0 } else { 2 }),
            cache_dir: std::env::var("KITE_CACHE_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("kitewright-cache")),
            prewarm_url: std::env::var("KITE_PREWARM_URL")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }
}

// -- browser install cache (shared with `kite install`) -----------------------

/// Basename of the `chrome-headless-shell` binary on this platform.
pub fn headless_shell_binary_name() -> &'static str {
    if cfg!(windows) {
        "chrome-headless-shell.exe"
    } else {
        "chrome-headless-shell"
    }
}

/// Pure cache-root resolution, split out so it can be unit-tested without
/// touching the process environment. `kite_cache_dir` is `$KITE_CACHE_DIR`,
/// `platform_cache` is the OS cache dir (`dirs::cache_dir()`).
fn cache_root(kite_cache_dir: Option<PathBuf>, platform_cache: Option<PathBuf>) -> PathBuf {
    let base = kite_cache_dir
        .filter(|p| !p.as_os_str().is_empty())
        .or(platform_cache)
        .unwrap_or_else(std::env::temp_dir);
    base.join("kitewright").join("chrome-headless-shell")
}

/// Directory under which `kite install` places downloaded
/// `chrome-headless-shell` builds (one sub-directory per version), and where
/// the engine looks for one when no system Chrome is found. Honors
/// `$KITE_CACHE_DIR`, else the platform cache dir, else the temp dir.
pub fn install_cache_dir() -> PathBuf {
    cache_root(
        std::env::var_os("KITE_CACHE_DIR").map(PathBuf::from),
        dirs::cache_dir(),
    )
}

/// Compare two Chrome version strings ("120.0.6099.109") numerically,
/// component by component, so "120.0.0.0" sorts after "99.0.0.0".
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Scan the install cache for a downloaded `chrome-headless-shell` binary,
/// returning the newest version's binary path if one is present. The layout is
/// `<cache>/<version>/chrome-headless-shell-<platform>/chrome-headless-shell`.
pub fn find_installed_browser() -> Option<PathBuf> {
    find_installed_browser_in(&install_cache_dir())
}

/// Testable core of [`find_installed_browser`]: scan `root` for version dirs.
fn find_installed_browser_in(root: &std::path::Path) -> Option<PathBuf> {
    let bin = headless_shell_binary_name();
    let mut versions: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    // Newest version first.
    versions.sort_by(|a, b| {
        let ka = a.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let kb = b.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        version_key(kb).cmp(&version_key(ka))
    });
    for ver_dir in versions {
        // Binary directly under the version dir, or nested one level (the CfT
        // zip extracts a `chrome-headless-shell-<platform>/` folder).
        let direct = ver_dir.join(bin);
        if direct.is_file() {
            return Some(direct);
        }
        if let Ok(entries) = std::fs::read_dir(&ver_dir) {
            for entry in entries.flatten() {
                let cand = entry.path().join(bin);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// Resolve the Chromium executable to launch: an explicit `BROWSER_EXECUTABLE`
/// wins; otherwise prefer a system Chrome/Chromium (chromiumoxide's own
/// detection); only if none is found do we fall back to a `kite install`-managed
/// build in the install cache. Returns `None` when nothing is found, in which
/// case [`chromiumoxide::browser::BrowserConfig`] surfaces its own error.
fn resolve_executable(explicit: Option<&str>) -> Option<String> {
    if let Some(e) = explicit {
        return Some(e.to_string());
    }
    // Prefer a real, directly-launchable browser in a known location over
    // chromiumoxide's own detection. On macOS that detection can return
    // Homebrew's `chromium.wrapper.sh` shim: the file exists (so an existence
    // check passes) but it `exec`s `/Applications/Chromium.app`, which may have
    // been uninstalled — launching then fails with "No such file or directory".
    // Our curated paths point straight at the executable, so try them first,
    // then a `kite install`-managed build.
    if let Some(p) = known_system_browser().or_else(find_installed_browser) {
        return Some(p.to_string_lossy().into_owned());
    }
    // Last resort: chromiumoxide's detection — but only trust a path that both
    // exists and is a real binary, never a shell shim that may point at a
    // browser that is no longer installed.
    let opts = chromiumoxide::detection::DetectionOptions {
        msedge: false,
        unstable: false,
    };
    if let Ok(detected) = chromiumoxide::detection::default_executable(opts) {
        if detected.exists() && !is_shell_wrapper(&detected) {
            return Some(detected.to_string_lossy().into_owned());
        }
    }
    None
}

/// True if `path` looks like a shell shim rather than a real browser executable
/// (e.g. Homebrew's `chromium.wrapper.sh`), which can `exec` a browser that is
/// no longer installed and so must not be trusted from auto-detection.
fn is_shell_wrapper(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|e| e == "sh") || path.to_string_lossy().contains("Caskroom")
}

/// Standard install locations for a Chromium-family browser, checked when
/// chromiumoxide's own detection misses or returns a path that doesn't exist.
const SYSTEM_BROWSER_PATHS: &[&str] = &[
    // macOS
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    // Linux
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/snap/bin/chromium",
    "/opt/google/chrome/chrome",
    // Windows
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
];

fn known_system_browser() -> Option<PathBuf> {
    first_existing(SYSTEM_BROWSER_PATHS)
}

fn first_existing(paths: &[&str]) -> Option<PathBuf> {
    paths.iter().map(PathBuf::from).find(|p| p.exists())
}

/// URL patterns blocked in "lite" mode via CDP `Network.setBlockedURLs`
/// (wildcards per CDP semantics). Two groups: heavy resource types matched by
/// file extension (image/media/font — irrelevant to text extraction), and
/// well-known ad/analytics/tracking hosts. This is the biggest lever we have on
/// external-page latency: it cannot beat the DNS+TLS+TTFB network floor, but it
/// skips the bulk of the bytes a heavy page would otherwise fetch.
const LITE_BLOCK_PATTERNS: &[&str] = &[
    // Images.
    "*.png",
    "*.jpg",
    "*.jpeg",
    "*.gif",
    "*.webp",
    "*.svg",
    "*.ico",
    "*.bmp",
    "*.avif",
    // Media.
    "*.mp4",
    "*.webm",
    "*.ogg",
    "*.mp3",
    "*.wav",
    "*.m4a",
    "*.mov",
    "*.avi",
    // Fonts.
    "*.woff",
    "*.woff2",
    "*.ttf",
    "*.otf",
    "*.eot",
    // Ad / analytics / tracking hosts.
    "*doubleclick.net*",
    "*google-analytics.com*",
    "*googletagmanager.com*",
    "*googlesyndication.com*",
    "*analytics.google.com*",
    "*adservice.google.com*",
    "*connect.facebook.net*",
    "*facebook.com/tr*",
    "*hotjar.com*",
    "*mixpanel.com*",
    "*amplitude.com*",
    "*segment.com*",
    "*segment.io*",
    "*scorecardresearch.com*",
    "*quantserve.com*",
    "*ads-twitter.com*",
    "*bat.bing.com*",
];

static LAUNCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// A pre-created, unused blank browser context + page held in the warm pool.
/// Single-use: handed whole to the next new session (never recycled between
/// sessions, so cookie isolation is preserved).
struct PooledContext {
    context_id: BrowserContextId,
    page: Page,
}

struct BrowserHandle {
    browser: Browser,
    event_task: JoinHandle<()>,
    last_used: Instant,
    data_dir: PathBuf,
    /// Monotonic launch counter: sessions remember which launch their page
    /// belongs to, so they can detect a reaped/crashed browser and recover.
    generation: u64,
    /// Pre-warmed blank contexts ready for new sessions. Lives inside the
    /// handle so it dies WITH the browser on idle-reap/crash — the pool can
    /// never keep the process alive (it never touches `last_used`) and drains
    /// automatically, refilling lazily on the next demand.
    pool: Vec<PooledContext>,
}

async fn close_handle(mut h: BrowserHandle, reason: &str) {
    // Graceful CDP close first; if the process doesn't exit promptly (the CDP
    // channel can flake during shutdown), force-kill so Chromium is NEVER
    // orphaned holding 300+ MB.
    let graceful = tokio::time::timeout(Duration::from_secs(3), async {
        let _ = h.browser.close().await;
        let _ = h.browser.wait().await;
    })
    .await;
    if graceful.is_err() {
        let _ = h.browser.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), h.browser.wait()).await;
        tracing::warn!(
            reason,
            "browser force-killed after graceful close timed out"
        );
    } else {
        tracing::info!(reason, "browser process closed");
    }
    h.event_task.abort();
    let _ = tokio::fs::remove_dir_all(&h.data_dir).await;
}

/// Shared browser engine. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct Engine {
    config: EngineConfig,
    handle: Arc<Mutex<Option<BrowserHandle>>>,
}

#[derive(Debug)]
pub struct PageInfo {
    pub url: String,
    pub title: String,
    pub text: String,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        let engine = Self {
            config,
            handle: Arc::new(Mutex::new(None)),
        };
        engine.spawn_reaper();
        engine
    }

    /// Create a session with its own persistent page (lazily created on first
    /// use) inside a dedicated browser context for cookie isolation. Dropping
    /// the last clone of the session closes its page and context.
    pub fn create_session(&self) -> BrowserSession {
        BrowserSession {
            shared: Arc::new(SessionShared {
                engine: self.clone(),
                state: Mutex::new(SessionState::default()),
                dialog: std::sync::Mutex::new(None),
                console: std::sync::Mutex::new(VecDeque::new()),
                network: std::sync::Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// One-shot: navigate a fresh page and return title + visible text
    /// (capped for LLM contexts). The page is closed afterwards.
    pub async fn navigate(&self, url: &str) -> Result<PageInfo> {
        let page = self.open(url).await?;
        let result = read_page_info(&page, url).await;
        let _ = page.close().await;
        result
    }

    /// One-shot: navigate a fresh page and capture a PNG screenshot.
    pub async fn screenshot(&self, url: &str, full_page: bool) -> Result<Vec<u8>> {
        let page = self.open(url).await?;
        let result = capture_png(&page, full_page).await;
        let _ = page.close().await;
        result
    }

    /// One-shot: navigate a fresh page and extract text (or an attribute)
    /// from elements matching a CSS selector.
    pub async fn extract(
        &self,
        url: &str,
        selector: &str,
        attribute: Option<&str>,
    ) -> Result<Vec<String>> {
        let page = self.open(url).await?;
        let result = extract_from_page(&page, selector, attribute).await;
        let _ = page.close().await;
        result
    }

    /// Whether a browser process is currently alive (used by tests and metrics).
    pub async fn is_running(&self) -> bool {
        self.handle.lock().await.is_some()
    }

    /// Refresh the idle-reaper clock mid-operation. Long single ops (a polling
    /// `wait_for`/`assert`) otherwise only touch `last_used` once at the start,
    /// so a short `idle_ttl` could reap the browser out from under an in-flight
    /// wait. Callers already hold the session state lock; taking the handle lock
    /// here keeps the established state→handle order.
    pub(crate) async fn touch_activity(&self) {
        if let Some(h) = self.handle.lock().await.as_mut() {
            h.last_used = Instant::now();
        }
    }

    /// Number of pre-warmed contexts currently held in the warm pool (0 when
    /// the browser is not running). Exposed for observability and tests.
    pub async fn pooled_context_count(&self) -> usize {
        self.handle
            .lock()
            .await
            .as_ref()
            .map(|h| h.pool.len())
            .unwrap_or(0)
    }

    /// Explicitly shut the browser down (also called by the idle reaper).
    pub async fn shutdown(&self) {
        let mut guard = self.handle.lock().await;
        if let Some(h) = guard.take() {
            close_handle(h, "shutdown").await;
        }
    }

    /// Launch the browser ahead of the first tool call so its startup cost
    /// (~1–2s) overlaps the MCP handshake instead of blocking the first
    /// navigation. Fills the warm context pool (which also spawns the renderer
    /// process, ~0.5–1.5s) so the first session gets an instantly-ready
    /// context+page. When `KITE_PREWARM_URL` is set, also establishes
    /// DNS+TLS+connection to that origin so the first real navigate skips the
    /// handshake. Cheap no-op when the browser is already running.
    pub async fn prewarm(&self) -> Result<()> {
        // In headed mode, launching the browser at boot would pop a visible
        // blank window before any task runs (the MCP server prewarms on start,
        // so it would appear every time the client launches). Skip prewarm and
        // launch lazily on the first real navigation — which is exactly when a
        // headed user wants the window to appear.
        if self.config.headful {
            return Ok(());
        }
        let mut guard = self.handle.lock().await;
        let was_running = guard.is_some();
        self.launch_if_needed(&mut guard).await?;
        if !was_running {
            let handle = guard.as_mut().unwrap();
            // Filling the pool creates the first page, which spawns the
            // renderer — so the first real navigation only pays network cost.
            self.fill_pool(handle).await;
            let renderer_cold = handle.pool.is_empty();
            match self.config.prewarm_url.clone() {
                // Connection pre-warm: a throwaway navigation in the default
                // context warms the (browser-global) DNS + TLS caches AND the
                // renderer. Best-effort and time-boxed.
                Some(url) => {
                    if let Ok(page) = handle.browser.new_page("about:blank").await {
                        let warmed =
                            tokio::time::timeout(self.config.nav_timeout, page.goto(url.as_str()))
                                .await
                                .map(|r| r.is_ok())
                                .unwrap_or(false);
                        let _ = page.close().await;
                        if warmed {
                            tracing::info!(%url, "connection pre-warmed");
                        } else {
                            tracing::debug!(%url, "connection pre-warm did not complete");
                        }
                    }
                }
                // No prewarm URL and pooling disabled: still warm the renderer
                // with a throwaway blank page so the first navigate is fast.
                None if renderer_cold => {
                    if let Ok(page) = handle.browser.new_page("about:blank").await {
                        let _ = page.close().await;
                    }
                }
                None => {}
            }
        }
        Ok(())
    }

    // -- internals -----------------------------------------------------------

    async fn launch_if_needed(&self, guard: &mut Option<BrowserHandle>) -> Result<()> {
        // Present isn't the same as alive. The browser can die on its own — a
        // crash, an OOM, the user closing the headed window, an external kill —
        // WITHOUT the reaper running, leaving a handle whose CDP channel is dead;
        // reusing it makes every call fail with "receiver is gone". (The handler
        // stream keeps yielding Err rather than ending on connection loss, so
        // `event_task.is_finished()` is NOT a reliable death signal.) Probe it
        // with a cheap CDP call under a short budget; if it doesn't answer,
        // discard the corpse and fall through to relaunch.
        if let Some(h) = guard.as_ref() {
            let responsive = matches!(
                tokio::time::timeout(Duration::from_secs(2), h.browser.version()).await,
                Ok(Ok(_))
            );
            if responsive {
                return Ok(());
            }
            tracing::warn!("browser unresponsive (crash / closed window / kill); relaunching");
            if let Some(dead) = guard.take() {
                close_handle(dead, "browser died").await;
            }
        }
        let started = Instant::now();
        let seq = LAUNCH_SEQ.fetch_add(1, Ordering::Relaxed);
        // Unique profile dir per launch: never collide with the user's
        // Chrome or an orphaned previous instance.
        let data_dir =
            std::env::temp_dir().join(format!("kitewright-{}-{}", std::process::id(), seq,));
        tokio::fs::create_dir_all(&data_dir).await?;

        // Generous CDP command timeout: the default is short enough that a
        // single command (e.g. Input.dispatchKeyEvent) can time out on a slow
        // or loaded runner (observed as flaky "Request timed out" in CI on
        // shared macOS runners). 30s never fires in normal use but absorbs
        // scheduling stalls.
        let mut builder = BrowserConfig::builder()
            .user_data_dir(&data_dir)
            .request_timeout(Duration::from_secs(30));
        if let Some(exe) = resolve_executable(self.config.executable.as_deref()) {
            builder = builder.chrome_executable(exe);
        }
        // Shared on-disk HTTP cache, stable across launches so repeat asset
        // fetches (across sessions in a run, and across process restarts) hit
        // cache instead of the network. Best-effort: a create failure just
        // means Chromium falls back to its default in-profile cache.
        let _ = tokio::fs::create_dir_all(&self.config.cache_dir).await;
        // chromiumoxide renders each arg as `--{key}`, so keys MUST be bare (no
        // leading `--`) — passing `--no-sandbox` yields `----no-sandbox`, which
        // Chrome ignores. That silently defeated every flag here: most notably
        // `no-sandbox` (fatal "No usable sandbox!" on Linux CI/containers) and
        // `disable-dev-shm-usage`.
        let disk_cache_arg = format!("disk-cache-dir={}", self.config.cache_dir.display());
        let mut args: Vec<String> = [
            "disable-gpu",
            "disable-extensions",
            "mute-audio",
            // Write shared memory to /tmp instead of /dev/shm. On Linux CI and in
            // containers /dev/shm is often tiny (~64MB), which crashes Chromium's
            // renderer mid-navigation (seen as empty/timed-out CDP responses).
            // Harmless elsewhere.
            "disable-dev-shm-usage",
            // Skip startup work that only matters for interactive Chrome:
            "no-first-run",
            "no-default-browser-check",
            "disable-background-networking",
            "disable-component-update",
            "disable-sync",
            "disable-default-apps",
            "hide-scrollbars",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.push(disk_cache_arg);
        if self.config.no_sandbox {
            args.push("no-sandbox".to_string());
        }
        builder = builder.args(args);
        // Headed (visible window) mode is opt-in via KITE_HEADFUL; the default
        // is headless (correct for agents/servers/CI).
        if self.config.headful {
            builder = builder.with_head();
        }
        let config = builder.build().map_err(|e| anyhow::anyhow!(e))?;

        let (browser, mut handler) = Browser::launch(config).await.context(
            "failed to launch Chromium — set BROWSER_EXECUTABLE or install chrome/chromium",
        )?;
        let event_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "browser launched"
        );
        *guard = Some(BrowserHandle {
            browser,
            event_task,
            last_used: Instant::now(),
            data_dir,
            generation: seq,
            pool: Vec::new(),
        });
        Ok(())
    }

    /// Top the warm context pool back up to `context_pool_size`. Best-effort:
    /// stops on the first CDP error. IMPORTANT: never touches `last_used`, so a
    /// filled pool can never defeat the idle reaper.
    async fn fill_pool(&self, handle: &mut BrowserHandle) {
        while handle.pool.len() < self.config.context_pool_size {
            let ctx = match handle
                .browser
                .create_browser_context(CreateBrowserContextParams::default())
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::debug!("pool context creation failed: {e}");
                    break;
                }
            };
            let mut params = CreateTargetParams::new("about:blank");
            params.browser_context_id = Some(ctx.clone());
            match handle.browser.new_page(params).await {
                Ok(page) => handle.pool.push(PooledContext {
                    context_id: ctx,
                    page,
                }),
                Err(e) => {
                    let _ = handle.browser.dispose_browser_context(ctx).await;
                    tracing::debug!("pool page creation failed: {e}");
                    break;
                }
            }
        }
        tracing::debug!(pool = handle.pool.len(), "context pool topped up");
    }

    /// Refill the warm pool in the background (does not block the caller and
    /// does not keep the browser alive). No-op when pooling is disabled or the
    /// browser is gone.
    fn spawn_pool_refill(&self) {
        if self.config.context_pool_size == 0 {
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            let mut guard = engine.handle.lock().await;
            if let Some(h) = guard.as_mut() {
                engine.fill_pool(h).await;
            }
        });
    }

    async fn open(&self, url: &str) -> Result<Page> {
        let mut guard = self.handle.lock().await;
        self.launch_if_needed(&mut guard).await?;
        let handle = guard.as_mut().unwrap();
        handle.last_used = Instant::now();
        let page = tokio::time::timeout(self.config.nav_timeout, handle.browser.new_page(url))
            .await
            .context("navigation timed out")??;
        Ok(page)
    }

    /// Best-effort disposal of a browser context (no-op when the browser is
    /// already gone — its contexts died with it).
    async fn dispose_context(&self, id: BrowserContextId) {
        let guard = self.handle.lock().await;
        if let Some(h) = guard.as_ref() {
            if let Err(e) = h.browser.dispose_browser_context(id).await {
                tracing::debug!("dispose browser context failed: {e}");
            }
        }
    }

    fn spawn_reaper(&self) {
        let handle = Arc::clone(&self.handle);
        let ttl = self.config.idle_ttl;
        // Check at least every 15s, but faster when the TTL itself is short
        // (short TTLs are used in tests and low-memory deployments).
        let period = ttl.min(Duration::from_secs(15)).max(Duration::from_secs(1));
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            loop {
                interval.tick().await;
                let mut guard = handle.lock().await;
                // `last_used` is touched by every session/one-shot op, so the
                // browser is only reaped when NO session has recent activity.
                let idle = guard
                    .as_ref()
                    .map(|h| h.last_used.elapsed() >= ttl)
                    .unwrap_or(false);
                if idle {
                    if let Some(h) = guard.take() {
                        close_handle(h, "idle ttl").await;
                    }
                }
            }
        });
    }
}

// -- sessions -----------------------------------------------------------------

#[derive(Default)]
struct SessionState {
    page: Option<Page>,
    context_id: Option<BrowserContextId>,
    /// Which browser launch `page` belongs to (compare with
    /// `BrowserHandle::generation` to detect a reaped/relaunched browser).
    generation: u64,
    last_url: Option<String>,
    /// Whether a persistent CDP dialog handler has been wired onto the current
    /// page (see [`BrowserSession::handle_dialog`]). Reset when the page is
    /// (re)created.
    dialog_armed: bool,
    /// localStorage entries restored from a saved state, keyed by origin.
    /// Applied automatically after the next navigation to that origin, since
    /// localStorage can only be written while on the matching origin.
    pending_localstorage: Option<(String, HashMap<String, String>)>,
    /// Previous snapshot text, used by [`BrowserSession::snapshot_diff`] to emit
    /// only what changed since the last snapshot in this session.
    last_snapshot: Option<String>,
    /// Sticky "lite mode" default for this session: when true, navigations
    /// block heavy resources (see [`LITE_BLOCK_PATTERNS`]). Set by an explicit
    /// `lite` argument to [`BrowserSession::navigate_with`]; the text-only tools
    /// force lite on per call without touching this default.
    lite_default: bool,
}

/// Desired auto-response for the next JS dialog (alert/confirm/prompt/
/// beforeunload). Read by the page-event handler task.
#[derive(Debug, Clone)]
struct DialogBehavior {
    accept: bool,
    prompt_text: Option<String>,
}

struct SessionShared {
    engine: Engine,
    state: Mutex<SessionState>,
    /// Behavior applied by the persistent dialog handler. In a std Mutex (not
    /// the async one) so the event-handler task can read it without contending
    /// on the per-call session lock.
    dialog: std::sync::Mutex<Option<DialogBehavior>>,
    /// Console messages captured on this session's page. std Mutex so the
    /// listener tasks can push without touching the per-call session lock.
    /// Bounded at [`CAPTURE_CAP`] (oldest dropped).
    console: std::sync::Mutex<VecDeque<ConsoleMessage>>,
    /// Network requests captured on this session's page (status filled in when
    /// the matching response arrives). Bounded at [`CAPTURE_CAP`].
    network: std::sync::Mutex<VecDeque<NetworkRequest>>,
}

impl Drop for SessionShared {
    fn drop(&mut self) {
        // Runs when the last clone of the session is dropped (e.g. the MCP
        // session ends). Drop can't be async, so spawn the CDP cleanup; if the
        // runtime is already gone the browser is being shut down anyway.
        let st = self.state.get_mut();
        let page = st.page.take();
        let ctx = st.context_id.take();
        if page.is_none() && ctx.is_none() {
            return;
        }
        let engine = self.engine.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Some(p) = page {
                    let _ = p.close().await;
                }
                if let Some(id) = ctx {
                    engine.dispose_context(id).await;
                }
                tracing::debug!("browser session cleaned up");
            });
        }
    }
}

/// A persistent browsing session: one page in its own browser context, kept
/// alive across tool calls so agents can log in and then click around.
/// Cheap to clone (Arc inside); the page/context are closed when the last
/// clone is dropped (or via [`BrowserSession::close`]).
#[derive(Clone)]
pub struct BrowserSession {
    shared: Arc<SessionShared>,
}

impl BrowserSession {
    /// Navigate the session page and return title + visible text (capped).
    /// Uses the session's current lite-mode default (off unless a previous
    /// [`Self::navigate_with`] turned it on).
    pub async fn navigate(&self, url: &str) -> Result<PageInfo> {
        self.navigate_with(url, None).await
    }

    /// Like [`Self::navigate`], but `lite` explicitly selects "lite mode" for
    /// this navigation and becomes the session's sticky default: `Some(true)`
    /// blocks images/media/fonts + ad/analytics hosts (faster DOM-ready on
    /// heavy pages, pixels irrelevant); `Some(false)` loads everything; `None`
    /// keeps the current session default.
    pub async fn navigate_with(&self, url: &str, lite: Option<bool>) -> Result<PageInfo> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, true).await?;
        if let Some(l) = lite {
            st.lite_default = l;
        }
        let effective = st.lite_default;
        self.goto(&mut st, &page, url, effective).await?;
        read_page_info(&page, url).await
    }

    /// Screenshot the current page (PNG). If `url` is given, navigate first.
    pub async fn screenshot(&self, url: Option<&str>, full_page: bool) -> Result<Vec<u8>> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, url.is_some()).await?;
        if let Some(u) = url {
            // Screenshots need pixels: never block images/media in lite mode.
            self.goto(&mut st, &page, u, false).await?;
        }
        capture_png(&page, full_page).await
    }

    /// Extract text (or an attribute) from elements matching a CSS selector
    /// on the current page. If `url` is given, navigate first.
    pub async fn extract(
        &self,
        url: Option<&str>,
        selector: &str,
        attribute: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, url.is_some()).await?;
        if let Some(u) = url {
            // Text extraction ignores pixels: default to lite for a faster load.
            self.goto(&mut st, &page, u, true).await?;
        }
        extract_from_page(&page, selector, attribute).await
    }

    /// Accessibility-tree snapshot of the current page, rendered as an
    /// indented text outline for LLM consumption (capped at ~15k chars).
    /// Falls back to a DOM-derived outline when the AX tree is unavailable.
    pub async fn snapshot(&self) -> Result<String> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let snap = compute_snapshot(&page).await?;
        // A full snapshot also (re)sets the diff baseline for this session.
        st.last_snapshot = Some(snap.clone());
        Ok(snap)
    }

    /// Like [`Self::snapshot`], but returns only what CHANGED since the previous
    /// snapshot in this session (added/removed role+name lines). The first call
    /// returns the full tree tagged as the baseline. Useful for "what changed
    /// after I clicked".
    pub async fn snapshot_diff(&self) -> Result<String> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let current = compute_snapshot(&page).await?;
        let out = match st.last_snapshot.take() {
            None => format!(
                "(baseline: full snapshot — call snapshot again after an action to see only the changes)\n\n{current}"
            ),
            Some(prev) => diff_snapshots(&prev, &current),
        };
        st.last_snapshot = Some(current);
        Ok(out)
    }

    /// Print the current page (or `url` if given) to PDF via CDP
    /// `Page.printToPDF`, returning the raw PDF bytes. `format` selects the
    /// paper size (A4/Letter/Legal; default A4).
    pub async fn pdf(&self, url: Option<&str>, opts: PdfOptions) -> Result<Vec<u8>> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, url.is_some()).await?;
        if let Some(u) = url {
            // A PDF is a visual artifact: keep images/fonts (lite off).
            self.goto(&mut st, &page, u, false).await?;
        }
        let (width, height) = paper_size(opts.format.as_deref());
        // Margins accept CSS lengths (px/mm/cm/in/pt); CDP wants inches. Unset →
        // 0 (matches the "no margin" default; puppeteer callers pass explicit
        // margins like invoice-service's top:20px/bottom:35px).
        let params = PrintToPdfParams {
            landscape: Some(opts.landscape),
            print_background: Some(opts.print_background),
            paper_width: Some(width),
            paper_height: Some(height),
            display_header_footer: Some(opts.display_header_footer),
            header_template: opts.header_template.clone(),
            footer_template: opts.footer_template.clone(),
            margin_top: Some(css_len_to_inches(opts.margin_top.as_deref()).unwrap_or(0.0)),
            margin_bottom: Some(css_len_to_inches(opts.margin_bottom.as_deref()).unwrap_or(0.0)),
            margin_left: Some(css_len_to_inches(opts.margin_left.as_deref()).unwrap_or(0.0)),
            margin_right: Some(css_len_to_inches(opts.margin_right.as_deref()).unwrap_or(0.0)),
            scale: opts.scale,
            prefer_css_page_size: Some(opts.prefer_css_page_size),
            ..Default::default()
        };
        page.pdf(params)
            .await
            .context("Page.printToPDF failed (PDF generation needs Chrome headless)")
    }

    /// Load a raw HTML string into the session page (puppeteer's
    /// `page.setContent`). The document is replaced in place via CDP
    /// `Page.setDocumentContent` on the page's main frame — this passes the HTML
    /// as a protocol string field, so arbitrarily large documents are handled
    /// without any string interpolation into a script.
    ///
    /// `wait_until` mirrors puppeteer's option and is approximated by polling
    /// `document.readyState`:
    /// - `"domcontentloaded"` → wait for `interactive` (or better),
    /// - `"load"` (default) → wait for `complete`,
    /// - `"networkidle0"` → wait for `complete`, then a short settle so any
    ///   resources kicked off during parse can finish (we do not intercept every
    ///   in-flight request; for the setContent use case content is inline, so a
    ///   fixed settle is a faithful and robust approximation).
    pub async fn set_content(&self, html: &str, wait_until: Option<&str>) -> Result<()> {
        let mut st = self.shared.state.lock().await;
        // `navigating = true`: setting content establishes a fresh document, so
        // recreating a lost page transparently is correct here.
        let page = self.ensure_page(&mut st, true).await?;
        set_document_content(&page, html).await?;
        st.last_url = page.url().await.ok().flatten();
        let deadline = Instant::now() + self.shared.engine.config.nav_timeout;
        match wait_until.unwrap_or("load") {
            "domcontentloaded" => {
                wait_ready_state(&page, &["interactive", "complete"], deadline).await?;
            }
            "networkidle0" | "networkidle2" => {
                wait_ready_state(&page, &["complete"], deadline).await?;
                // Approximate network idle: brief settle after load.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            // "load" and anything unrecognized.
            _ => {
                wait_ready_state(&page, &["complete"], deadline).await?;
            }
        }
        Ok(())
    }

    /// Evaluate a JavaScript expression on the current page and return its
    /// result as JSON (puppeteer's `page.evaluate`). Uses CDP `Runtime.evaluate`
    /// with `awaitPromise` + `returnByValue`, so a promise-returning expression
    /// (e.g. `document.fonts.ready`) is awaited and its resolved value returned.
    /// Non-serializable results (a FontFaceSet, a DOM node) come back as JSON
    /// `null` — the call still succeeds, matching puppeteer's lenient behavior.
    /// A thrown JS exception surfaces as an `Err`.
    pub async fn evaluate(&self, script: &str) -> Result<serde_json::Value> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let resp = page
            .execute(RawCommand {
                id: "Runtime.evaluate",
                params: serde_json::json!({
                    "expression": script,
                    "awaitPromise": true,
                    "returnByValue": true,
                    "userGesture": true,
                }),
            })
            .await
            .context("Runtime.evaluate failed")?;
        if let Some(details) = resp.result.get("exceptionDetails") {
            let msg = details
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|d| d.as_str())
                .or_else(|| details.get("text").and_then(|t| t.as_str()))
                .unwrap_or("evaluation threw");
            bail!("page.evaluate error: {msg}");
        }
        Ok(resp
            .result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Convert the current page's main content to Markdown for LLM consumption
    /// ("readability" mode). Picks the best content root, strips nav/script/
    /// style/aside, and walks headings/paragraphs/links/lists/code/tables to
    /// Markdown. Capped at ~20k chars. If `url` is given, navigate first.
    pub async fn extract_markdown(&self, url: Option<&str>) -> Result<String> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, url.is_some()).await?;
        if let Some(u) = url {
            // Readability/Markdown ignores pixels: default to lite.
            self.goto(&mut st, &page, u, true).await?;
        }
        let md: String = page
            .evaluate(MARKDOWN_JS)
            .await
            .context("markdown extraction failed")?
            .into_value()
            .unwrap_or_default();
        Ok(cap(md, MAX_MARKDOWN_CHARS))
    }

    /// Return console messages (log/warn/error/info/…) captured on this
    /// session's page since the last call (or since the buffer was cleared).
    /// `clear` empties the buffer after returning.
    pub async fn console(&self, clear: bool) -> Result<Vec<ConsoleMessage>> {
        // Ensure the page (and its capture listeners) exist before reading.
        {
            let mut st = self.shared.state.lock().await;
            let _ = self.ensure_page(&mut st, false).await?;
        }
        let mut buf = self.shared.console.lock().expect("console mutex poisoned");
        let out: Vec<ConsoleMessage> = buf.iter().cloned().collect();
        if clear {
            buf.clear();
        }
        Ok(out)
    }

    /// Return network requests captured on this session's page. `filter`
    /// substring-matches the URL; `clear` empties the buffer after returning.
    pub async fn network(&self, clear: bool, filter: Option<&str>) -> Result<Vec<NetworkRequest>> {
        {
            let mut st = self.shared.state.lock().await;
            let _ = self.ensure_page(&mut st, false).await?;
        }
        let mut buf = self.shared.network.lock().expect("network mutex poisoned");
        let out: Vec<NetworkRequest> = buf
            .iter()
            .filter(|r| filter.map(|f| r.url.contains(f)).unwrap_or(true))
            .cloned()
            .collect();
        if clear {
            buf.clear();
        }
        Ok(out)
    }

    /// Find the first element matching `selector` (CSS / `text=` / `role=`) and
    /// return a lightweight [`ElementRef`] handle, or `None` if nothing matches.
    /// The handle stores a stable unique CSS path and re-resolves the element on
    /// each operation, so it survives benign DOM mutations.
    pub async fn query(&self, selector: &str) -> Result<Option<ElementRef>> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        match resolve_selector(&page, selector).await {
            Ok(css) => Ok(Some(ElementRef {
                page,
                css_path: css,
            })),
            Err(_) => Ok(None),
        }
    }

    /// Return [`ElementRef`] handles for every element matching a CSS selector
    /// (a `text=`/`role=` selector resolves to at most one). Capped at 200.
    pub async fn query_all(&self, selector: &str) -> Result<Vec<ElementRef>> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        if is_special_selector(selector) {
            return Ok(match resolve_selector(&page, selector).await {
                Ok(css) => vec![ElementRef {
                    page,
                    css_path: css,
                }],
                Err(_) => Vec::new(),
            });
        }
        let paths = all_css_paths(&page, selector).await?;
        Ok(paths
            .into_iter()
            .map(|css_path| ElementRef {
                page: page.clone(),
                css_path,
            })
            .collect())
    }

    /// Find the first element matching `selector`, scroll it into view, click.
    /// Accepts CSS (default), `text=…` and `role=…[name="…"]` selectors.
    /// `timeout_ms` overrides the default 5s actionability budget for this op.
    pub async fn click(&self, selector: &str, timeout_ms: Option<u64>) -> Result<()> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let resolved = resolve_selector(&page, selector).await?;
        click_css_path(&page, &resolved, resolve_actionable_timeout(timeout_ms))
            .await
            .with_context(|| format!("click {selector:?}"))
    }

    /// Click/focus the element matching `selector` and type `text` into it.
    /// `clear` empties the current value first; `press_enter` submits after.
    /// Accepts CSS, `text=…` and `role=…` selectors. `timeout_ms` overrides the
    /// default 5s actionability budget for this op.
    pub async fn type_text(
        &self,
        selector: &str,
        text: &str,
        clear: bool,
        press_enter: bool,
        timeout_ms: Option<u64>,
    ) -> Result<()> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        perform_type(
            &page,
            selector,
            text,
            clear,
            press_enter,
            resolve_actionable_timeout(timeout_ms),
        )
        .await
    }

    /// Fill several inputs in one call. Each field is typed with `clear=true`
    /// (replace existing value). Never aborts on the first failure: returns a
    /// per-field outcome so an agent sees exactly which fields succeeded.
    /// `timeout_ms` overrides the default actionability budget per field.
    pub async fn fill_form(
        &self,
        fields: &[(String, String)],
        timeout_ms: Option<u64>,
    ) -> Result<Vec<FieldOutcome>> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let timeout = resolve_actionable_timeout(timeout_ms);
        let mut out = Vec::with_capacity(fields.len());
        for (selector, value) in fields {
            let result = perform_type(&page, selector, value, true, false, timeout).await;
            out.push(match result {
                Ok(()) => FieldOutcome {
                    selector: selector.clone(),
                    ok: true,
                    error: None,
                },
                Err(e) => FieldOutcome {
                    selector: selector.clone(),
                    ok: false,
                    error: Some(format!("{e:#}")),
                },
            });
        }
        Ok(out)
    }

    /// Select an `<option>` in a `<select>` by its `value` or visible `label`.
    /// Sets the select's value via JS and dispatches `input`+`change` events so
    /// frameworks (React/Vue/…) observe the change. Errors if neither matches.
    pub async fn select_option(
        &self,
        selector: &str,
        value: Option<&str>,
        label: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<String> {
        if value.is_none() && label.is_none() {
            bail!("select_option needs one of `value` or `label`");
        }
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let resolved = resolve_selector(&page, selector).await?;
        wait_actionable(&page, &resolved, resolve_actionable_timeout(timeout_ms)).await?;
        // Prefer value when both are supplied.
        let matcher = if let Some(v) = value {
            format!(
                "opt = opts.find(o => o.value === {});",
                serde_json_string(v)
            )
        } else {
            let l = serde_json_string(label.unwrap());
            format!(
                "opt = opts.find(o => (o.label || o.textContent || '').trim() === {l}) \
                 || opts.find(o => (o.textContent || '').trim().includes({l}));"
            )
        };
        let js = format!(
            r#"(() => {{
                const sel = document.querySelector({path});
                if (!sel) return {{ ok: false, error: 'select element not found' }};
                if (sel.tagName.toLowerCase() !== 'select')
                    return {{ ok: false, error: 'element is not a <select>' }};
                const opts = Array.from(sel.options);
                let opt = null;
                {matcher}
                if (!opt) return {{ ok: false, error: 'no matching <option>' }};
                sel.value = opt.value;
                sel.dispatchEvent(new Event('input', {{ bubbles: true }}));
                sel.dispatchEvent(new Event('change', {{ bubbles: true }}));
                return {{ ok: true, value: sel.value }};
            }})()"#,
            path = serde_json_string(&resolved),
        );
        let outcome: SelectOutcome = page
            .evaluate(js)
            .await
            .context("select_option evaluation failed")?
            .into_value()
            .context("select_option returned unexpected shape")?;
        if !outcome.ok {
            bail!(
                "select_option failed: {}",
                outcome.error.unwrap_or_else(|| "unknown".into())
            );
        }
        Ok(outcome.value.unwrap_or_default())
    }

    /// Hover the element matching `selector`: scroll into view, then move the
    /// real CDP mouse pointer to its center (so CSS `:hover` menus reveal).
    /// Accepts CSS, `text=…` and `role=…` selectors.
    pub async fn hover(&self, selector: &str, timeout_ms: Option<u64>) -> Result<()> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let resolved = resolve_selector(&page, selector).await?;
        wait_actionable(&page, &resolved, resolve_actionable_timeout(timeout_ms)).await?;
        let el = find_element(&page, &resolved).await?;
        el.hover()
            .await
            .with_context(|| format!("failed to hover element matching {selector:?}"))?;
        Ok(())
    }

    /// Go back one entry in the session page's history, then return the new
    /// title + URL.
    pub async fn navigate_back(&self) -> Result<PageInfo> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        page.evaluate("history.back()")
            .await
            .context("history.back() failed")?;
        // history.back() does not resolve on load; give the navigation a moment
        // to settle before reading the new document.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let info = read_page_info(&page, "").await?;
        st.last_url = Some(info.url.clone());
        Ok(info)
    }

    /// Arm a persistent handler for the NEXT JS dialog (alert/confirm/prompt/
    /// beforeunload) on this session's page: it is auto-accepted or dismissed
    /// per `accept`, optionally filling a prompt with `prompt_text`. Because
    /// dialogs block JS, the handler must be pre-armed *before* the action that
    /// triggers the dialog. Arming persists for all subsequent dialogs until
    /// changed (or the page is recreated).
    pub async fn handle_dialog(&self, accept: bool, prompt_text: Option<String>) -> Result<()> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        *self.shared.dialog.lock().expect("dialog mutex poisoned") = Some(DialogBehavior {
            accept,
            prompt_text,
        });
        if st.dialog_armed {
            return Ok(());
        }
        // Page domain must be enabled to receive javascriptDialogOpening.
        page.execute(PageEnableParams::default())
            .await
            .context("failed to enable Page domain for dialog handling")?;
        let mut events = page
            .event_listener::<EventJavascriptDialogOpening>()
            .await
            .context("failed to subscribe to dialog events")?;
        let page_for_task = page.clone();
        // Weak, not Arc: a strong ref here would pin the session and block Drop
        // (which closes the page), leaking the context+page. Upgrade per event.
        let weak = Arc::downgrade(&self.shared);
        // The stream ends when the page closes, so the task exits with the page.
        tokio::spawn(async move {
            while events.next().await.is_some() {
                let Some(shared) = weak.upgrade() else { break };
                let desired = shared.dialog.lock().expect("dialog mutex poisoned").clone();
                if let Some(b) = desired {
                    let mut params = HandleJavaScriptDialogParams::new(b.accept);
                    params.prompt_text = b.prompt_text;
                    if let Err(e) = page_for_task.execute(params).await {
                        tracing::warn!("failed to handle JS dialog: {e}");
                    }
                }
            }
        });
        st.dialog_armed = true;
        Ok(())
    }

    /// Capture this session's storage state — cookies (browser-context wide) +
    /// localStorage for the current origin + the current URL — as a compact
    /// JSON string the caller can persist and later feed to [`Self::restore_state`].
    pub async fn save_state(&self) -> Result<String> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let cookies = get_all_cookies(&page).await?;
        let url = page.url().await.ok().flatten().unwrap_or_default();
        let origin = page_origin(&page).await.unwrap_or_default();
        let local_storage: HashMap<String, String> = page
            .evaluate(DUMP_LOCALSTORAGE_JS)
            .await
            .context("failed to read localStorage")?
            .into_value()
            .unwrap_or_default();
        let state = serde_json::json!({
            "url": url,
            "origin": origin,
            "cookies": cookies,
            "localStorage": local_storage,
        });
        Ok(state.to_string())
    }

    /// Restore a storage state produced by [`Self::save_state`]: cookies are set
    /// immediately (browser-context wide); localStorage is applied on/after the
    /// next navigation to its origin (it can only be written while on-origin).
    /// Call order is flexible — restore then navigate, or navigate then restore.
    pub async fn restore_state(&self, state: &str) -> Result<()> {
        let parsed: serde_json::Value =
            serde_json::from_str(state).context("state is not valid JSON")?;
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;

        if let Some(cookies) = parsed.get("cookies").and_then(|c| c.as_array()) {
            set_cookies(&page, cookies).await?;
        }

        let origin = parsed
            .get("origin")
            .and_then(|o| o.as_str())
            .unwrap_or_default()
            .to_string();
        let entries: HashMap<String, String> = parsed
            .get("localStorage")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if !entries.is_empty() && !origin.is_empty() {
            // Apply now if we are already on the origin; otherwise defer to the
            // next navigation to it.
            let on_origin = page_origin(&page)
                .await
                .map(|o| o == origin)
                .unwrap_or(false);
            if on_origin {
                apply_localstorage(&page, &entries).await?;
            } else {
                st.pending_localstorage = Some((origin, entries));
            }
        }
        Ok(())
    }

    /// Assert the presence (or, with `should_exist=false`, absence) of a
    /// selector and/or body text within `timeout_ms`. Unlike [`Self::wait_for`],
    /// this NEVER errors on a failed condition: it returns a structured
    /// pass/fail so an agent can gate a feature test on the result.
    pub async fn assert(
        &self,
        selector: Option<&str>,
        text: Option<&str>,
        should_exist: bool,
        timeout_ms: Option<u64>,
    ) -> Result<AssertOutcome> {
        if selector.is_none() && text.is_none() {
            bail!("assert needs at least one of `selector` or `text`");
        }
        let timeout = Duration::from_millis(
            timeout_ms
                .unwrap_or(WAIT_FOR_DEFAULT_TIMEOUT_MS)
                .min(WAIT_FOR_MAX_TIMEOUT_MS),
        );
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let mut conds = Vec::new();
        let mut checked = Vec::new();
        if let Some(sel) = selector {
            conds.push(format!("(({}) !== null)", resolve_iife(sel)));
            checked.push(format!("selector {sel:?}"));
        }
        if let Some(t) = text {
            conds.push(format!(
                "!!(document.body && document.body.innerText.includes({}))",
                serde_json_string(t)
            ));
            checked.push(format!("text {t:?}"));
        }
        let js = conds.join(" && ");
        let checked = format!(
            "{} {}",
            checked.join(" and "),
            if should_exist { "present" } else { "absent" }
        );
        let started = Instant::now();
        // Last successfully-evaluated presence; a transient CDP failure just
        // keeps polling (assert never errors on a failed condition, and a poll
        // hiccup is not a condition result).
        let mut present = false;
        loop {
            if let Ok(v) = poll_condition(&page, &js, poll_budget(timeout, started)).await {
                present = v;
                // `present == should_exist` is the passing state for both modes.
                if present == should_exist {
                    return Ok(AssertOutcome {
                        passed: true,
                        checked,
                        found: present,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    });
                }
            }
            if started.elapsed() >= timeout {
                return Ok(AssertOutcome {
                    passed: false,
                    checked,
                    found: present,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                });
            }
            // Keep the browser from being idle-reaped mid-assert.
            self.shared.engine.touch_activity().await;
            tokio::time::sleep(WAIT_FOR_POLL).await;
        }
    }

    /// Send a single key (DOM key value: Enter, Tab, Escape, ArrowDown, "a" …)
    /// to whatever is focused on the current page.
    pub async fn press_key(&self, key: &str) -> Result<()> {
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        dispatch_key(&page, key).await
    }

    /// Poll every 100ms until `selector` matches and/or `text` appears in the
    /// body, up to `timeout_ms` (default 10s, capped at 30s). Returns elapsed
    /// milliseconds on success.
    pub async fn wait_for(
        &self,
        selector: Option<&str>,
        text: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<u64> {
        if selector.is_none() && text.is_none() {
            bail!("wait_for needs at least one of `selector` or `text`");
        }
        let timeout = Duration::from_millis(
            timeout_ms
                .unwrap_or(WAIT_FOR_DEFAULT_TIMEOUT_MS)
                .min(WAIT_FOR_MAX_TIMEOUT_MS),
        );
        let mut st = self.shared.state.lock().await;
        let page = self.ensure_page(&mut st, false).await?;
        let mut conds = Vec::new();
        if let Some(sel) = selector {
            // resolve_iife handles CSS as well as text=/role= selectors and
            // returns null when nothing matches.
            conds.push(format!("(({}) !== null)", resolve_iife(sel)));
        }
        if let Some(t) = text {
            conds.push(format!(
                "!!(document.body && document.body.innerText.includes({}))",
                serde_json_string(t)
            ));
        }
        let js = conds.join(" && ");
        let started = Instant::now();
        loop {
            // A single transient CDP hiccup (e.g. "Request timed out" on a
            // loaded runner) must NOT abort the wait — bound the poll, treat any
            // failure as "not yet satisfied", and keep polling until the
            // deadline. Surface the last poll's error only if we actually time
            // out.
            let outcome = poll_condition(&page, &js, poll_budget(timeout, started)).await;
            if matches!(outcome, Ok(true)) {
                return Ok(started.elapsed().as_millis() as u64);
            }
            if started.elapsed() >= timeout {
                return Err(match outcome {
                    Err(e) => anyhow::anyhow!(
                        "wait_for timed out after {}ms (selector: {:?}, text: {:?}); last evaluation error: {}",
                        timeout.as_millis(),
                        selector,
                        text,
                        e
                    ),
                    Ok(_) => anyhow::anyhow!(
                        "wait_for timed out after {}ms (selector: {:?}, text: {:?})",
                        timeout.as_millis(),
                        selector,
                        text
                    ),
                });
            }
            // Keep the browser from being idle-reaped mid-wait.
            self.shared.engine.touch_activity().await;
            tokio::time::sleep(WAIT_FOR_POLL).await;
        }
    }

    /// Explicitly close the session's page and browser context.
    pub async fn close(&self) {
        let (page, ctx) = {
            let mut st = self.shared.state.lock().await;
            st.last_url = None;
            (st.page.take(), st.context_id.take())
        };
        if let Some(p) = page {
            let _ = p.close().await;
        }
        if let Some(id) = ctx {
            self.shared.engine.dispose_context(id).await;
        }
    }

    // -- internals -------------------------------------------------------------

    /// Return the session's live page, (re)creating it as needed. Touches the
    /// engine's `last_used` so the idle reaper never kills an active session.
    /// When the browser was reaped/crashed since the last op, in-page state is
    /// gone: recreate transparently when the caller is about to navigate,
    /// otherwise return a polite error telling the agent to navigate again.
    async fn ensure_page(&self, st: &mut SessionState, navigating: bool) -> Result<Page> {
        let engine = &self.shared.engine;
        let mut guard = engine.handle.lock().await;
        engine.launch_if_needed(&mut guard).await?;
        let handle = guard.as_mut().unwrap();
        handle.last_used = Instant::now();

        if let Some(page) = &st.page {
            if st.generation == handle.generation {
                return Ok(page.clone());
            }
        }
        // First use, or the page belongs to a dead browser launch.
        let lost_state = st.page.take().is_some();
        st.context_id = None;
        if lost_state {
            st.last_url = None;
            if !navigating {
                bail!(
                    "the browser was restarted (idle reap or crash) and this session's \
                     page state was lost — call browser_navigate to start again"
                );
            }
        }
        // Prefer a pre-warmed context from the pool (zero context-creation
        // latency); the pool holds FRESH, unused contexts, so cookie isolation
        // between sessions is preserved.
        let (ctx, page) = if let Some(pooled) = handle.pool.pop() {
            tracing::debug!(
                pool = handle.pool.len(),
                "session took a pre-warmed context from the pool"
            );
            (Some(pooled.context_id), pooled.page)
        } else {
            // Cold path: dedicated browser context for cookie isolation between
            // MCP sessions; fall back to the shared default context on failure.
            let ctx = match handle
                .browser
                .create_browser_context(CreateBrowserContextParams::default())
                .await
            {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!("browser context creation failed, using shared context: {e}");
                    None
                }
            };
            let mut params = CreateTargetParams::new("about:blank");
            params.browser_context_id = ctx.clone();
            let page = handle
                .browser
                .new_page(params)
                .await
                .context("failed to create session page")?;
            (ctx, page)
        };
        st.page = Some(page.clone());
        st.context_id = ctx;
        st.generation = handle.generation;
        // A fresh page has no dialog handler wired onto it yet.
        st.dialog_armed = false;
        // Release the engine handle lock before arming capture (it only touches
        // the page + the session's std-Mutex buffers, no async locks).
        drop(guard);
        self.arm_capture(&page).await;
        // Lazily top the pool back up in the background (never blocks this call
        // and never keeps the browser alive).
        engine.spawn_pool_refill();
        tracing::debug!(relaunched = lost_state, "session page created");
        Ok(page)
    }

    /// Wire persistent console (Runtime.consoleAPICalled + Log.entryAdded) and
    /// network (Network.requestWillBeSent + responseReceived) listeners onto a
    /// freshly created session page, buffering into the session's capture
    /// buffers. Each stream ends when the page closes, so the tasks exit with
    /// the page (same pattern as the dialog handler). Best-effort: a failure to
    /// enable a domain just means that stream stays empty.
    async fn arm_capture(&self, page: &Page) {
        // Enable the three capture domains concurrently: these are independent
        // CDP round-trips, so issuing them in parallel shaves ~2 round-trips off
        // the page-setup path that precedes every first navigation.
        let (_, _, _) = tokio::join!(
            page.execute(RuntimeEnableParams::default()),
            page.execute(LogEnableParams::default()),
            page.execute(NetworkEnableParams::default()),
        );

        // The tasks below hold a WEAK ref to the session, upgrading only to push
        // an event. Holding a strong `Arc` would pin `SessionShared` forever —
        // each stream ends only when the page closes, but the page closes in
        // `Drop`, which can't run while a strong ref is parked here. That cycle
        // leaked a context + page per session (Drop never fired). With a Weak,
        // the last real Arc dropping lets Drop run, which closes the page and
        // ends these streams.
        if let Ok(mut events) = page.event_listener::<EventConsoleApiCalled>().await {
            let weak = Arc::downgrade(&self.shared);
            tokio::spawn(async move {
                while let Some(e) = events.next().await {
                    let Some(shared) = weak.upgrade() else { break };
                    push_capped(
                        &shared.console,
                        ConsoleMessage {
                            level: e.r#type.as_ref().to_string(),
                            text: console_args_to_text(&e.args),
                        },
                    );
                }
            });
        }
        if let Ok(mut events) = page.event_listener::<EventEntryAdded>().await {
            let weak = Arc::downgrade(&self.shared);
            tokio::spawn(async move {
                while let Some(e) = events.next().await {
                    let Some(shared) = weak.upgrade() else { break };
                    push_capped(
                        &shared.console,
                        ConsoleMessage {
                            level: e.entry.level.as_ref().to_string(),
                            text: e.entry.text.clone(),
                        },
                    );
                }
            });
        }
        if let Ok(mut events) = page.event_listener::<EventRequestWillBeSent>().await {
            let weak = Arc::downgrade(&self.shared);
            tokio::spawn(async move {
                while let Some(e) = events.next().await {
                    let Some(shared) = weak.upgrade() else { break };
                    push_capped(
                        &shared.network,
                        NetworkRequest {
                            request_id: e.request_id.inner().clone(),
                            method: e.request.method.clone(),
                            url: e.request.url.clone(),
                            status: None,
                            resource_type: e.r#type.as_ref().map(|t| t.as_ref().to_string()),
                        },
                    );
                }
            });
        }
        if let Ok(mut events) = page.event_listener::<EventResponseReceived>().await {
            let weak = Arc::downgrade(&self.shared);
            tokio::spawn(async move {
                while let Some(e) = events.next().await {
                    let Some(shared) = weak.upgrade() else { break };
                    let rid = e.request_id.inner();
                    let status = e.response.status;
                    // Named binding (drops before `shared`) so the guard can't
                    // outlive the per-iteration upgraded Arc.
                    let locked = shared.network.lock();
                    if let Ok(mut buf) = locked {
                        // Fill in the status on the most recent matching request.
                        if let Some(item) = buf.iter_mut().rev().find(|r| &r.request_id == rid) {
                            item.status = Some(status);
                        }
                    }
                }
            });
        }
    }

    /// Navigate the session page. `goto` resolves once the page is loaded.
    /// `lite` toggles resource blocking for this navigation (applied before the
    /// load so blocked resources are never fetched).
    async fn goto(&self, st: &mut SessionState, page: &Page, url: &str, lite: bool) -> Result<()> {
        validate_navigation_url(url)?;
        apply_blocking(page, lite).await;
        tokio::time::timeout(self.shared.engine.config.nav_timeout, page.goto(url))
            .await
            .context("navigation timed out")?
            .with_context(|| format!("navigation to {url} failed"))?;
        st.last_url = Some(
            page.url()
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| url.to_string()),
        );
        // If a restored state left localStorage pending for this origin, apply
        // it now that we are on the origin (localStorage is origin-scoped).
        if let Some((origin, _)) = &st.pending_localstorage {
            let on_origin = page_origin(page)
                .await
                .map(|o| &o == origin)
                .unwrap_or(false);
            if on_origin {
                let (_, entries) = st.pending_localstorage.take().unwrap();
                if let Err(e) = apply_localstorage(page, &entries).await {
                    tracing::warn!("failed to apply restored localStorage: {e:#}");
                }
            }
        }
        Ok(())
    }
}

// -- shared page operations -----------------------------------------------------

/// Enable ("lite mode") or clear resource blocking on a page via CDP
/// `Network.setBlockedURLs`. Requires the Network domain to be enabled first
/// (armed in [`BrowserSession::arm_capture`] at page creation).
///
/// Sent via [`RawCommand`] rather than the typed `SetBlockedUrLsParams`: the
/// typed builder marks `urls` `skip_serializing_if = "Vec::is_empty"`, so an
/// empty vec would be dropped from the payload and Chromium would reject the
/// call — we must send an explicit `{"urls": []}` to CLEAR a prior block list.
async fn apply_blocking(page: &Page, enabled: bool) {
    let urls: Vec<&str> = if enabled {
        LITE_BLOCK_PATTERNS.to_vec()
    } else {
        Vec::new()
    };
    if let Err(e) = page
        .execute(RawCommand {
            id: "Network.setBlockedURLs",
            params: serde_json::json!({ "urls": urls }),
        })
        .await
    {
        tracing::debug!(enabled, "Network.setBlockedURLs failed: {e}");
    }
}

async fn read_page_info(page: &Page, fallback_url: &str) -> Result<PageInfo> {
    let title = page.get_title().await?.unwrap_or_default();
    let text: String = page
        .evaluate("document.body ? document.body.innerText : ''")
        .await?
        .into_value()
        .unwrap_or_default();
    let url = page
        .url()
        .await?
        .unwrap_or_else(|| fallback_url.to_string());
    Ok(PageInfo {
        url,
        title,
        text: cap(text, MAX_TEXT_CHARS),
    })
}

async fn capture_png(page: &Page, full_page: bool) -> Result<Vec<u8>> {
    page.screenshot(
        ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .full_page(full_page)
            .build(),
    )
    .await
    .context("screenshot failed")
}

async fn extract_from_page(
    page: &Page,
    selector: &str,
    attribute: Option<&str>,
) -> Result<Vec<String>> {
    // Plain CSS extracts from all matches (up to 50). A text=/role= selector
    // resolves to a single element, so extract just that one.
    let query = if is_special_selector(selector) {
        match resolve_selector(page, selector).await {
            Ok(css) => css,
            Err(_) => return Ok(Vec::new()),
        }
    } else {
        selector.to_string()
    };
    let js = format!(
        r#"Array.from(document.querySelectorAll({sel})).slice(0, 50).map(el => {expr})"#,
        sel = serde_json_string(&query),
        expr = match attribute {
            Some(attr) => format!("el.getAttribute({}) || ''", serde_json_string(attr)),
            None => "(el.innerText || el.textContent || '').trim()".to_string(),
        }
    );
    let values: Vec<String> = page.evaluate(js).await?.into_value().unwrap_or_default();
    Ok(values)
}

async fn find_element(page: &Page, selector: &str) -> Result<chromiumoxide::Element> {
    page.find_element(selector)
        .await
        .map_err(|e| anyhow::anyhow!("no element matches selector {selector:?}: {e}"))
}

fn actionable_timeout() -> Duration {
    Duration::from_millis(ACTIONABLE_DEFAULT_TIMEOUT_MS)
}

/// Resolve an optional per-op actionability timeout (milliseconds, capped at
/// [`WAIT_FOR_MAX_TIMEOUT_MS`]) to a [`Duration`], defaulting to the standard
/// budget when unset.
fn resolve_actionable_timeout(timeout_ms: Option<u64>) -> Duration {
    match timeout_ms {
        Some(ms) => Duration::from_millis(ms.min(WAIT_FOR_MAX_TIMEOUT_MS)),
        None => actionable_timeout(),
    }
}

/// Scroll a resolved CSS element into view, wait until it is actionable, then
/// click it. Shared by [`BrowserSession::click`] and [`ElementRef::click`].
async fn click_css_path(page: &Page, css_path: &str, timeout: Duration) -> Result<()> {
    wait_actionable(page, css_path, timeout).await?;
    let el = find_element(page, css_path).await?;
    let _ = el.scroll_into_view().await; // best effort; click re-checks the point
    el.click().await.context("click failed")?;
    Ok(())
}

/// In-page actionability check: returns `{state, rect:[x,y,w,h]}` where `state`
/// is one of ok / notfound / hidden / disabled / covered.
const ACTIONABLE_CHECK_JS: &str = r#"(() => {
    const el = document.querySelector(SEL);
    if (!el) return { state: 'notfound' };
    const style = window.getComputedStyle(el);
    if (!style || style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0')
        return { state: 'hidden' };
    const rect = el.getBoundingClientRect();
    if ((rect.width === 0 && rect.height === 0) && !el.offsetParent)
        return { state: 'hidden' };
    const disabled = (('disabled' in el) && el.disabled)
        || el.hasAttribute('disabled')
        || el.getAttribute('aria-disabled') === 'true';
    if (disabled) return { state: 'disabled', rect: [rect.x, rect.y, rect.width, rect.height] };
    const cx = rect.left + rect.width / 2, cy = rect.top + rect.height / 2;
    let covered = false;
    // Only meaningful when the center point is within the viewport.
    if (cx >= 0 && cy >= 0 && cx <= window.innerWidth && cy <= window.innerHeight) {
        const top = document.elementFromPoint(cx, cy);
        covered = !(top && (top === el || el.contains(top) || top.contains(el)));
    }
    if (covered) return { state: 'covered', rect: [rect.x, rect.y, rect.width, rect.height] };
    return { state: 'ok', rect: [rect.x, rect.y, rect.width, rect.height] };
})()"#;

/// Evaluate `js` on the page, retrying up to twice on a transient CDP error
/// (the channel can flake mid-navigation). Returns the JSON value (or Null).
async fn eval_json_retry(page: &Page, js: &str) -> Result<serde_json::Value> {
    let mut last: Option<String> = None;
    for attempt in 0..3 {
        match page.evaluate(js.to_string()).await {
            Ok(r) => return Ok(r.into_value().unwrap_or(serde_json::Value::Null)),
            Err(e) => {
                last = Some(e.to_string());
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
    bail!(
        "CDP evaluate failed after retries: {}",
        last.unwrap_or_default()
    )
}

/// Poll until the element at `css_path` is actionable: present AND visible AND
/// enabled AND not covered by another element AND geometrically stable across
/// two consecutive samples. On timeout, returns an error naming the specific
/// blocker (not found / not visible / disabled / covered / unstable) so an
/// agent can react. Fast path: a settled, visible, enabled element passes on
/// the first poll (after one ~2-frame stability re-sample).
async fn wait_actionable(page: &Page, css_path: &str, timeout: Duration) -> Result<()> {
    let js = ACTIONABLE_CHECK_JS.replace("SEL", &serde_json_string(css_path));
    let started = Instant::now();
    loop {
        let v = eval_json_retry(page, &js).await?;
        let state = v
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("notfound");
        let blocker = if state == "ok" {
            let rect1 = v.get("rect").cloned();
            // Re-sample after ~2 frames: the element must not still be moving.
            tokio::time::sleep(Duration::from_millis(40)).await;
            let v2 = eval_json_retry(page, &js).await?;
            let stable = v2.get("state").and_then(|s| s.as_str()) == Some("ok")
                && rects_equal(rect1.as_ref(), v2.get("rect"));
            if stable {
                return Ok(());
            }
            "unstable"
        } else {
            state
        };
        if started.elapsed() >= timeout {
            bail!(actionable_error(css_path, blocker, timeout));
        }
        tokio::time::sleep(ACTIONABLE_POLL).await;
    }
}

fn rects_equal(a: Option<&serde_json::Value>, b: Option<&serde_json::Value>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn actionable_error(css_path: &str, blocker: &str, timeout: Duration) -> String {
    let ms = timeout.as_millis();
    match blocker {
        "notfound" => format!("element {css_path:?} not found within {ms}ms"),
        "hidden" => format!(
            "element {css_path:?} is present but not visible (display:none / visibility:hidden / \
             zero-size) after {ms}ms"
        ),
        "disabled" => {
            format!("element {css_path:?} is present but disabled after {ms}ms")
        }
        "covered" => format!(
            "element {css_path:?} is covered by another element (not the top element at its \
             center point) after {ms}ms"
        ),
        "unstable" => format!(
            "element {css_path:?} never became stable (still animating/moving) within {ms}ms"
        ),
        other => format!("element {css_path:?} not actionable ({other}) within {ms}ms"),
    }
}

/// Return unique CSS paths for every element matching a plain CSS selector
/// (capped at 200). Reuses the same `nth-of-type` path builder as
/// [`resolve_iife`] so the paths round-trip through `querySelector`.
async fn all_css_paths(page: &Page, css: &str) -> Result<Vec<String>> {
    let js = format!(
        r#"(() => {{
            const cssPath = (el) => {{
                if (!el || el.nodeType !== 1) return null;
                const parts = [];
                let node = el;
                while (node && node.nodeType === 1 && node !== document.documentElement) {{
                    let seg = node.nodeName.toLowerCase();
                    const parent = node.parentElement;
                    if (!parent) {{ parts.unshift(seg); break; }}
                    const sib = Array.from(parent.children).filter(c => c.nodeName === node.nodeName);
                    if (sib.length > 1) seg += ':nth-of-type(' + (sib.indexOf(node) + 1) + ')';
                    parts.unshift(seg);
                    node = parent;
                }}
                return parts.length ? parts.join(' > ') : null;
            }};
            return Array.from(document.querySelectorAll({sel}))
                .slice(0, 200)
                .map(cssPath)
                .filter(Boolean);
        }})()"#,
        sel = serde_json_string(css),
    );
    let paths: Vec<String> = page
        .evaluate(js)
        .await
        .context("query_all path resolution failed")?
        .into_value()
        .unwrap_or_default();
    Ok(paths)
}

/// Compute the accessibility-tree snapshot text (with DOM-outline fallback).
/// Shared by [`BrowserSession::snapshot`] and [`BrowserSession::snapshot_diff`].
async fn compute_snapshot(page: &Page) -> Result<String> {
    match ax_snapshot(page).await {
        Ok(s) if !s.trim().is_empty() => Ok(s),
        Ok(_) => dom_outline(page).await,
        Err(e) => {
            tracing::warn!("AX snapshot failed, falling back to DOM outline: {e:#}");
            dom_outline(page).await
        }
    }
}

/// Line-based diff between two snapshots: report added (`+`) and removed (`-`)
/// role/name lines. Order-insensitive (a moved line is not reported as a
/// change) so the output stays focused on what actually appeared/disappeared.
fn diff_snapshots(prev: &str, current: &str) -> String {
    use std::collections::HashSet;
    let prev_set: HashSet<&str> = prev
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let cur_set: HashSet<&str> = current
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut out = String::from("snapshot diff (vs previous snapshot in this session):\n");
    let mut changed = false;
    for line in current.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !prev_set.contains(line) {
            out.push_str("+ ");
            out.push_str(line);
            out.push('\n');
            changed = true;
        }
    }
    for line in prev.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !cur_set.contains(line) {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
            changed = true;
        }
    }
    if !changed {
        out.push_str("(no changes since last snapshot)\n");
    }
    out
}

/// Map a paper-size name to (width, height) in inches. Defaults to A4.
/// Replace the page's document with `html` via CDP `Page.setDocumentContent`
/// on the main frame. The HTML travels as a protocol string field (not
/// interpolated into a script), so large documents are handled safely.
async fn set_document_content(page: &Page, html: &str) -> Result<()> {
    let frame_id = page
        .mainframe()
        .await
        .context("failed to resolve the page's main frame")?
        .ok_or_else(|| anyhow::anyhow!("page has no main frame yet"))?;
    page.execute(SetDocumentContentParams::new(frame_id, html.to_string()))
        .await
        .context("Page.setDocumentContent failed")?;
    Ok(())
}

/// Poll `document.readyState` until it reaches one of `accepted`, or `deadline`
/// passes (returns Ok anyway once the deadline is hit — a best-effort wait that
/// never wedges a caller, matching puppeteer's lenient setContent semantics).
async fn wait_ready_state(page: &Page, accepted: &[&str], deadline: Instant) -> Result<()> {
    loop {
        let state: String = page
            .evaluate("document.readyState")
            .await
            .context("reading document.readyState failed")?
            .into_value()
            .unwrap_or_default();
        if accepted.iter().any(|a| *a == state) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            tracing::debug!(state, "set_content wait timed out; proceeding");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Convert a CSS length string (`"35px"`, `"20mm"`, `"1cm"`, `"0.5in"`,
/// `"12pt"`, or a bare number treated as px) to inches for CDP `printToPDF`.
/// Returns `None` for empty/unparseable input so the caller can fall back to a
/// default. Unknown units are treated as px (Chromium's own fallback).
fn css_len_to_inches(len: Option<&str>) -> Option<f64> {
    let raw = len?.trim();
    if raw.is_empty() {
        return None;
    }
    // Split the trailing alphabetic unit from the leading numeric part.
    let split = raw.find(|c: char| c.is_alphabetic()).unwrap_or(raw.len());
    let (num, unit) = raw.split_at(split);
    let value: f64 = num.trim().parse().ok()?;
    let inches = match unit.trim().to_ascii_lowercase().as_str() {
        "in" => value,
        "mm" => value / 25.4,
        "cm" => value / 2.54,
        "pt" => value / 72.0,
        "pc" => value / 6.0,
        // "px", "", and anything else → CSS px (96 px per inch).
        _ => value / 96.0,
    };
    Some(inches)
}

fn paper_size(format: Option<&str>) -> (f64, f64) {
    match format.map(|f| f.trim().to_ascii_lowercase()).as_deref() {
        Some("letter") => (8.5, 11.0),
        Some("legal") => (8.5, 14.0),
        Some("a3") => (11.69, 16.54),
        // A4 is the default (also for any unrecognized value).
        _ => (8.27, 11.69),
    }
}

/// Render a console.* call's arguments into a single text line, using each
/// argument's JSON value when present, else its description, else its type.
fn console_args_to_text(args: &[RemoteObject]) -> String {
    args.iter()
        .map(|a| {
            if let Some(v) = &a.value {
                match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                }
            } else if let Some(d) = &a.description {
                d.clone()
            } else {
                a.r#type.as_ref().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Push into a bounded capture buffer, dropping the oldest entry past the cap.
fn push_capped<T>(buf: &std::sync::Mutex<VecDeque<T>>, item: T) {
    if let Ok(mut q) = buf.lock() {
        if q.len() >= CAPTURE_CAP {
            q.pop_front();
        }
        q.push_back(item);
    }
}

/// Focus and type into the element matching `selector` (resolving CSS/text=/
/// role= selectors). Shared by `type_text` and `fill_form`.
async fn perform_type(
    page: &Page,
    selector: &str,
    text: &str,
    clear: bool,
    press_enter: bool,
    timeout: Duration,
) -> Result<()> {
    let resolved = resolve_selector(page, selector).await?;
    wait_actionable(page, &resolved, timeout).await?;
    let el = find_element(page, &resolved).await?;
    let _ = el.scroll_into_view().await;
    el.click()
        .await
        .with_context(|| format!("failed to focus element matching {selector:?}"))?;
    if clear {
        el.call_js_fn(
            "function() { \
               if ('value' in this) { \
                 this.value = ''; \
                 this.dispatchEvent(new Event('input', { bubbles: true })); \
               } else { this.textContent = ''; } \
             }",
            false,
        )
        .await
        .context("failed to clear element value")?;
    }
    el.type_str(text).await.context("failed to type text")?;
    if press_enter {
        el.press_key("Enter")
            .await
            .context("failed to press Enter")?;
    }
    Ok(())
}

/// Send a key down/up pair via CDP `Input.dispatchKeyEvent`. chromiumoxide
/// only exposes key presses on `Element`, so this replicates its key handling
/// (US-layout `KeyDefinition` table) at the page level for `browser_press_key`.
async fn dispatch_key(page: &Page, key: &str) -> Result<()> {
    let def = chromiumoxide::keys::get_key_definition(key).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown key {key:?} — use DOM key values like Enter, Tab, Escape, \
             Backspace, ArrowDown, PageDown, a, 1"
        )
    })?;
    let mut cmd = DispatchKeyEventParams::builder();
    // Keys that produce text send KeyDown (so a `text` payload is delivered);
    // pure control keys must be RawKeyDown. Mirrors puppeteer's Input.js.
    let key_down_type = if let Some(text) = def.text {
        cmd = cmd.text(text);
        DispatchKeyEventType::KeyDown
    } else if def.key.len() == 1 {
        cmd = cmd.text(def.key);
        DispatchKeyEventType::KeyDown
    } else {
        DispatchKeyEventType::RawKeyDown
    };
    cmd = cmd
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.key_code)
        .native_virtual_key_code(def.key_code);
    let down = cmd
        .clone()
        .r#type(key_down_type)
        .build()
        .map_err(|e| anyhow::anyhow!(e))?;
    let up = cmd
        .r#type(DispatchKeyEventType::KeyUp)
        .build()
        .map_err(|e| anyhow::anyhow!(e))?;
    execute_key_event_retry(page, down, "key down").await?;
    execute_key_event_retry(page, up, "key up").await?;
    Ok(())
}

/// Send one CDP `Input.dispatchKeyEvent`, retrying up to 3 times on a
/// transient/timeout CDP error. The observed CI flake is a "Request timed out"
/// on the key-up (or key-down) send on slow/loaded macOS runners, which the
/// larger `request_timeout` alone did not eliminate. A key down/up is
/// idempotent for the control keys agents send through here (Escape/Tab/arrows/
/// Enter), so re-sending after a timed-out attempt is safe.
async fn execute_key_event_retry(
    page: &Page,
    params: DispatchKeyEventParams,
    what: &str,
) -> Result<()> {
    let mut last: Option<String> = None;
    for attempt in 0..3 {
        match page.execute(params.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last = Some(e.to_string());
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
    bail!("{what} failed after retries: {}", last.unwrap_or_default())
}

// -- public result types ----------------------------------------------------------

/// Per-field result of [`BrowserSession::fill_form`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct FieldOutcome {
    pub selector: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// Options for [`BrowserSession::pdf`], mirroring the puppeteer `page.pdf()` /
/// CDP `Page.printToPDF` parameter set that invoice-service (and similar HTML→
/// PDF services) rely on. All fields map straight onto CDP; margins accept CSS
/// length strings (`"35px"`, `"20mm"`, `"0.5in"`) and are converted to the
/// inches CDP expects (see [`css_len_to_inches`]). Unset margins default to 0.
#[derive(Debug, Clone, Default)]
pub struct PdfOptions {
    /// Paper size name: A4 (default) / Letter / Legal / A3.
    pub format: Option<String>,
    /// Landscape orientation (default portrait).
    pub landscape: bool,
    /// Print background graphics/colors (default false).
    pub print_background: bool,
    /// Render the header/footer templates into the PDF. Puppeteer's
    /// `displayHeaderFooter`. Must be true for [`Self::footer_template`] /
    /// [`Self::header_template`] to appear.
    pub display_header_footer: bool,
    /// HTML template for the page header. Uses the CDP class hooks
    /// (`date`/`title`/`url`/`pageNumber`/`totalPages`). Ignored unless
    /// [`Self::display_header_footer`] is true.
    pub header_template: Option<String>,
    /// HTML template for the page footer (same class hooks as the header). This
    /// is what invoice-service uses to stamp legal text + page numbers on every
    /// page. Ignored unless [`Self::display_header_footer`] is true.
    pub footer_template: Option<String>,
    /// Top margin as a CSS length (e.g. `"20px"`, `"1cm"`). Defaults to 0.
    pub margin_top: Option<String>,
    /// Bottom margin as a CSS length. Defaults to 0.
    pub margin_bottom: Option<String>,
    /// Left margin as a CSS length. Defaults to 0.
    pub margin_left: Option<String>,
    /// Right margin as a CSS length. Defaults to 0.
    pub margin_right: Option<String>,
    /// Scale of the page rendering (CDP default 1.0). `None` leaves it at 1.
    pub scale: Option<f64>,
    /// Prefer the page size declared by CSS `@page` over `format`. Puppeteer's
    /// `preferCSSPageSize`.
    pub prefer_css_page_size: bool,
}

/// A console message captured on a session page (see [`BrowserSession::console`]).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsoleMessage {
    /// log / debug / info / warning / error (or a Log-domain level).
    pub level: String,
    pub text: String,
}

/// A network request captured on a session page (see [`BrowserSession::network`]).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NetworkRequest {
    /// CDP request id, used internally to attach the response status. Not part
    /// of the serialized output.
    #[serde(skip)]
    request_id: String,
    pub method: String,
    pub url: String,
    /// HTTP status once the response is seen (None while still pending).
    pub status: Option<i64>,
    /// Resource type as perceived by Chromium (Document, Script, XHR, …).
    pub resource_type: Option<String>,
}

/// A lightweight element handle: the foundation for a future JS/Python facade
/// where `const el = await page.$(sel); await el.click()`. It stores a stable
/// unique CSS path and re-resolves the live element on each operation (so it
/// tolerates benign DOM churn) rather than holding a fragile remote object.
#[derive(Clone)]
pub struct ElementRef {
    page: Page,
    css_path: String,
}

impl ElementRef {
    /// The resolved unique CSS path this handle points at.
    pub fn selector(&self) -> &str {
        &self.css_path
    }

    /// Wait until actionable, then click.
    pub async fn click(&self) -> Result<()> {
        click_css_path(&self.page, &self.css_path, actionable_timeout()).await
    }

    /// Wait until actionable, focus, then type `text` (does not clear first).
    pub async fn type_str(&self, text: &str) -> Result<()> {
        wait_actionable(&self.page, &self.css_path, actionable_timeout()).await?;
        let el = find_element(&self.page, &self.css_path).await?;
        let _ = el.scroll_into_view().await;
        el.click().await.context("failed to focus element")?;
        el.type_str(text).await.context("failed to type text")?;
        Ok(())
    }

    /// The element's visible inner text.
    pub async fn text(&self) -> Result<String> {
        let el = find_element(&self.page, &self.css_path).await?;
        Ok(el.inner_text().await?.unwrap_or_default())
    }

    /// The value of an attribute, or None if unset.
    pub async fn attribute(&self, name: &str) -> Result<Option<String>> {
        let el = find_element(&self.page, &self.css_path).await?;
        Ok(el.attribute(name).await?)
    }

    /// The element's bounding box as (x, y, width, height) in CSS pixels.
    pub async fn bounding_box(&self) -> Result<(f64, f64, f64, f64)> {
        let el = find_element(&self.page, &self.css_path).await?;
        let b = el.bounding_box().await?;
        Ok((b.x, b.y, b.width, b.height))
    }
}

/// Structured result of [`BrowserSession::assert`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct AssertOutcome {
    /// Whether the assertion held within the timeout.
    pub passed: bool,
    /// Human-readable description of what was checked.
    pub checked: String,
    /// Whether the target was present at the moment the decision was made.
    pub found: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, serde::Deserialize)]
struct SelectOutcome {
    ok: bool,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

// -- selector resolution (CSS default, plus text= and role=) ----------------------

fn is_special_selector(selector: &str) -> bool {
    selector.starts_with("text=") || selector.starts_with("role=")
}

/// Resolve a selector to a concrete CSS selector string that `find_element`/
/// `querySelector` can use. Plain CSS is returned as-is (when it matches);
/// `text=…` and `role=…[name="…"]` are resolved in-page to a unique CSS path.
/// Errors when nothing matches.
async fn resolve_selector(page: &Page, selector: &str) -> Result<String> {
    let resolved: Option<String> = page
        .evaluate(resolve_iife(selector))
        .await
        .context("selector resolution failed")?
        .into_value()
        .unwrap_or(None);
    resolved.ok_or_else(|| anyhow::anyhow!("no element matches selector {selector:?}"))
}

/// Build a self-contained JS IIFE that evaluates to a unique CSS path string
/// for the element matching `selector`, or `null` when nothing matches. Used
/// both by `resolve_selector` and inline by `wait_for`/`assert` (existence).
///
/// Role resolution is a pragmatic JS heuristic (implicit-role element map +
/// accessible-name from aria-label/aria-labelledby/associated label/text/
/// value/placeholder/title/alt), not a full ARIA computed-name implementation;
/// it covers the common interactive roles agents target.
fn resolve_iife(selector: &str) -> String {
    format!(
        r#"(() => {{
        const SEL = {sel};
        const t = (s) => (s || '').replace(/\s+/g, ' ').trim();
        const visText = (el) => t(el.innerText || el.textContent || '');
        const cssPath = (el) => {{
            if (!el || el.nodeType !== 1) return null;
            const parts = [];
            let node = el;
            while (node && node.nodeType === 1 && node !== document.documentElement) {{
                let seg = node.nodeName.toLowerCase();
                const parent = node.parentElement;
                if (!parent) {{ parts.unshift(seg); break; }}
                const sib = Array.from(parent.children).filter(c => c.nodeName === node.nodeName);
                if (sib.length > 1) seg += ':nth-of-type(' + (sib.indexOf(node) + 1) + ')';
                parts.unshift(seg);
                node = parent;
            }}
            return parts.length ? parts.join(' > ') : null;
        }};
        const accName = (el) => {{
            let n = t(el.getAttribute && el.getAttribute('aria-label'));
            if (n) return n;
            const lb = el.getAttribute && el.getAttribute('aria-labelledby');
            if (lb) {{
                const s = t(lb.split(/\s+/).map(id => {{
                    const e = document.getElementById(id); return e ? visText(e) : '';
                }}).join(' '));
                if (s) return s;
            }}
            if (el.id) {{
                const lab = document.querySelector('label[for="' + el.id.replace(/"/g, '\\"') + '"]');
                if (lab) {{ const s = visText(lab); if (s) return s; }}
            }}
            if (el.closest) {{ const w = el.closest('label'); if (w) {{ const s = visText(w); if (s) return s; }} }}
            let s = visText(el); if (s) return s;
            s = t(el.getAttribute && el.getAttribute('value')); if (s) return s;
            s = t(el.getAttribute && el.getAttribute('placeholder')); if (s) return s;
            s = t(el.getAttribute && el.getAttribute('title')); if (s) return s;
            s = t(el.getAttribute && el.getAttribute('alt')); if (s) return s;
            return '';
        }};
        const isHidden = (el) => {{
            const st = window.getComputedStyle(el);
            return !st || st.display === 'none' || st.visibility === 'hidden';
        }};
        let target = null;
        if (SEL.slice(0, 5) === 'text=') {{
            const needle = SEL.slice(5).trim();
            for (const el of document.querySelectorAll('body, body *')) {{
                if (isHidden(el)) continue;
                const txt = visText(el);
                if (txt && txt.includes(needle)) {{
                    let deeper = false;
                    for (const c of el.children) {{
                        if (!isHidden(c) && visText(c).includes(needle)) {{ deeper = true; break; }}
                    }}
                    if (!deeper) {{ target = el; break; }}
                }}
            }}
        }} else if (SEL.slice(0, 5) === 'role=') {{
            const m = SEL.slice(5).match(/^([a-zA-Z]+)(?:\[name=(?:"([^"]*)"|'([^']*)'|([^\]]*))\])?$/);
            if (m) {{
                const role = m[1].toLowerCase();
                const name = m[2] !== undefined ? m[2] : (m[3] !== undefined ? m[3] : m[4]);
                const map = {{
                    button: 'button,[role=button],input[type=button],input[type=submit],input[type=reset]',
                    link: 'a[href],[role=link]',
                    textbox: "input:not([type=button]):not([type=submit]):not([type=reset]):not([type=checkbox]):not([type=radio]):not([type=hidden]):not([type=file]):not([type=image]),textarea,[role=textbox]",
                    checkbox: 'input[type=checkbox],[role=checkbox]',
                    radio: 'input[type=radio],[role=radio]',
                    combobox: 'select,[role=combobox]',
                    heading: 'h1,h2,h3,h4,h5,h6,[role=heading]',
                    img: 'img,[role=img]',
                    list: 'ul,ol,[role=list]',
                    listitem: 'li,[role=listitem]'
                }};
                const q = map[role] || ('[role=' + role + ']');
                for (const el of document.querySelectorAll(q)) {{
                    if (name === undefined || name === null || name === '') {{ target = el; break; }}
                    if (accName(el).toLowerCase().includes(String(name).toLowerCase())) {{ target = el; break; }}
                }}
            }}
        }} else {{
            target = document.querySelector(SEL);
            return target ? SEL : null;
        }}
        return cssPath(target);
    }})()"#,
        sel = serde_json_string(selector),
    )
}

// -- markdown (readability) extraction ---------------------------------------------

/// Pick the best content root and walk it to Markdown, stripping chrome
/// (nav/aside/script/style/…). Pragmatic, not a full readability port: it
/// scores candidate blocks by text length and converts the common block/inline
/// elements. Returns a plain Markdown string (caller caps the length).
const MARKDOWN_JS: &str = r#"(() => {
    const strip = new Set(['SCRIPT','STYLE','NOSCRIPT','NAV','ASIDE','HEADER','FOOTER','FORM','TEMPLATE','SVG','IFRAME']);
    const textLen = (el) => (el.innerText || '').replace(/\s+/g, ' ').trim().length;
    // Prefer an explicit main/article; otherwise the densest block.
    let root = document.querySelector('article') || document.querySelector('main')
        || document.querySelector('[role=main]');
    if (!root) {
        let best = document.body, bestScore = textLen(document.body) * 0.4;
        const blocks = document.querySelectorAll('article, main, section, div');
        for (const b of blocks) {
            if (b.closest('nav, aside, header, footer')) continue;
            const s = textLen(b);
            if (s > bestScore) { best = b; bestScore = s; }
        }
        root = best;
    }
    if (!root) return '';
    const esc = (s) => s.replace(/([\\`*_\[\]])/g, '\\$1');
    const clean = (s) => (s || '').replace(/\s+/g, ' ');
    const out = [];
    const inline = (el) => {
        let s = '';
        for (const n of el.childNodes) {
            if (n.nodeType === 3) { s += clean(n.textContent); continue; }
            if (n.nodeType !== 1) continue;
            const t = n.tagName;
            if (strip.has(t)) continue;
            if (t === 'A') {
                const href = n.getAttribute('href') || '';
                const txt = clean(n.innerText) || '';
                s += href ? '[' + esc(txt) + '](' + href + ')' : esc(txt);
            } else if (t === 'STRONG' || t === 'B') {
                s += '**' + inline(n).trim() + '**';
            } else if (t === 'EM' || t === 'I') {
                s += '*' + inline(n).trim() + '*';
            } else if (t === 'CODE') {
                s += '`' + clean(n.innerText) + '`';
            } else if (t === 'BR') {
                s += ' ';
            } else if (t === 'IMG') {
                const alt = n.getAttribute('alt') || '';
                const src = n.getAttribute('src') || '';
                if (src) s += '![' + esc(alt) + '](' + src + ')';
            } else {
                s += inline(n);
            }
        }
        return s;
    };
    const walk = (el) => {
        for (const n of el.children) {
            const t = n.tagName;
            if (strip.has(t)) continue;
            if (/^H[1-6]$/.test(t)) {
                const level = Number(t[1]);
                const txt = clean(n.innerText).trim();
                if (txt) out.push('#'.repeat(level) + ' ' + txt);
            } else if (t === 'P') {
                const txt = inline(n).trim();
                if (txt) out.push(txt);
            } else if (t === 'UL' || t === 'OL') {
                let i = 1;
                for (const li of n.children) {
                    if (li.tagName !== 'LI') continue;
                    const bullet = t === 'OL' ? (i++) + '. ' : '- ';
                    const txt = inline(li).trim();
                    if (txt) out.push(bullet + txt);
                }
                out.push('');
            } else if (t === 'PRE') {
                const code = (n.innerText || '').replace(/\s+$/, '');
                if (code) out.push('```\n' + code + '\n```');
            } else if (t === 'BLOCKQUOTE') {
                const txt = clean(n.innerText).trim();
                if (txt) out.push(txt.split('\n').map(l => '> ' + l).join('\n'));
            } else if (t === 'TABLE') {
                const rows = Array.from(n.querySelectorAll('tr'));
                let first = true;
                for (const r of rows) {
                    const cells = Array.from(r.children).map(c => clean(c.innerText).trim());
                    if (!cells.length) continue;
                    out.push('| ' + cells.join(' | ') + ' |');
                    if (first) {
                        out.push('| ' + cells.map(() => '---').join(' | ') + ' |');
                        first = false;
                    }
                }
                out.push('');
            } else if (t === 'A' || t === 'SPAN' || t === 'STRONG' || t === 'EM' || t === 'CODE') {
                const txt = inline(n).trim();
                if (txt) out.push(txt);
            } else {
                walk(n);
            }
        }
    };
    walk(root);
    return out.join('\n\n').replace(/\n{3,}/g, '\n\n').trim();
})()"#;

// -- storage state (cookies + localStorage) ----------------------------------------

const DUMP_LOCALSTORAGE_JS: &str = r#"(() => {
    const o = {};
    try {
        for (let i = 0; i < localStorage.length; i++) {
            const k = localStorage.key(i);
            o[k] = localStorage.getItem(k);
        }
    } catch (e) {}
    return o;
})()"#;

async fn page_origin(page: &Page) -> Option<String> {
    page.evaluate("location.origin")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .filter(|o| o != "null" && !o.is_empty())
}

async fn apply_localstorage(page: &Page, entries: &HashMap<String, String>) -> Result<()> {
    let json = serde_json::to_string(entries).unwrap_or_else(|_| "{}".into());
    let js = format!(
        r#"(() => {{
            const e = {json};
            try {{ for (const k in e) localStorage.setItem(k, e[k]); return true; }}
            catch (err) {{ return false; }}
        }})()"#,
    );
    page.evaluate(js)
        .await
        .context("failed to write localStorage")?;
    Ok(())
}

/// Raw CDP command with a JSON params body and a loose JSON response. Used for
/// cookie get/set to sidestep the strict enum deserialization in the typed
/// chromiumoxide bindings (same rationale as [`RawGetFullAxTree`]).
struct RawCommand {
    id: &'static str,
    params: serde_json::Value,
}

impl serde::Serialize for RawCommand {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.params.serialize(s)
    }
}

impl chromiumoxide::types::Method for RawCommand {
    fn identifier(&self) -> chromiumoxide::types::MethodId {
        self.id.into()
    }
}

impl chromiumoxide::types::Command for RawCommand {
    type Response = serde_json::Value;
}

async fn get_all_cookies(page: &Page) -> Result<Vec<serde_json::Value>> {
    let resp = page
        .execute(RawCommand {
            id: "Network.getAllCookies",
            params: serde_json::json!({}),
        })
        .await
        .context("Network.getAllCookies failed")?;
    Ok(resp
        .result
        .get("cookies")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default())
}

async fn set_cookies(page: &Page, cookies: &[serde_json::Value]) -> Result<()> {
    if cookies.is_empty() {
        return Ok(());
    }
    // Map saved cookies (as returned by getAllCookies) to Network.setCookies
    // params, keeping only fields the setter accepts.
    let params: Vec<serde_json::Value> = cookies
        .iter()
        .filter_map(|c| {
            let name = c.get("name")?.as_str()?;
            let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
            let mut p = serde_json::Map::new();
            p.insert("name".into(), name.into());
            p.insert("value".into(), value.into());
            if let Some(d) = c.get("domain").and_then(|v| v.as_str()) {
                p.insert("domain".into(), d.into());
            }
            if let Some(path) = c.get("path").and_then(|v| v.as_str()) {
                p.insert("path".into(), path.into());
            }
            if let Some(s) = c.get("secure").and_then(|v| v.as_bool()) {
                p.insert("secure".into(), s.into());
            }
            if let Some(h) = c.get("httpOnly").and_then(|v| v.as_bool()) {
                p.insert("httpOnly".into(), h.into());
            }
            if let Some(ss) = c.get("sameSite").and_then(|v| v.as_str()) {
                p.insert("sameSite".into(), ss.into());
            }
            // Persist expiry only for non-session cookies with a real expiry.
            let is_session = c.get("session").and_then(|v| v.as_bool()).unwrap_or(false);
            if !is_session {
                if let Some(exp) = c.get("expires").and_then(|v| v.as_f64()) {
                    if exp > 0.0 {
                        p.insert("expires".into(), serde_json::json!(exp));
                    }
                }
            }
            Some(serde_json::Value::Object(p))
        })
        .collect();
    page.execute(RawCommand {
        id: "Network.setCookies",
        params: serde_json::json!({ "cookies": params }),
    })
    .await
    .context("Network.setCookies failed")?;
    Ok(())
}

// -- accessibility snapshot -------------------------------------------------------

/// Raw `Accessibility.getFullAXTree`. The typed response in chromiumoxide_cdp
/// 0.7 rejects AX enum values added by newer Chrome (e.g. the "uninteresting"
/// ignored-reason), so we deserialize into loose JSON and read fields
/// defensively instead.
#[derive(Debug, Clone, Default, serde::Serialize)]
struct RawGetFullAxTree {}

impl chromiumoxide::types::Method for RawGetFullAxTree {
    fn identifier(&self) -> chromiumoxide::types::MethodId {
        "Accessibility.getFullAXTree".into()
    }
}

impl chromiumoxide::types::Command for RawGetFullAxTree {
    type Response = serde_json::Value;
}

async fn ax_snapshot(page: &Page) -> Result<String> {
    // Enable is best-effort: getFullAXTree works one-shot on current Chrome,
    // but older builds want the domain enabled first.
    let _ = page.execute(ax::EnableParams::default()).await;
    let resp = page
        .execute(RawGetFullAxTree::default())
        .await
        .context("Accessibility.getFullAXTree failed")?;
    let nodes = resp
        .result
        .get("nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();
    if nodes.is_empty() {
        bail!("empty accessibility tree");
    }
    let by_id: HashMap<&str, &serde_json::Value> = nodes
        .iter()
        .filter_map(|n| n.get("nodeId").and_then(|id| id.as_str()).map(|id| (id, n)))
        .collect();
    let root = nodes
        .iter()
        .find(|n| n.get("parentId").is_none())
        .unwrap_or(&nodes[0]);
    let mut out = String::new();
    let mut truncated = false;
    render_ax_node(root, &by_id, 0, &mut out, &mut truncated);
    if truncated {
        out.push_str(&format!(
            "\n[... snapshot truncated at {MAX_SNAPSHOT_CHARS} chars ...]"
        ));
    }
    Ok(out)
}

fn render_ax_node(
    node: &serde_json::Value,
    by_id: &HashMap<&str, &serde_json::Value>,
    depth: usize,
    out: &mut String,
    truncated: &mut bool,
) {
    if *truncated {
        return;
    }
    // Bound recursion independently of the output cap: skipped structural nodes
    // produce no text yet still recurse, so a pathologically deep AX tree could
    // overflow the stack even while under MAX_SNAPSHOT_CHARS.
    if depth > MAX_AX_DEPTH {
        return;
    }
    if out.len() >= MAX_SNAPSHOT_CHARS {
        *truncated = true;
        return;
    }
    let ignored = node
        .get("ignored")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let role = ax_value_str(node.get("role"));
    let name = ax_value_str(node.get("name"));
    // Structural noise (ignored/generic/unnamed wrappers) is skipped but its
    // children are lifted to the current depth, keeping the outline compact.
    let skip = ignored
        || ((role.is_empty()
            || matches!(
                role.as_str(),
                "generic" | "none" | "InlineTextBox" | "LineBreak"
            ))
            && name.is_empty());
    let mut child_depth = depth;
    if !skip {
        let display_role = match role.as_str() {
            "RootWebArea" => "document",
            "StaticText" => "text",
            other => other,
        };
        out.push_str(&"  ".repeat(depth));
        out.push_str(display_role);
        if !name.is_empty() {
            out.push_str(&format!(" {}", serde_json_string(&name)));
        }
        let props = ax_props(node);
        if !props.is_empty() {
            out.push_str(&format!(" [{props}]"));
        }
        out.push('\n');
        child_depth = depth + 1;
    }
    for child_id in node
        .get("childIds")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str())
    {
        if let Some(child) = by_id.get(child_id) {
            render_ax_node(child, by_id, child_depth, out, truncated);
        }
    }
}

/// Extract the computed value of an `AXValue` object ({"type": .., "value": ..}).
fn ax_value_str(v: Option<&serde_json::Value>) -> String {
    v.and_then(|v| v.get("value"))
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn ax_props(node: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    for p in node
        .get("properties")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let Some(name) = p.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if !matches!(
            name,
            "focused"
                | "checked"
                | "disabled"
                | "expanded"
                | "selected"
                | "pressed"
                | "required"
                | "readonly"
                | "level"
        ) {
            continue;
        }
        match p.get("value").and_then(|v| v.get("value")) {
            Some(serde_json::Value::Bool(true)) => parts.push(name.to_string()),
            Some(serde_json::Value::Bool(false)) | None => {}
            Some(v) => parts.push(format!("{name}={v}")),
        }
    }
    parts.join(", ")
}

/// DOM-derived outline (interactive elements + headings + landmarks with
/// selectors) used when the accessibility tree is unavailable.
async fn dom_outline(page: &Page) -> Result<String> {
    let js = r#"(() => {
        const out = [];
        const sels = 'h1,h2,h3,h4,a[href],button,input,select,textarea,[role],main,nav,header,footer,form,label';
        for (const el of document.querySelectorAll(sels)) {
            if (out.length >= 400) break;
            const tag = el.tagName.toLowerCase();
            const id = el.id ? '#' + el.id : '';
            const label = (el.getAttribute('aria-label') || el.innerText || el.value || '')
                .trim().replace(/\s+/g, ' ').slice(0, 80);
            out.push(`${tag}${id} "${label}"`);
        }
        return out.join('\n');
    })()"#;
    let text: String = page
        .evaluate(js)
        .await
        .context("DOM outline evaluation failed")?
        .into_value()
        .unwrap_or_default();
    if text.is_empty() {
        Ok("(empty page)".to_string())
    } else {
        Ok(cap(text, MAX_SNAPSHOT_CHARS))
    }
}

fn cap(s: String, max: usize) -> String {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n[... truncated ...]", &s[..end])
    }
}

fn serde_json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Guard navigation against schemes that read local files or reach non-web
/// resources. http/https/about/data/blob pass; `file:` is allowed only when
/// `KITE_ALLOW_FILE_URLS` is set (off by default, so an agent can't exfiltrate
/// local files like `file:///etc/passwd`); anything else (javascript:, chrome:,
/// view-source:, …) is rejected. A URL with no parseable scheme (bare host /
/// relative) is left for the browser to resolve.
fn validate_navigation_url(url: &str) -> Result<()> {
    let scheme = match url.split_once(':') {
        Some((s, _))
            if !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            s.to_ascii_lowercase()
        }
        _ => return Ok(()),
    };
    match scheme.as_str() {
        "http" | "https" | "about" | "data" | "blob" => Ok(()),
        "file" => {
            if std::env::var("KITE_ALLOW_FILE_URLS").is_ok() {
                Ok(())
            } else {
                bail!(
                    "file:// navigation is disabled by default; set KITE_ALLOW_FILE_URLS=1 to allow local-file access"
                )
            }
        }
        other => bail!("navigation to {other}: URLs is not allowed"),
    }
}

/// Per-poll CDP evaluation budget for [`BrowserSession::wait_for`] /
/// [`BrowserSession::assert`]: the time still left before the deadline, capped
/// at [`WAIT_FOR_POLL_BUDGET`] and floored at [`WAIT_FOR_MIN_BUDGET`] so a
/// single stuck evaluation is abandoned and retried rather than consuming the
/// whole wait.
fn poll_budget(timeout: Duration, started: Instant) -> Duration {
    timeout
        .saturating_sub(started.elapsed())
        .min(WAIT_FOR_POLL_BUDGET)
        .max(WAIT_FOR_MIN_BUDGET)
}

/// Evaluate a boolean poll condition once, bounded by `budget`. A timeout or CDP
/// error is returned as `Err` so the caller can treat it as "not yet satisfied"
/// and keep polling, instead of aborting the whole wait on one transient hiccup.
async fn poll_condition(page: &Page, js: &str, budget: Duration) -> Result<bool> {
    let value = tokio::time::timeout(budget, page.evaluate(js.to_string()))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "condition evaluation exceeded {}ms poll budget",
                budget.as_millis()
            )
        })?
        .context("condition evaluation failed")?;
    Ok(value.into_value().unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_respects_char_boundaries() {
        let s = "héllo wörld".repeat(100);
        let capped = cap(s.clone(), 50);
        assert!(capped.len() < s.len());
        assert!(capped.ends_with("[... truncated ...]"));
        // must not panic on multi-byte boundaries
        let _ = cap("日本語テスト".to_string(), 4);
    }

    #[test]
    fn cap_leaves_short_strings_alone() {
        assert_eq!(cap("short".to_string(), 100), "short");
    }

    #[test]
    fn selector_is_json_escaped() {
        assert_eq!(serde_json_string(r#"a[href="x"]"#), r#""a[href=\"x\"]""#);
    }

    #[test]
    fn css_len_to_inches_converts_units() {
        // px → in (÷96), the puppeteer default unit.
        assert!((css_len_to_inches(Some("96px")).unwrap() - 1.0).abs() < 1e-9);
        assert!((css_len_to_inches(Some("35px")).unwrap() - 35.0 / 96.0).abs() < 1e-9);
        // mm/cm/in/pt.
        assert!((css_len_to_inches(Some("25.4mm")).unwrap() - 1.0).abs() < 1e-9);
        assert!((css_len_to_inches(Some("2.54cm")).unwrap() - 1.0).abs() < 1e-9);
        assert!((css_len_to_inches(Some("0.5in")).unwrap() - 0.5).abs() < 1e-9);
        assert!((css_len_to_inches(Some("72pt")).unwrap() - 1.0).abs() < 1e-9);
        // Bare number → px. Whitespace tolerated.
        assert!((css_len_to_inches(Some(" 48 ")).unwrap() - 0.5).abs() < 1e-9);
        // Empty / None / garbage → None so the caller defaults to 0.
        assert_eq!(css_len_to_inches(None), None);
        assert_eq!(css_len_to_inches(Some("")), None);
        assert_eq!(css_len_to_inches(Some("auto")), None);
    }

    #[test]
    fn cache_root_prefers_kite_cache_dir() {
        let root = cache_root(
            Some(PathBuf::from("/custom/cache")),
            Some(PathBuf::from("/os/cache")),
        );
        assert_eq!(
            root,
            PathBuf::from("/custom/cache/kitewright/chrome-headless-shell")
        );
        // Empty KITE_CACHE_DIR falls through to the platform cache dir.
        let root = cache_root(Some(PathBuf::from("")), Some(PathBuf::from("/os/cache")));
        assert_eq!(
            root,
            PathBuf::from("/os/cache/kitewright/chrome-headless-shell")
        );
    }

    #[test]
    fn version_key_orders_numerically() {
        // Lexical sort would put "120" before "99"; numeric must not.
        assert!(version_key("120.0.0.0") > version_key("99.0.4844.51"));
        assert!(version_key("120.0.6099.109") > version_key("120.0.6099.99"));
        assert_eq!(version_key("1.2.3"), vec![1, 2, 3]);
    }

    #[test]
    fn first_existing_skips_missing_returns_present() {
        let marker = std::env::temp_dir().join("kite-detect-existence-marker");
        std::fs::write(&marker, b"x").unwrap();
        let present = marker.to_str().unwrap();
        // A bogus path first (like the orphaned /Applications/Chromium.app) must
        // be skipped in favor of the one that actually exists.
        assert_eq!(
            first_existing(&["/no/such/kite/browser/path", present]),
            Some(marker.clone())
        );
        assert_eq!(first_existing(&["/no/such/a", "/no/such/b"]), None);
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn shell_wrapper_shims_are_rejected() {
        // The exact shim chromiumoxide detection returns on macOS when the
        // Homebrew Chromium cask is installed but its app was removed.
        assert!(is_shell_wrapper(std::path::Path::new(
            "/opt/homebrew/Caskroom/chromium/latest/chromium.wrapper.sh"
        )));
        assert!(is_shell_wrapper(std::path::Path::new(
            "/some/where/launch.sh"
        )));
        // Real browser executables must not be flagged.
        assert!(!is_shell_wrapper(std::path::Path::new(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        )));
        assert!(!is_shell_wrapper(std::path::Path::new("/usr/bin/chromium")));
    }

    #[test]
    fn find_installed_browser_picks_newest_version() {
        let tmp = std::env::temp_dir().join(format!("kw-install-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let bin = headless_shell_binary_name();
        // Two versions, binary nested under a platform folder in the newer one.
        let older = tmp.join("99.0.4844.51");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::write(older.join(bin), b"old").unwrap();
        let newer = tmp.join("120.0.6099.109").join("chrome-headless-shell-x64");
        std::fs::create_dir_all(&newer).unwrap();
        std::fs::write(newer.join(bin), b"new").unwrap();

        let found = find_installed_browser_in(&tmp).expect("should find a binary");
        assert!(found.starts_with(tmp.join("120.0.6099.109")));
        assert_eq!(found.file_name().unwrap().to_str().unwrap(), bin);

        // Empty / missing dir → None.
        assert!(find_installed_browser_in(&tmp.join("nope")).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
