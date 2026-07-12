//! End-to-end test of the MCP Streamable HTTP surface: spawns the actual
//! compiled binary and drives the protocol like a real client.
//! Protocol tests need no browser; the tool-call test skips without one.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct ServerGuard {
    child: Child,
    base: String,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        // SIGTERM first so the server's graceful-shutdown path runs and the
        // browser child is reaped (SIGKILL would orphan chrome-headless-shell).
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status();
            for _ in 0..30 {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The heavy end-to-end tests below spawn a server and drive a real Chrome.
/// They are gated out of the blocking CI job (the shared 2-core runner starves
/// the renderer, yielding empty tool output), and run locally by default plus
/// in the non-blocking `browser` CI job. Set KITE_SKIP_BROWSER_E2E=1 to skip.
fn skip_browser_e2e() -> bool {
    if std::env::var("KITE_SKIP_BROWSER_E2E").is_ok() {
        eprintln!("SKIP: browser e2e disabled (KITE_SKIP_BROWSER_E2E set)");
        return true;
    }
    false
}

fn chrome_path() -> Option<String> {
    // Under the CI skip flag, report "no browser" so every test guarded by
    // `chrome_path().is_none()` self-skips (see skip_browser_e2e). The three
    // non-browser tests here (auth, rate limit, cross-origin) don't consult this
    // and keep running in the blocking job.
    if std::env::var("KITE_SKIP_BROWSER_E2E").is_ok() {
        return None;
    }
    if let Ok(p) = std::env::var("BROWSER_EXECUTABLE") {
        return Some(p);
    }
    for p in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
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

async fn start_server(port: u16) -> ServerGuard {
    start_server_with(port, &[]).await
}

async fn start_server_with(port: u16, envs: &[(&str, &str)]) -> ServerGuard {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kite"));
    cmd.env("MCP_HTTP_BIND", format!("127.0.0.1:{port}"))
        .env("BROWSER_NO_SANDBOX", "1")
        // Kite launches headed by default; tests must run headless (no display
        // on CI, and no windows popping up during a local test run).
        .env("KITE_HEADLESS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    if let Some(chrome) = chrome_path() {
        cmd.env("BROWSER_EXECUTABLE", chrome);
    }
    let mut child = cmd.spawn().expect("failed to spawn kite binary");
    let base = format!("http://127.0.0.1:{port}/mcp");

    // Wait for the port to accept connections.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return ServerGuard { child, base };
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("server did not start listening on port {port}");
}

fn sse_data(body: &str) -> serde_json::Value {
    let line = body
        .lines()
        .find(|l| l.starts_with("data: {"))
        .unwrap_or_else(|| panic!("no data line in response: {body}"));
    serde_json::from_str(line.trim_start_matches("data: ")).expect("invalid JSON in SSE data")
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    session: Option<&str>,
    payload: serde_json::Value,
) -> reqwest::Response {
    let mut req = client
        .post(base)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(payload.to_string());
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    req.send().await.expect("request failed")
}

#[tokio::test(flavor = "multi_thread")]
async fn streamable_http_handshake_and_tools() {
    if skip_browser_e2e() {
        return;
    }
    let port = 18300 + (std::process::id() % 500) as u16;
    let server = start_server(port).await;
    let client = reqwest::Client::new();

    // --- initialize: must return serverInfo and a session id header ---
    let resp = post(
        &client,
        &server.base,
        None,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "initialize failed: {}",
        resp.status()
    );
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .expect("missing mcp-session-id header")
        .to_str()
        .unwrap()
        .to_string();
    let body = resp.text().await.unwrap();
    let init = sse_data(&body);
    assert_eq!(init["result"]["serverInfo"]["name"], "kitewright");

    // --- initialized notification ---
    let resp = post(
        &client,
        &server.base,
        Some(&sid),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert!(resp.status().is_success());

    // --- tools/list: all three browser tools present with schemas ---
    let resp = post(
        &client,
        &server.base,
        Some(&sid),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    let tools = sse_data(&resp.text().await.unwrap());
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .expect("tools not an array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for expected in [
        "browser_navigate",
        "browser_screenshot",
        "browser_extract",
        "browser_snapshot",
        "browser_click",
        "browser_type",
        "browser_press_key",
        "browser_wait_for",
        "browser_fill_form",
        "browser_select_option",
        "browser_hover",
        "browser_navigate_back",
        "browser_handle_dialog",
        "browser_save_state",
        "browser_restore_state",
        "browser_assert",
        "browser_pdf",
        "browser_set_content",
        "browser_extract_markdown",
        "browser_console",
        "browser_network",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing tool {expected}, got {names:?}"
        );
    }

    // --- request without a session id (and not initialize) must be rejected ---
    let resp = post(
        &client,
        &server.base,
        None,
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
    )
    .await;
    assert!(
        resp.status().is_client_error(),
        "sessionless tools/list should be rejected, got {}",
        resp.status()
    );

    // --- tool call end-to-end (only when a browser is available) ---
    if chrome_path().is_some() {
        let resp = post(
            &client,
            &server.base,
            Some(&sid),
            serde_json::json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": "browser_navigate", "arguments": {"url": "data:text/html,<title>T</title><body>hello-world</body>"}}
            }),
        )
        .await;
        let result = sse_data(&resp.text().await.unwrap());
        let text = result["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        assert!(
            text.contains("hello-world"),
            "unexpected tool output: {text}"
        );
    } else {
        eprintln!("SKIP tool-call assertion: no Chromium found");
    }
}

/// Full MCP handshake; returns the session id.
async fn init_session(client: &reqwest::Client, base: &str) -> String {
    let resp = post(
        client,
        base,
        None,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }),
    )
    .await;
    assert!(resp.status().is_success(), "initialize: {}", resp.status());
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .expect("missing mcp-session-id header")
        .to_str()
        .unwrap()
        .to_string();
    let resp = post(
        client,
        base,
        Some(&sid),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert!(resp.status().is_success());
    sid
}

async fn call_tool(
    client: &reqwest::Client,
    base: &str,
    sid: &str,
    id: u64,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let resp = post(
        client,
        base,
        Some(sid),
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "tools/call {name}: {}",
        resp.status()
    );
    let result = sse_data(&resp.text().await.unwrap());
    assert!(
        result["result"]["isError"] != serde_json::json!(true),
        "tool {name} returned an error: {result}"
    );
    result
}

fn tool_text(result: &serde_json::Value) -> &str {
    result["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
}

#[tokio::test(flavor = "multi_thread")]
async fn bearer_auth_is_enforced_when_token_set() {
    let port = 19000 + (std::process::id() % 500) as u16;
    let server = start_server_with(port, &[("MCP_AUTH_TOKEN", "test-secret-token")]).await;
    let client = reqwest::Client::new();
    let init_payload = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    });

    // --- no Authorization header: 401 with a JSON-RPC error body ---
    let resp = post(&client, &server.base, None, init_payload.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value =
        serde_json::from_str(&resp.text().await.unwrap()).expect("401 body must be JSON");
    assert_eq!(body["jsonrpc"], "2.0");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("unauthorized"));

    // --- wrong token: 401 ---
    let resp = client
        .post(&server.base)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", "Bearer wrong-token")
        .body(init_payload.to_string())
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // --- correct token: handshake succeeds ---
    let resp = client
        .post(&server.base)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", "Bearer test-secret-token")
        .body(init_payload.to_string())
        .send()
        .await
        .expect("request failed");
    assert!(
        resp.status().is_success(),
        "authorized initialize failed: {}",
        resp.status()
    );
    assert!(resp.headers().get("mcp-session-id").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_returns_429_when_exceeded() {
    let port = 19600 + (std::process::id() % 500) as u16;
    let server = start_server_with(port, &[("MCP_RATE_LIMIT_PER_MINUTE", "3")]).await;
    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    });

    let mut saw_429 = false;
    for i in 0..5 {
        let resp = post(&client, &server.base, None, payload.clone()).await;
        if i < 3 {
            assert_ne!(
                resp.status(),
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                "request {i} should be within the limit"
            );
        } else {
            assert_eq!(
                resp.status(),
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                "request {i} should be rate limited"
            );
            let body: serde_json::Value =
                serde_json::from_str(&resp.text().await.unwrap()).expect("429 body must be JSON");
            assert!(body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("rate limit"));
            saw_429 = true;
        }
    }
    assert!(saw_429);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_snapshot_click_type_wait_for_end_to_end() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let port = 20200 + (std::process::id() % 500) as u16;
    let server = start_server(port).await;
    let client = reqwest::Client::new();
    let sid = init_session(&client, &server.base).await;

    // Fixture page as a data: URL (spaces %20-encoded): a heading, a button
    // that reveals text on click, and a labelled input mirroring its value.
    let url = "data:text/html,<title>Interact</title>\
        <h1>Snap%20Heading</h1>\
        <button onclick=\"document.getElementById('x').textContent='now-visible'\">Press%20me</button>\
        <input aria-label=\"Name%20box\" oninput=\"document.getElementById('m').textContent=this.value\">\
        <div id='x'></div><div id='m'></div>";

    let nav = call_tool(
        &client,
        &server.base,
        &sid,
        10,
        "browser_navigate",
        serde_json::json!({"url": url}),
    )
    .await;
    assert!(tool_text(&nav).contains("Interact"));

    // --- snapshot: roles + accessible names ---
    let snap = call_tool(
        &client,
        &server.base,
        &sid,
        11,
        "browser_snapshot",
        serde_json::json!({}),
    )
    .await;
    let snap_text = tool_text(&snap);
    assert!(
        snap_text.contains("heading") && snap_text.contains("Snap Heading"),
        "snapshot missing heading: {snap_text}"
    );
    assert!(
        snap_text.contains("button") && snap_text.contains("Press me"),
        "snapshot missing button: {snap_text}"
    );
    assert!(
        snap_text.contains("textbox") && snap_text.contains("Name box"),
        "snapshot missing textbox: {snap_text}"
    );

    // --- click the button, wait for the revealed text (session persists) ---
    call_tool(
        &client,
        &server.base,
        &sid,
        12,
        "browser_click",
        serde_json::json!({"selector": "button"}),
    )
    .await;
    let waited = call_tool(
        &client,
        &server.base,
        &sid,
        13,
        "browser_wait_for",
        serde_json::json!({"text": "now-visible", "timeout_ms": 5000}),
    )
    .await;
    assert!(
        tool_text(&waited).contains("elapsed_ms"),
        "wait_for output: {}",
        tool_text(&waited)
    );

    // --- extract WITHOUT url: current page, revealed div ---
    let revealed = call_tool(
        &client,
        &server.base,
        &sid,
        14,
        "browser_extract",
        serde_json::json!({"selector": "#x"}),
    )
    .await;
    assert!(tool_text(&revealed).contains("now-visible"));

    // --- type into the input, read the mirrored value back ---
    call_tool(
        &client,
        &server.base,
        &sid,
        15,
        "browser_type",
        serde_json::json!({"selector": "input", "text": "hi from mcp"}),
    )
    .await;
    let mirrored = call_tool(
        &client,
        &server.base,
        &sid,
        16,
        "browser_extract",
        serde_json::json!({"selector": "#m"}),
    )
    .await;
    assert!(
        tool_text(&mirrored).contains("hi from mcp"),
        "typed text not mirrored: {}",
        tool_text(&mirrored)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pdf_and_differentiators_end_to_end() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    use base64::Engine as _;
    let port = 21400 + (std::process::id() % 500) as u16;
    let server = start_server(port).await;
    let client = reqwest::Client::new();
    let sid = init_session(&client, &server.base).await;

    let url = "data:text/html,<title>Doc</title>\
        <article><h1>Report%20Title</h1>\
        <p>Body with a <a href=\"https://example.com\">link</a>.</p>\
        <ul><li>one</li><li>two</li></ul></article>\
        <button onclick=\"console.log('console-marker-99')\">Log</button>";

    call_tool(
        &client,
        &server.base,
        &sid,
        40,
        "browser_navigate",
        serde_json::json!({"url": url}),
    )
    .await;

    // --- browser_pdf: base64 in the envelope decodes to a %PDF header ---
    let pdf = call_tool(
        &client,
        &server.base,
        &sid,
        41,
        "browser_pdf",
        serde_json::json!({"format": "A4"}),
    )
    .await;
    let pdf_json: serde_json::Value =
        serde_json::from_str(tool_text(&pdf)).expect("pdf envelope must be JSON");
    let b64 = pdf_json["base64"].as_str().expect("missing base64");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("base64 must decode");
    assert_eq!(&bytes[..5], b"%PDF-", "decoded PDF must start with %PDF-");
    assert!(pdf_json["bytes"].as_u64().unwrap_or(0) > 500);

    // --- browser_extract_markdown: headings/links/list render ---
    let md = call_tool(
        &client,
        &server.base,
        &sid,
        42,
        "browser_extract_markdown",
        serde_json::json!({}),
    )
    .await;
    let md_text = tool_text(&md);
    assert!(md_text.contains("# Report Title"), "markdown h1: {md_text}");
    assert!(
        md_text.contains("[link](https://example.com)"),
        "markdown link: {md_text}"
    );

    // --- browser_console: capture a console.log after a click ---
    call_tool(
        &client,
        &server.base,
        &sid,
        43,
        "browser_click",
        serde_json::json!({"selector": "button"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let console = call_tool(
        &client,
        &server.base,
        &sid,
        44,
        "browser_console",
        serde_json::json!({}),
    )
    .await;
    assert!(
        tool_text(&console).contains("console-marker-99"),
        "console capture: {}",
        tool_text(&console)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fill_form_select_option_and_assert_end_to_end() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    let port = 20800 + (std::process::id() % 500) as u16;
    let server = start_server(port).await;
    let client = reqwest::Client::new();
    let sid = init_session(&client, &server.base).await;

    // A form with two inputs, a select, and a submit that echoes the values.
    let url = "data:text/html,<title>Form</title>\
        <input id='u'><input id='p'>\
        <select id='s' onchange=\"document.getElementById('so').textContent=this.value\">\
        <option value='x'>Ex</option><option value='y'>Why</option></select>\
        <button onclick=\"document.getElementById('o').textContent=\
        document.getElementById('u').value+':'+document.getElementById('p').value\">Go</button>\
        <div id='o'></div><div id='so'></div>";

    call_tool(
        &client,
        &server.base,
        &sid,
        30,
        "browser_navigate",
        serde_json::json!({"url": url}),
    )
    .await;

    // --- fill_form: two fields at once ---
    let filled = call_tool(
        &client,
        &server.base,
        &sid,
        31,
        "browser_fill_form",
        serde_json::json!({"fields": [
            {"selector": "#u", "value": "carol"},
            {"selector": "#p", "value": "pw"}
        ]}),
    )
    .await;
    assert!(
        tool_text(&filled).contains("\"filled\": 2"),
        "fill_form output: {}",
        tool_text(&filled)
    );
    call_tool(
        &client,
        &server.base,
        &sid,
        32,
        "browser_click",
        serde_json::json!({"selector": "button"}),
    )
    .await;
    let echoed = call_tool(
        &client,
        &server.base,
        &sid,
        33,
        "browser_extract",
        serde_json::json!({"selector": "#o"}),
    )
    .await;
    assert!(tool_text(&echoed).contains("carol:pw"));

    // --- select_option by label, verify change fired ---
    call_tool(
        &client,
        &server.base,
        &sid,
        34,
        "browser_select_option",
        serde_json::json!({"selector": "#s", "label": "Why"}),
    )
    .await;
    let so = call_tool(
        &client,
        &server.base,
        &sid,
        35,
        "browser_extract",
        serde_json::json!({"selector": "#so"}),
    )
    .await;
    assert!(
        tool_text(&so).contains("y"),
        "select change: {}",
        tool_text(&so)
    );

    // --- assert: passed:true for present text ---
    let pass = call_tool(
        &client,
        &server.base,
        &sid,
        36,
        "browser_assert",
        serde_json::json!({"condition_text": "carol:pw", "timeout_ms": 2000}),
    )
    .await;
    assert!(
        tool_text(&pass).contains("\"passed\": true"),
        "assert present: {}",
        tool_text(&pass)
    );

    // --- assert: passed:false for a selector that never appears ---
    let fail = call_tool(
        &client,
        &server.base,
        &sid,
        37,
        "browser_assert",
        serde_json::json!({"condition_selector": "#nope", "timeout_ms": 400}),
    )
    .await;
    assert!(
        tool_text(&fail).contains("\"passed\": false"),
        "assert timeout should be passed:false, not an error: {}",
        tool_text(&fail)
    );
}

/// MCP-path proof of the invoice-service flow: browser_set_content (raw HTML,
/// no URL) followed by browser_pdf with a real footer template +
/// display_header_footer, driven over the actual Streamable HTTP MCP surface.
/// This proves the engine capability end-to-end independently of the napi
/// bindings.
#[tokio::test(flavor = "multi_thread")]
async fn set_content_then_pdf_with_footer_end_to_end() {
    if chrome_path().is_none() {
        eprintln!("SKIP: no Chromium found (set BROWSER_EXECUTABLE)");
        return;
    }
    use base64::Engine as _;
    let port = 22100 + (std::process::id() % 500) as u16;
    let server = start_server(port).await;
    let client = reqwest::Client::new();
    let sid = init_session(&client, &server.base).await;

    // Representative static invoice with a CSS background (printBackground).
    let invoice_html = "<!doctype html><html><head><title>Invoice INV-1</title>\
        <style>body{font-family:Arial;margin:0}\
        h1{color:#0b5}.company{background:#eef6ff;padding:16px}\
        table{width:100%;border-collapse:collapse}\
        th{background:#0b5;color:#fff;padding:8px}td{padding:8px;border-bottom:1px solid #ddd}\
        </style></head><body>\
        <h1>INVOICE</h1>\
        <div class=\"company\"><strong>Skuad Pte. Ltd.</strong><br>Invoice #INV-1</div>\
        <table><thead><tr><th>Description</th><th>Amount</th></tr></thead>\
        <tbody><tr><td>Employer of Record</td><td>$1,497.00</td></tr>\
        <tr><td>Payroll processing</td><td>$147.00</td></tr></tbody></table>\
        <p><strong>Total Due: $1,644.00</strong></p></body></html>";

    // --- browser_set_content: load raw HTML, wait for networkidle0 ---
    let sc = call_tool(
        &client,
        &server.base,
        &sid,
        50,
        "browser_set_content",
        serde_json::json!({"html": invoice_html, "wait_until": "networkidle0"}),
    )
    .await;
    assert!(
        tool_text(&sc).contains("\"ok\": true"),
        "set_content result: {}",
        tool_text(&sc)
    );

    // --- confirm the content rendered ---
    let extracted = call_tool(
        &client,
        &server.base,
        &sid,
        51,
        "browser_extract",
        serde_json::json!({"selector": "h1"}),
    )
    .await;
    assert!(
        tool_text(&extracted).contains("INVOICE"),
        "set_content did not render h1: {}",
        tool_text(&extracted)
    );

    // --- browser_pdf with footer_template + display_header_footer + margins ---
    let footer = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/invoice-footer.html"
    ))
    .expect("footer fixture must exist");
    let pdf = call_tool(
        &client,
        &server.base,
        &sid,
        52,
        "browser_pdf",
        serde_json::json!({
            "format": "A4",
            "print_background": true,
            "display_header_footer": true,
            "footer_template": footer,
            "margin_top": "20px",
            "margin_bottom": "35px",
        }),
    )
    .await;
    let pdf_json: serde_json::Value =
        serde_json::from_str(tool_text(&pdf)).expect("pdf envelope must be JSON");
    let b64 = pdf_json["base64"].as_str().expect("missing base64");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("base64 must decode");
    assert_eq!(&bytes[..5], b"%PDF-", "decoded PDF must start with %PDF-");
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
}

/// DNS-rebinding protection (CVE-2026-42559 mitigation): a request carrying a
/// cross-origin `Origin` header (as a malicious webpage in a browser would) is
/// rejected with 403, even before auth. Requests with no Origin (normal MCP
/// clients) are unaffected — every other test here sends none and passes.
#[tokio::test(flavor = "multi_thread")]
async fn cross_origin_request_is_rejected() {
    let port = 18990 + (std::process::id() % 400) as u16;
    let server = start_server(port).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(&server.base)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("origin", "https://evil.example")
        .body(
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                           "clientInfo": {"name": "attacker", "version": "0"}}
            })
            .to_string(),
        )
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 403, "cross-origin request must be rejected");
}
