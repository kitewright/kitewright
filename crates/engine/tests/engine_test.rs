//! Integration tests for the browser engine, run against a local HTML fixture
//! server so they are hermetic (no external network).
//!
//! Requires a Chromium-based browser: set BROWSER_EXECUTABLE, or have Chrome /
//! Chromium at a common path. Tests are skipped (with a notice) if none found.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kitewright_engine::{Engine, EngineConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 1×1 transparent PNG served by the tracking fixture at any `*.png` path.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

const TRACK_HTML: &str = r#"<!doctype html><html><head><title>Tracked</title></head>
<body><h1>tracked page</h1>
<img src="/tracked-image.png" width="10" height="10">
</body></html>"#;

/// Like [`start_fixture_server`], but records the set of request paths the
/// browser actually fetched (so a test can assert a sub-resource was / was not
/// requested) and serves a tiny PNG at `/tracked-image.png`.
async fn start_tracking_fixture_server() -> (String, Arc<Mutex<HashSet<String>>>) {
    let hits: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits_srv = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let hits_conn = Arc::clone(&hits_srv);
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                if let Ok(mut h) = hits_conn.lock() {
                    h.insert(path.clone());
                }
                let resp: Vec<u8> = if path.ends_with(".png") {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        TINY_PNG.len(),
                    );
                    [header.as_bytes(), TINY_PNG].concat()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        TRACK_HTML.len(),
                        TRACK_HTML,
                    )
                    .into_bytes()
                };
                let _ = sock.write_all(&resp).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/"), hits)
}

const FIXTURE_HTML: &str = r#"<!doctype html>
<html>
<head><title>Fixture Page</title></head>
<body>
  <h1 id="heading">Hello from fixture</h1>
  <ul>
    <li class="item">alpha</li>
    <li class="item">beta</li>
    <li class="item">gamma</li>
  </ul>
  <a href="/link-target" id="link">a link</a>
  <label for="name-input">Your name</label>
  <input id="name-input" type="text"
         oninput="document.getElementById('mirror').textContent = this.value"
         onkeydown="if (event.key === 'Enter') document.getElementById('enter-flag').textContent = 'enter-pressed'">
  <div id="mirror"></div>
  <div id="enter-flag"></div>
  <button id="reveal-btn" onclick="document.getElementById('secret').style.display = 'block'">Reveal secret</button>
  <div id="secret" style="display:none">secret-revealed-text</div>
  <div id="key-log"></div>

  <!-- select_option -->
  <select id="fruit" onchange="document.getElementById('fruit-out').textContent = this.value">
    <option value="a">Apple</option>
    <option value="b">Banana</option>
    <option value="c">Cherry</option>
  </select>
  <div id="fruit-out"></div>

  <!-- fill_form -->
  <input id="field-user" type="text">
  <input id="field-pass" type="text">
  <button id="submit-btn" onclick="document.getElementById('form-out').textContent = document.getElementById('field-user').value + ':' + document.getElementById('field-pass').value">Submit</button>
  <div id="form-out"></div>

  <!-- role / text selectors -->
  <button id="role-target" aria-label="Special Action" onclick="document.getElementById('role-out').textContent='role-clicked'">x</button>
  <div id="role-out"></div>
  <a href='#' id="text-target" onclick="document.getElementById('text-out').textContent='text-clicked';return false;">Unique Link Text</a>
  <div id="text-out"></div>

  <!-- storage state: set writes a cookie + localStorage; read copies them out -->
  <button id="set-state-btn" onclick="document.cookie='kw_session=sess-A-value; path=/'; localStorage.setItem('kw_ls','ls-A-value'); document.getElementById('state-set').textContent='state-set';">Set State</button>
  <div id="state-set"></div>
  <button id="read-state-btn" onclick="document.getElementById('cookie-out').textContent=document.cookie; document.getElementById('ls-out').textContent=localStorage.getItem('kw_ls')||'';">Read State</button>
  <div id="cookie-out"></div>
  <div id="ls-out"></div>

  <!-- dialog -->
  <button id="confirm-btn" onclick="document.getElementById('confirm-out').textContent = confirm('sure?') ? 'accepted' : 'dismissed'">Confirm</button>
  <div id="confirm-out"></div>

  <!-- hover menu (CSS :hover) -->
  <style>
    #hover-menu { display: none; }
    #hover-trigger:hover + #hover-menu { display: block; }
  </style>
  <div id="hover-trigger">Hover me</div>
  <div id="hover-menu">menu-revealed</div>

  <!-- actionability: disabled, hidden, covered, and a plain enabled button -->
  <button id="act-ok" onclick="document.getElementById('act-out').textContent='act-ok-clicked'">OK Button</button>
  <div id="act-out"></div>
  <button id="act-disabled" disabled>Disabled Button</button>
  <button id="act-hidden" style="display:none">Hidden Button</button>
  <div id="act-cover-wrap" style="position:relative; width:120px; height:40px">
    <button id="act-covered" style="position:absolute; left:0; top:0; width:120px; height:40px">Covered</button>
    <div id="act-overlay" style="position:absolute; left:0; top:0; width:120px; height:40px; background:red"></div>
  </div>

  <!-- markdown (readability) content root -->
  <article id="md-root">
    <h1>Doc Title</h1>
    <p>An intro paragraph with a <a href="https://example.com/x">sample link</a> inside.</p>
    <h2>Features</h2>
    <ul>
      <li>first feature</li>
      <li>second feature</li>
    </ul>
    <pre><code>let x = 1;</code></pre>
  </article>

  <!-- console + network triggers -->
  <button id="log-btn" onclick="console.log('hello-console-42')">Log</button>
  <button id="fetch-btn" onclick="fetch('/api/ping').catch(()=>{})">Fetch</button>

  <script>
    document.addEventListener('keydown', (e) => {
      document.getElementById('key-log').textContent = 'last-key:' + e.key;
    });
  </script>
</body>
</html>"#;

fn chrome_path() -> Option<String> {
    // The real-browser tests are gated out of the blocking CI job (where the
    // shared 2-core runner starves Chromium and they flake); returning None here
    // makes every browser test self-skip via its `chrome_path().is_none()`
    // guard. They still run locally by default and in the non-blocking
    // `browser` CI job (which does not set this flag).
    if std::env::var("KITE_SKIP_BROWSER_E2E").is_ok() {
        return None;
    }
    if let Ok(p) = std::env::var("BROWSER_EXECUTABLE") {
        return Some(p);
    }
    for p in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/opt/google/chrome/chrome",
    ] {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

/// Minimal HTTP/1.1 server serving the fixture HTML on an ephemeral port.
async fn start_fixture_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    FIXTURE_HTML.len(),
                    FIXTURE_HTML,
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}/")
}

fn test_engine(idle_ttl: Duration) -> Engine {
    test_engine_cfg(idle_ttl, 1)
}

/// Build a test engine with an explicit warm-context-pool size. Each test gets
/// its own unique cache dir so a shared on-disk cache can never leak state
/// between tests (and so parallel test browsers don't fight over the dir).
fn test_engine_cfg(idle_ttl: Duration, context_pool_size: usize) -> Engine {
    let unique = format!(
        "kitewright-test-cache-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    Engine::new(EngineConfig {
        idle_ttl,
        nav_timeout: Duration::from_secs(20),
        no_sandbox: true,
        headful: false,
        executable: chrome_path(),
        context_pool_size,
        cache_dir: std::env::temp_dir().join(unique),
        prewarm_url: None,
        viewport: (1440, 900),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn navigate_extract_screenshot_and_reaper() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(3));

    // --- navigate: title + visible text ---
    let info = engine.navigate(&url).await.expect("navigate failed");
    assert_eq!(info.title, "Fixture Page");
    assert!(
        info.text.contains("Hello from fixture"),
        "text was: {}",
        info.text
    );
    assert!(info.text.contains("beta"));

    // --- extract: text values via CSS selector ---
    let items = engine
        .extract(&url, ".item", None)
        .await
        .expect("extract failed");
    assert_eq!(items, vec!["alpha", "beta", "gamma"]);

    // --- extract: attribute values ---
    let hrefs = engine
        .extract(&url, "a#link", Some("href"))
        .await
        .expect("extract attr failed");
    assert_eq!(hrefs, vec!["/link-target"]);

    // --- extract: no matches is empty, not an error ---
    let none = engine
        .extract(&url, ".does-not-exist", None)
        .await
        .expect("empty extract failed");
    assert!(none.is_empty());

    // --- screenshot: valid PNG magic bytes ---
    let png = engine
        .screenshot(&url, false)
        .await
        .expect("screenshot failed");
    assert!(png.len() > 1000, "png too small: {} bytes", png.len());
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    // --- browser is alive after use, then reaped after idle TTL ---
    assert!(
        engine.is_running().await,
        "browser should be running after use"
    );
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(
        !engine.is_running().await,
        "idle reaper should have closed the browser"
    );

    // --- relaunch after reap works ---
    let info2 = engine
        .navigate(&url)
        .await
        .expect("relaunch navigate failed");
    assert_eq!(info2.title, "Fixture Page");

    engine.shutdown().await;
    assert!(!engine.is_running().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_page_persists_and_supports_interaction() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();

    // --- navigate, then extract WITHOUT url: must hit the same page ---
    let info = session.navigate(&url).await.expect("navigate failed");
    assert_eq!(info.title, "Fixture Page");
    let items = session
        .extract(None, ".item", None)
        .await
        .expect("session extract failed");
    assert_eq!(items, vec!["alpha", "beta", "gamma"]);

    // --- type into an input; the page mirrors the value into a div, which
    //     verifies real key events fired AND the page persisted ---
    session
        .type_text("#name-input", "hello agent", false, false, None)
        .await
        .expect("type failed");
    let mirror = session.extract(None, "#mirror", None).await.unwrap();
    assert_eq!(mirror, vec!["hello agent"]);

    // --- clear + retype + press Enter ---
    session
        .type_text("#name-input", "second", true, true, None)
        .await
        .expect("clear+type failed");
    let mirror = session.extract(None, "#mirror", None).await.unwrap();
    assert_eq!(mirror, vec!["second"], "clear=true must replace the value");
    session
        .wait_for(None, Some("enter-pressed"), Some(3_000))
        .await
        .expect("press_enter did not reach the page");

    // --- click reveals a hidden div; wait_for observes it ---
    session
        .click("#reveal-btn", None)
        .await
        .expect("click failed");
    let elapsed = session
        .wait_for(Some("#secret"), Some("secret-revealed-text"), Some(5_000))
        .await
        .expect("wait_for after click failed");
    assert!(elapsed <= 5_000);
    let secret = session.extract(None, "#secret", None).await.unwrap();
    assert_eq!(secret, vec!["secret-revealed-text"]);

    // --- page-level key press reaches the document listener ---
    session.press_key("Escape").await.expect("press_key failed");
    session
        // Generous timeout: on a heavily-loaded CI runner the keypress →
        // document listener → DOM update → CDP read-back round trip can be slow.
        .wait_for(None, Some("last-key:Escape"), Some(15_000))
        .await
        .expect("Escape key did not reach the page");

    // --- unknown key and missing element produce clear errors ---
    let e = session.press_key("NotAKey").await.unwrap_err();
    assert!(format!("{e:#}").contains("NotAKey"));
    let e = session
        .click("#does-not-exist", Some(500))
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("#does-not-exist"));

    // --- wait_for timeout is an error that reports the condition ---
    let e = session
        .wait_for(Some("#never-appears"), None, Some(400))
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("timed out"));

    // --- snapshot: roles and names of the fixture elements ---
    let snap = session.snapshot().await.expect("snapshot failed");
    assert!(
        snap.contains("heading") && snap.contains("Hello from fixture"),
        "snapshot missing heading: {snap}"
    );
    assert!(
        snap.contains("button") && snap.contains("Reveal secret"),
        "snapshot missing button: {snap}"
    );
    assert!(
        snap.contains("textbox") && snap.contains("Your name"),
        "snapshot missing labelled textbox: {snap}"
    );

    // --- a second session gets its own page: navigating it must not move
    //     the first session's page ---
    let session2 = engine.create_session();
    session2
        .navigate("data:text/html,<title>Other</title><body>other-page</body>")
        .await
        .expect("second session navigate failed");
    let still_there = session.extract(None, "#heading", None).await.unwrap();
    assert_eq!(still_there, vec!["Hello from fixture"]);

    session2.close().await;
    session.close().await;
    engine.shutdown().await;
    assert!(!engine.is_running().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_recovers_after_idle_reap() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(2));
    let session = engine.create_session();
    session.navigate(&url).await.expect("navigate failed");

    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        !engine.is_running().await,
        "idle reaper should have closed the browser"
    );

    // Non-navigating op after the reap: polite error telling the agent that
    // session state was lost.
    let e = session.extract(None, ".item", None).await.unwrap_err();
    assert!(
        format!("{e:#}").contains("browser_navigate"),
        "unexpected error: {e:#}"
    );

    // Navigating recovers the session with a fresh page.
    let info = session
        .navigate(&url)
        .await
        .expect("session did not recover after reap");
    assert_eq!(info.title, "Fixture Page");
    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn navigate_bad_port_returns_error_page_or_error() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let engine = test_engine(Duration::from_secs(30));
    // Connection-refused: must not hang or panic. What comes back varies by
    // browser build — full Chrome renders an error page with text, the
    // headless shell returns an empty chrome-error:// page, and some builds
    // surface a navigation error. All are acceptable; hanging past the
    // nav_timeout or panicking is not.
    let result = tokio::time::timeout(
        Duration::from_secs(25),
        engine.navigate("http://127.0.0.1:9/"),
    )
    .await
    .expect("navigate must resolve within the timeout");
    if let Err(e) = result {
        assert!(!e.to_string().is_empty());
    }
    // Engine must remain usable after the failed navigation.
    let url = start_fixture_server().await;
    let info = engine
        .navigate(&url)
        .await
        .expect("engine unusable after bad navigation");
    assert_eq!(info.title, "Fixture Page");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn select_option_and_fill_form_and_selectors() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();
    session.navigate(&url).await.expect("navigate failed");

    // --- select_option by value: changes value + fires change ---
    let selected = session
        .select_option("#fruit", Some("b"), None, None)
        .await
        .expect("select by value failed");
    assert_eq!(selected, "b");
    let out = session.extract(None, "#fruit-out", None).await.unwrap();
    assert_eq!(out, vec!["b"], "change event did not fire");

    // --- select_option by label ---
    let selected = session
        .select_option("#fruit", None, Some("Cherry"), None)
        .await
        .expect("select by label failed");
    assert_eq!(selected, "c");

    // --- select_option: no match is an error ---
    let e = session
        .select_option("#fruit", Some("nope"), None, None)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("no matching"));

    // --- fill_form: multiple inputs in one call ---
    let outcomes = session
        .fill_form(
            &[
                ("#field-user".into(), "alice".into()),
                ("#field-pass".into(), "secret".into()),
            ],
            None,
        )
        .await
        .expect("fill_form failed");
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|o| o.ok), "fields: {outcomes:?}");
    session.click("#submit-btn", None).await.unwrap();
    let form_out = session.extract(None, "#form-out", None).await.unwrap();
    assert_eq!(form_out, vec!["alice:secret"]);

    // --- fill_form reports per-field failure without aborting ---
    let outcomes = session
        .fill_form(
            &[
                ("#field-user".into(), "bob".into()),
                ("#does-not-exist".into(), "x".into()),
            ],
            Some(500),
        )
        .await
        .expect("fill_form failed");
    assert!(outcomes[0].ok);
    assert!(!outcomes[1].ok && outcomes[1].error.is_some());

    // --- role= selector clicks the element with the matching accessible name ---
    session
        .click("role=button[name=\"Special Action\"]", None)
        .await
        .expect("role selector click failed");
    let role_out = session.extract(None, "#role-out", None).await.unwrap();
    assert_eq!(role_out, vec!["role-clicked"]);

    // --- text= selector clicks the element whose text contains it ---
    session
        .click("text=Unique Link Text", None)
        .await
        .expect("text selector click failed");
    let text_out = session.extract(None, "#text-out", None).await.unwrap();
    assert_eq!(text_out, vec!["text-clicked"]);

    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn assert_present_absent_and_timeout() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();
    session.navigate(&url).await.expect("navigate failed");

    // --- passes for a present selector ---
    let a = session
        .assert(Some("#heading"), None, true, Some(2_000))
        .await
        .unwrap();
    assert!(a.passed && a.found, "present assert: {a:?}");

    // --- passes for absence (should_exist = false) of a missing selector ---
    let a = session
        .assert(Some("#never-there"), None, false, Some(2_000))
        .await
        .unwrap();
    assert!(a.passed && !a.found, "absent assert: {a:?}");

    // --- fails (not errors) when a required selector never appears ---
    let a = session
        .assert(Some("#never-there"), None, true, Some(400))
        .await
        .unwrap();
    assert!(!a.passed, "timeout assert should fail: {a:?}");
    assert!(a.elapsed_ms >= 400);

    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn save_and_restore_state_across_sessions() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));

    // --- session A: set a cookie + localStorage key, then save state ---
    let a = engine.create_session();
    a.navigate(&url).await.expect("A navigate failed");
    a.click("#set-state-btn", None)
        .await
        .expect("set state failed");
    a.wait_for(None, Some("state-set"), Some(2_000))
        .await
        .expect("state was not set");
    let state = a.save_state().await.expect("save_state failed");
    assert!(
        state.contains("kw_session") && state.contains("kw_ls"),
        "saved state missing keys: {state}"
    );
    a.close().await;

    // --- session B (different browser context): restore, navigate, read back ---
    let b = engine.create_session();
    b.restore_state(&state).await.expect("restore_state failed");
    b.navigate(&url).await.expect("B navigate failed");
    b.click("#read-state-btn", None)
        .await
        .expect("read state failed");

    let cookie = b.extract(None, "#cookie-out", None).await.unwrap();
    assert!(
        cookie
            .first()
            .map(|c| c.contains("sess-A-value"))
            .unwrap_or(false),
        "cookie not restored across sessions: {cookie:?}"
    );
    let ls = b.extract(None, "#ls-out", None).await.unwrap();
    assert_eq!(
        ls,
        vec!["ls-A-value"],
        "localStorage not restored across sessions"
    );

    b.close().await;
    engine.shutdown().await;
}

/// An authenticated session must survive an idle reap: cookies captured after
/// navigation are auto-restored onto the relaunched browser, so the agent stays
/// logged in across a pause that reaps the browser. This is the #1 gap vs
/// Playwright (which keeps the browser alive) — verified end-to-end here.
#[tokio::test(flavor = "multi_thread")]
async fn auth_survives_idle_reap_via_auto_restore() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    // idle_ttl = 2s so the reaper kills the idle browser during our sleep below.
    let engine = test_engine(Duration::from_secs(2));
    let session = engine.create_session();

    // Establish an authenticated session: set a cookie + localStorage, then
    // re-navigate so the post-navigation auto-capture snapshots the cookie.
    session.navigate(&url).await.expect("navigate failed");
    session
        .click("#set-state-btn", None)
        .await
        .expect("set state failed");
    session
        .wait_for(None, Some("state-set"), Some(2_000))
        .await
        .expect("state was not set");
    session
        .navigate(&url)
        .await
        .expect("re-navigate (to capture cookie) failed");

    // Let the idle reaper kill the browser (ttl 2s, reaper period 2s).
    tokio::time::sleep(Duration::from_secs(5)).await;

    // The next navigation relaunches the browser; auto-restore must replay the
    // cookie so we are still authenticated.
    session
        .navigate(&url)
        .await
        .expect("navigate after reap failed");
    session
        .click("#read-state-btn", None)
        .await
        .expect("read state failed");
    let cookie = session.extract(None, "#cookie-out", None).await.unwrap();
    assert!(
        cookie
            .first()
            .map(|c| c.contains("sess-A-value"))
            .unwrap_or(false),
        "auth cookie did NOT survive the idle reap (auto-restore failed): {cookie:?}"
    );

    session.close().await;
    engine.shutdown().await;
}

/// browser_fill_secret must resolve an `env:` reference server-side and type the
/// resolved value into the field — the plaintext only ever lives in the server,
/// never in the tool call. A bare (schemeless) reference is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn fill_secret_resolves_env_and_types_it() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    std::env::set_var("KW_FILL_SECRET_TEST", "hunter2-secret");
    let engine = test_engine(Duration::from_secs(60));
    let s = engine.create_session();
    s.set_content("<input id=pw>", None)
        .await
        .expect("set_content failed");
    s.fill_secret("#pw", "env:KW_FILL_SECRET_TEST", false, None)
        .await
        .expect("fill_secret failed");
    let value = s
        .evaluate("document.getElementById('pw').value")
        .await
        .expect("evaluate failed");
    assert_eq!(
        value,
        serde_json::json!("hunter2-secret"),
        "secret not typed"
    );
    // A schemeless (plaintext) reference is refused.
    assert!(
        s.fill_secret("#pw", "not-a-reference", false, None)
            .await
            .is_err(),
        "plaintext secret ref should be rejected"
    );
    // file: secrets are OFF by default (KITE_ALLOW_SECRET_FILES unset) — no
    // arbitrary host-file reads without an explicit opt-in.
    assert!(
        s.fill_secret("#pw", "file:/etc/hostname", false, None)
            .await
            .is_err(),
        "file: secret must be refused when KITE_ALLOW_SECRET_FILES is unset"
    );
    s.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pdf_returns_valid_pdf_bytes() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();

    // PDF of a freshly navigated page (url form) must start with the %PDF magic.
    let bytes = session
        .pdf(Some(&url), kitewright_engine::PdfOptions::default())
        .await
        .expect("pdf failed");
    assert!(bytes.len() > 500, "pdf too small: {} bytes", bytes.len());
    assert_eq!(
        &bytes[..5],
        b"%PDF-",
        "not a PDF: {:?}",
        &bytes[..8.min(bytes.len())]
    );

    // PDF of the current page (no url) with options set.
    let landscape = session
        .pdf(
            None,
            kitewright_engine::PdfOptions {
                format: Some("Letter".into()),
                landscape: true,
                print_background: true,
                ..Default::default()
            },
        )
        .await
        .expect("landscape pdf failed");
    assert_eq!(&landscape[..5], b"%PDF-");

    session.close().await;
    engine.shutdown().await;
}

/// Representative static invoice: heading, company block, an itemized table with
/// rows + totals, and a CSS background color (to exercise printBackground). This
/// stands in for invoice-service's Handlebars-rendered output.
const INVOICE_HTML: &str = r#"<!doctype html><html><head><meta charset="utf-8">
<title>Invoice INV-2026-0042</title>
<style>
  body { font-family: Arial, sans-serif; margin: 0; color: #1a1a1a; }
  .sheet { padding: 32px; }
  h1 { color: #0b5; }
  .company { background: #eef6ff; padding: 16px; border-radius: 6px; }
  table { width: 100%; border-collapse: collapse; margin-top: 24px; }
  th { background: #0b5; color: #fff; text-align: left; padding: 8px; }
  td { padding: 8px; border-bottom: 1px solid #ddd; }
  .totals { margin-top: 16px; text-align: right; font-weight: bold; }
</style></head>
<body><div class="sheet">
  <h1>INVOICE</h1>
  <div class="company">
    <strong>Skuad Pte. Ltd.</strong><br>
    68 Circular Road, Singapore 049422<br>
    Invoice #INV-2026-0042 &middot; Date: 2026-07-11
  </div>
  <table>
    <thead><tr><th>Description</th><th>Qty</th><th>Unit</th><th>Amount</th></tr></thead>
    <tbody>
      <tr><td>Employer of Record — July</td><td>3</td><td>$499.00</td><td>$1,497.00</td></tr>
      <tr><td>Compliance & payroll processing</td><td>3</td><td>$49.00</td><td>$147.00</td></tr>
      <tr><td>Benefits administration</td><td>3</td><td>$29.00</td><td>$87.00</td></tr>
    </tbody>
  </table>
  <div class="totals">Subtotal: $1,731.00<br>Tax (0%): $0.00<br>Total Due: $1,731.00</div>
</div></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn set_content_renders_and_pdf_with_footer() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();

    // set_content loads a raw HTML string (no navigation/URL).
    session
        .set_content(INVOICE_HTML, Some("networkidle0"))
        .await
        .expect("set_content failed");

    // Extraction sees the injected content.
    let h1 = session
        .extract(None, "h1", None)
        .await
        .expect("extract failed");
    assert_eq!(h1, vec!["INVOICE".to_string()], "h1 not rendered: {h1:?}");
    let rows = session
        .extract(None, "tbody tr td:first-child", None)
        .await
        .expect("extract rows failed");
    assert!(
        rows.iter().any(|r| r.contains("Employer of Record")),
        "table rows not rendered: {rows:?}"
    );

    // A snapshot of the set-content page also reflects the document.
    let snap = session.snapshot().await.expect("snapshot failed");
    assert!(
        snap.contains("INVOICE"),
        "snapshot missing invoice heading: {snap}"
    );

    // PDF with the real footer template + displayHeaderFooter, background on, and
    // puppeteer-style CSS-unit margins (top 20px / bottom 35px).
    let footer = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/invoice-footer.html"
    ))
    .expect("footer fixture must exist");
    let bytes = session
        .pdf(
            None,
            kitewright_engine::PdfOptions {
                format: Some("A4".into()),
                print_background: true,
                display_header_footer: true,
                footer_template: Some(footer),
                margin_top: Some("20px".into()),
                margin_bottom: Some("35px".into()),
                ..Default::default()
            },
        )
        .await
        .expect("pdf with footer failed");

    // Valid PDF: magic header, EOF marker, and a plausible size for a full-page
    // invoice with background + footer.
    assert_eq!(&bytes[..5], b"%PDF-", "not a PDF");
    assert!(
        bytes.len() > 3000,
        "invoice PDF implausibly small: {} bytes",
        bytes.len()
    );
    let tail = &bytes[bytes.len().saturating_sub(1024)..];
    assert!(
        tail.windows(5).any(|w| w == b"%%EOF"),
        "PDF missing %%EOF trailer"
    );
    // At least one page object is present (PDF content streams are compressed, so
    // we assert structure rather than the footer's literal text).
    let page_objs = bytes.windows(10).filter(|w| *w == b"/Type /Pag").count();
    assert!(page_objs >= 1, "no /Type /Page objects found in PDF");

    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn actionability_passes_fast_and_reports_specific_blockers() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();
    session.navigate(&url).await.expect("navigate failed");

    // --- visible + enabled + stable element: passes quickly and acts ---
    let started = std::time::Instant::now();
    session
        .click("#act-ok", None)
        .await
        .expect("click ok failed");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "actionable click should be fast, took {:?}",
        started.elapsed()
    );
    let out = session.extract(None, "#act-out", None).await.unwrap();
    assert_eq!(out, vec!["act-ok-clicked"]);

    // Blocked cases must fail with a cause-specific message. A short per-op
    // timeout keeps the test quick (the default budget is 5s each).
    let disabled = session.click("#act-disabled", Some(400)).await.unwrap_err();
    assert!(
        format!("{disabled:#}").contains("disabled"),
        "disabled error: {disabled:#}"
    );

    let hidden = session.click("#act-hidden", Some(400)).await.unwrap_err();
    assert!(
        format!("{hidden:#}").contains("not visible"),
        "hidden error: {hidden:#}"
    );

    let covered = session.click("#act-covered", Some(400)).await.unwrap_err();
    assert!(
        format!("{covered:#}").contains("covered"),
        "covered error: {covered:#}"
    );

    let missing = session.click("#act-nope", Some(400)).await.unwrap_err();
    assert!(
        format!("{missing:#}").contains("#act-nope")
            || format!("{missing:#}").contains("no element"),
        "missing error: {missing:#}"
    );

    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn markdown_extraction_produces_expected_substrings() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();

    let md = session
        .extract_markdown(Some(&url))
        .await
        .expect("markdown failed");
    assert!(md.contains("# Doc Title"), "missing h1: {md}");
    assert!(md.contains("## Features"), "missing h2: {md}");
    assert!(
        md.contains("[sample link](https://example.com/x)"),
        "missing link: {md}"
    );
    assert!(md.contains("- first feature"), "missing list item: {md}");
    assert!(md.contains("```"), "missing code fence: {md}");

    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_diff_baseline_then_changes() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();
    session.navigate(&url).await.expect("navigate failed");

    // First diff call is the baseline (full tree, tagged as such).
    let baseline = session.snapshot_diff().await.expect("baseline diff failed");
    assert!(baseline.contains("baseline"), "not baseline: {baseline}");
    assert!(
        baseline.contains("Hello from fixture"),
        "baseline missing tree"
    );

    // Mutate the DOM: reveal a previously hidden div, then diff again.
    session
        .click("#reveal-btn", None)
        .await
        .expect("reveal click failed");
    session
        .wait_for(Some("#secret"), Some("secret-revealed-text"), Some(3_000))
        .await
        .expect("reveal did not happen");
    let diff = session.snapshot_diff().await.expect("second diff failed");
    assert!(diff.contains("snapshot diff"), "not a diff: {diff}");
    assert!(
        diff.contains("secret-revealed-text"),
        "diff should surface the newly revealed text: {diff}"
    );

    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn console_and_network_capture() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();
    session.navigate(&url).await.expect("navigate failed");

    // --- console: click a button that logs, then read the buffer ---
    session
        .click("#log-btn", None)
        .await
        .expect("log click failed");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let messages = session.console(false).await.expect("console failed");
    assert!(
        messages.iter().any(|m| m.text.contains("hello-console-42")),
        "console did not capture the log: {messages:?}"
    );

    // --- network: the document navigation must have been captured ---
    let all = session.network(false, None).await.expect("network failed");
    assert!(
        all.iter()
            .any(|r| r.url.contains(&url) || r.url.contains("127.0.0.1")),
        "network did not capture the document request: {all:?}"
    );

    // Trigger an explicit fetch, then filter by its URL.
    session
        .click("#fetch-btn", None)
        .await
        .expect("fetch click failed");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let ping = session
        .network(false, Some("/api/ping"))
        .await
        .expect("network filter failed");
    assert!(
        ping.iter().any(|r| r.url.contains("/api/ping")),
        "network did not capture the fetch: {ping:?}"
    );

    // clear empties the console buffer.
    let _ = session.console(true).await.unwrap();
    let after = session.console(false).await.unwrap();
    assert!(after.is_empty(), "console buffer not cleared: {after:?}");

    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn element_handle_query_and_click() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();
    session.navigate(&url).await.expect("navigate failed");

    // --- query a button handle, click it, assert the effect ---
    let btn = session
        .query("#act-ok")
        .await
        .expect("query failed")
        .expect("button handle should exist");
    assert_eq!(btn.text().await.unwrap(), "OK Button");
    btn.click().await.expect("handle click failed");
    let out = session.extract(None, "#act-out", None).await.unwrap();
    assert_eq!(out, vec!["act-ok-clicked"]);

    // --- query_all returns every match; text() reads through ---
    let items = session.query_all(".item").await.expect("query_all failed");
    assert_eq!(items.len(), 3, "expected 3 .item handles");
    assert_eq!(items[0].text().await.unwrap(), "alpha");

    // --- attribute + bounding_box on a handle ---
    let link = session.query("a#link").await.unwrap().expect("link handle");
    assert_eq!(
        link.attribute("href").await.unwrap().as_deref(),
        Some("/link-target")
    );
    let (_, _, w, h) = link.bounding_box().await.unwrap();
    assert!(w > 0.0 && h > 0.0, "bounding box should be non-zero");

    // --- query for a missing element returns None ---
    assert!(session.query("#does-not-exist").await.unwrap().is_none());

    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hover_reveals_css_menu_and_dialog_is_handled() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));
    let session = engine.create_session();
    session.navigate(&url).await.expect("navigate failed");

    // --- hover reveals a CSS :hover menu ---
    session
        .hover("#hover-trigger", None)
        .await
        .expect("hover failed");
    let menu = session.extract(None, "#hover-menu", None).await.unwrap();
    assert_eq!(menu, vec!["menu-revealed"], "hover did not reveal menu");

    // --- arm dialog handling, then trigger a confirm() that blocks JS ---
    session
        .handle_dialog(true, None)
        .await
        .expect("handle_dialog failed");
    session
        .click("#confirm-btn", None)
        .await
        .expect("confirm click failed");
    session
        .wait_for(None, Some("accepted"), Some(3_000))
        .await
        .expect("confirm dialog was not auto-accepted");

    session.close().await;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lite_mode_blocks_images_but_full_mode_loads_them() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    // Two independent origins so their hit-sets (and disk-cache keys) never
    // collide: one navigated in full mode, one in lite mode.
    let (url_full, hits_full) = start_tracking_fixture_server().await;
    let (url_lite, hits_lite) = start_tracking_fixture_server().await;
    let engine = test_engine(Duration::from_secs(60));

    // --- full mode (lite=false): the <img> IS fetched ---
    let full = engine.create_session();
    full.navigate_with(&url_full, Some(false))
        .await
        .expect("full navigate failed");
    // navigate resolves on the load event (which waits for images in full
    // mode), but give a slack margin for the request to be recorded.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        hits_full.lock().unwrap().contains("/tracked-image.png"),
        "full-mode navigation should have fetched the image, hits: {:?}",
        hits_full.lock().unwrap()
    );

    // --- lite mode (lite=true): the <img> is NOT fetched ---
    let lite = engine.create_session();
    lite.navigate_with(&url_lite, Some(true))
        .await
        .expect("lite navigate failed");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (doc_fetched, img_fetched, lite_snapshot) = {
        let h = hits_lite.lock().unwrap();
        (
            h.contains("/"),
            h.contains("/tracked-image.png"),
            format!("{h:?}"),
        )
    };
    assert!(
        doc_fetched,
        "lite-mode navigation should still fetch the document, hits: {lite_snapshot}"
    );
    assert!(
        !img_fetched,
        "lite mode must block the image request, but it was fetched: {lite_snapshot}"
    );

    full.close().await;
    lite.close().await;
    engine.shutdown().await;
}

/// Latency decomposition against a LOCAL fixture server (no external network),
/// so the numbers isolate our own overhead + render cost. Run with:
///   cargo test -p kitewright-engine bench_localhost --release -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
async fn bench_localhost_first_navigate() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;

    // --- cold: fresh engine, no prewarm — first navigate pays browser launch ---
    let cold_engine = test_engine_cfg(Duration::from_secs(120), 2);
    let cold_session = cold_engine.create_session();
    let t = std::time::Instant::now();
    cold_session.navigate(&url).await.expect("cold navigate");
    let cold = t.elapsed();
    cold_engine.shutdown().await;

    // --- prewarmed: prewarm (launch + pool) off the critical path, then a NEW
    //     session navigate — no launch, instantly-ready pooled context ---
    let warm_engine = test_engine_cfg(Duration::from_secs(120), 2);
    let t = std::time::Instant::now();
    warm_engine.prewarm().await.expect("prewarm");
    let prewarm = t.elapsed();
    let s = warm_engine.create_session();
    let t = std::time::Instant::now();
    s.navigate(&url).await.expect("prewarmed navigate");
    let prewarmed_first = t.elapsed();
    // --- warm: second navigate on the same live session ---
    let t = std::time::Instant::now();
    s.navigate(&url).await.expect("warm navigate");
    let warm = t.elapsed();
    s.close().await;
    warm_engine.shutdown().await;

    eprintln!("\n==== kitewright localhost latency decomposition ====");
    eprintln!("cold first-navigate (incl. browser launch): {cold:?}");
    eprintln!("prewarm() (launch + fill context pool):      {prewarm:?}");
    eprintln!("prewarmed first-navigate (pooled context):   {prewarmed_first:?}");
    eprintln!("warm navigate (reused session page):         {warm:?}");
    eprintln!("====================================================\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn warm_context_pool_serves_ready_context_and_refills() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let url = start_fixture_server().await;
    let engine = test_engine_cfg(Duration::from_secs(60), 2);

    // Prewarm fills the pool to the cap so a NEW session pays no
    // context-creation cost.
    engine.prewarm().await.expect("prewarm failed");
    assert_eq!(
        engine.pooled_context_count().await,
        2,
        "prewarm should have filled the pool to the cap"
    );

    // A fresh session's first op works immediately (it consumes a pooled
    // context)...
    let s = engine.create_session();
    let info = s.navigate(&url).await.expect("navigate failed");
    assert_eq!(info.title, "Fixture Page");

    // ...and the pool refills back to the cap in the background.
    let mut refilled = false;
    for _ in 0..50 {
        if engine.pooled_context_count().await == 2 {
            refilled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        refilled,
        "pool should refill to the cap after a session took a context (was {})",
        engine.pooled_context_count().await
    );

    // A second brand-new session also starts cleanly from the pool.
    let s2 = engine.create_session();
    assert_eq!(
        s2.navigate(&url)
            .await
            .expect("second navigate failed")
            .title,
        "Fixture Page"
    );

    s.close().await;
    s2.close().await;
    engine.shutdown().await;
    // Draining with the browser: pool is empty once the process is gone.
    assert_eq!(engine.pooled_context_count().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_prewarm_noops_when_unset_and_warms_when_set() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    // --- unset (default): prewarm must succeed and the browser is usable ---
    let engine = test_engine(Duration::from_secs(60));
    engine
        .prewarm()
        .await
        .expect("prewarm with no KITE_PREWARM_URL must be a safe no-op");
    assert!(engine.is_running().await);
    let url = start_fixture_server().await;
    let s = engine.create_session();
    assert_eq!(
        s.navigate(&url).await.expect("navigate failed").title,
        "Fixture Page"
    );
    s.close().await;
    engine.shutdown().await;

    // --- set to a reachable origin: prewarm still succeeds (best-effort) ---
    let target = start_fixture_server().await;
    let engine2 = Engine::new(EngineConfig {
        idle_ttl: Duration::from_secs(60),
        nav_timeout: Duration::from_secs(20),
        no_sandbox: true,
        headful: false,
        executable: chrome_path(),
        context_pool_size: 1,
        cache_dir: std::env::temp_dir().join(format!("kitewright-test-pw-{}", std::process::id())),
        prewarm_url: Some(target.clone()),
        viewport: (1440, 900),
    });
    engine2
        .prewarm()
        .await
        .expect("prewarm with KITE_PREWARM_URL set must succeed");
    assert!(engine2.is_running().await);
    let s2 = engine2.create_session();
    assert_eq!(
        s2.navigate(&target).await.expect("navigate failed").title,
        "Fixture Page"
    );
    s2.close().await;
    engine2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_cache_dir_is_created_and_reused() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let cache_dir = std::env::temp_dir().join(format!(
        "kitewright-test-cache-shared-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let engine = Engine::new(EngineConfig {
        idle_ttl: Duration::from_secs(60),
        nav_timeout: Duration::from_secs(20),
        no_sandbox: true,
        headful: false,
        executable: chrome_path(),
        context_pool_size: 1,
        cache_dir: cache_dir.clone(),
        prewarm_url: None,
        viewport: (1440, 900),
    });
    let url = start_fixture_server().await;

    // One-shot navigations use the browser's default context, which is what
    // the shared on-disk cache (`--disk-cache-dir`) applies to. A second fetch
    // of the same asset must still succeed (served from cache or re-fetched).
    let first = engine.navigate(&url).await.expect("first navigate failed");
    assert_eq!(first.title, "Fixture Page");
    let second = engine.navigate(&url).await.expect("second navigate failed");
    assert_eq!(second.title, "Fixture Page");

    assert!(
        cache_dir.exists(),
        "shared cache dir should have been created at {cache_dir:?}"
    );

    engine.shutdown().await;
    let _ = std::fs::remove_dir_all(&cache_dir);
}

/// Regression: Chrome opens an initial tab on launch, but sessions run in their
/// own browser contexts (a separate window when headed), so that default tab is
/// never used. It must be closed once a real session page exists — otherwise it
/// lingers as a stray blank window. With pooling disabled, exactly one page
/// (the session page) should remain after a navigate.
#[tokio::test]
async fn session_navigate_retires_the_launch_tab() {
    let engine = test_engine_cfg(Duration::from_secs(120), 0);
    let session = engine.create_session();
    session
        .navigate("about:blank")
        .await
        .expect("navigate failed");
    assert_eq!(
        engine.page_count().await,
        1,
        "Chrome's initial launch tab was not retired (stray blank window)"
    );
    engine.shutdown().await;
}
