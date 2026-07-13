# Kitewright

**Browser automation for AI agents as a single small binary.** An MCP server (Streamable HTTP) that gives LLM clients `navigate` / `screenshot` / `extract` — without carrying the Node.js + Playwright stack.

> Working name — final project name TBD before launch.

## Why — measured, not claimed

Head-to-head vs `@playwright/mcp` 0.0.78, same machine, same Chromium headless-shell build, same page ([full methodology](BENCHMARKS.md)):

| | @playwright/mcp | **Kitewright** |
|---|---:|---:|
| Cold start → listening | 354 ms | **75 ms** |
| Server RSS (idle) | 102–125 MB | **7.6 MB** |
| Server RSS (after work) | 93 MB | **10.9 MB** |
| Distribution | 18 MB pkg + Node.js runtime | **6.9 MB static binary** |
| First navigate (incl. browser launch) | 2623 ms | **822 ms** (session pre-warming) |
| Warm navigate latency | 80–116 ms | 99–105 ms (tie) |
| Idle behavior | browser kept alive | **browser reaped after idle TTL**, pre-warmed again on next session |

The browser itself (Chromium) costs the same in any language — warm latency is a tie because both speak CDP to the same browser. The wins are everything around it: startup, distribution, idle footprint, and lifecycle management. The honest gap: playwright-mcp ships ~25 tools today, we ship 21 — closing that is the roadmap.

## Reliability — actionability auto-waiting

`click` / `type` / `fill_form` / `select_option` / `hover` don't fire blindly. Before acting, the engine polls (100 ms, up to a 5 s per-op budget) until the target element is **present**, **visible** (not `display:none` / `visibility:hidden` / zero-size), **enabled** (no `disabled` / `aria-disabled`), **not covered** by another element at its click point, and **geometrically stable** across two consecutive frames. A settled element passes on the first poll, so this is invisible when things are fine — but when an action can't happen, you get a *cause-specific* error (`not found` / `not visible` / `disabled` / `covered` / `unstable`) instead of a silent misclick or a generic timeout. Transient CDP errors are retried twice. Pass `timeout_ms` on any interaction tool to override the 5 s per-op budget (e.g. a short timeout to fail fast when you expect an element to already be there).

## Performance

External-site latency is **network-bound** — DNS + TLS + TTFB (~400–600 ms) is a floor no tool beats, and Kitewright does not claim to. What it *does* attack is every controllable cost around the network:

- **Prewarm + warm-context pool.** The moment an MCP session initializes, the server launches the browser and fills a small pool of pre-created blank browser contexts (`MCP_CONTEXT_POOL`, default 2) in the background. A new session is then handed a ready context+page, so its **first** navigate pays zero browser-launch *and* zero context-creation cost. Measured localhost first-navigate: **~31 ms prewarmed** vs ~709 ms cold (Apple Silicon; [details](BENCHMARKS.md)). The pool drains with the browser on idle-reap and refills lazily on next demand, so idle footprint stays at the ~8 MB baseline.
- **Lite mode.** `browser_navigate {lite:true}` (and the default for `extract` / `extract_markdown`) blocks images/media/fonts + ad/analytics hosts before the load — 30–70 % faster DOM-ready on heavy pages by skipping the bulk of the bytes. Screenshots/PDF never block resources.
- **Shared disk cache + connection pre-warm.** A stable `--disk-cache-dir` (`KITE_CACHE_DIR`) lets repeat asset fetches hit cache across runs; `KITE_PREWARM_URL` establishes DNS+TLS to a known origin during prewarm.

The honest framing: the wins are browser launch, page weight, session start, and connection setup — not the network round-trip to a remote origin.

## Architecture

```
crates/
├── engine/    kitewright-engine — CDP core (chromiumoxide): lazy launch, idle reaper,
│              per-session browser contexts, capped text extraction + AX snapshots.
│              Shared by all frontends.
└── server/    kitewright — rmcp Streamable HTTP server exposing the engine as MCP tools.
bindings/
└── node/      @kitewright/node — napi-rs bindings exposing a Puppeteer-compatible
               (experimental) API over the same engine (built separately; kept out
               of the core cargo workspace). See "@kitewright/node" below.
```

Each MCP session owns one persistent page inside its own Chromium browser context (cookie isolation between agents): log in once, keep clicking. The page and context are closed when the session ends; the browser itself is still reaped after the idle TTL and transparently relaunched on the next call.

## Install

Kitewright is a single static binary named `kite`. Pick whichever is easiest:

```bash
# Prebuilt binary (fastest) — downloads the GitHub Release asset for your platform:
cargo binstall kitewright

# From a checkout, via cargo:
cargo install --path crates/server        # → ~/.cargo/bin/kite

# Homebrew (tap TBD — formula template in packaging/kitewright.rb):
# brew install kitewright/tap/kitewright

# Docker (headless Chromium bundled in the image):
docker run --rm -p 8090:8090 kitewright   # build locally: docker build -t kitewright .

# Build from source (needs a Rust toolchain):
cargo build --release -p kitewright        # → target/release/kite
```

### Get a browser

Kitewright drives Chromium over CDP but does not embed one. It uses a system
Chrome/Chromium when present, honors `BROWSER_EXECUTABLE`, and — if neither is
found — falls back to a browser downloaded by `kite install`:

```bash
kite install    # fetch the latest stable chrome-headless-shell into the kite cache
```

`kite install` downloads the current Chrome-for-Testing `chrome-headless-shell`
build for your platform into `$KITE_CACHE_DIR` (or the OS cache dir) and the
engine picks it up automatically — no `BROWSER_EXECUTABLE` needed. Re-running is
a no-op once a build is present. The Docker image already ships Chromium.

## Run & connect

`kite` with no arguments serves MCP over **Streamable HTTP** (networked, default,
supports auth + many sessions); `kite --stdio` serves a single session over
**stdio** for local clients.

```bash
kite
# → kitewright listening on http://0.0.0.0:8090/mcp
```

**HTTP transport** — connect from Claude Code:

```bash
claude mcp add kite --transport http http://localhost:8090/mcp
```

**stdio transport** — MCP client config (Claude Desktop, Cursor, …):

```json
{
  "mcpServers": {
    "kite": {
      "command": "kite",
      "args": ["--stdio"]
    }
  }
}
```

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `MCP_HTTP_BIND` | `0.0.0.0:8090` | Listen address |
| `MCP_AUTH_TOKEN` | unset | When set, `/mcp` requires `Authorization: Bearer <token>` (401 otherwise). Unset = open access + startup warning |
| `MCP_RATE_LIMIT_PER_MINUTE` | `300` | Per-client-IP request limit (fixed 60s window); 429 when exceeded |
| `BROWSER_EXECUTABLE` | auto-detect | Path to chrome / chromium / chrome-headless-shell. When unset: a system Chrome/Chromium is detected, else a `kite install`-managed build in the cache dir |
| `BROWSER_NO_SANDBOX` | unset | Set (any value) to pass `--no-sandbox` (containers) |
| `KITE_HEADLESS` | unset | Kite launches a **headed** (visible) browser by default so you can watch automation. Set (any value) to run **headless** — required on servers, CI, and containers with no display, where a headed Chrome fails to launch |
| `KITE_IDLE_TIMEOUT_SECS` | `1800` | Idle seconds before a headless browser is reaped to free memory. Default 30min keeps a session alive across normal pauses (headed never reaps). A reap that does happen is recovered by cookie **auto-restore**, so an authenticated session survives it. Lower it on a memory-constrained multi-session server |
| `KITE_ALLOW_SECRET_FILES` | unset | Set (any value) to let `browser_fill_secret` read `file:/path` secrets from disk. Optionally fence reads to a directory with `KITE_SECRET_DIR`. `env:` secrets need no opt-in |
| `KITE_VIEWPORT` | `1440x900` | Default viewport / window size as `WIDTHxHEIGHT` (Chromium's own default is a cramped 800x600). Adjust at runtime with the `browser_resize` tool |
| `MCP_CONTEXT_POOL` | `2` | Number of pre-warmed blank browser contexts kept ready so a **new** session gets an instantly-usable context+page (zero context-creation latency). `0` disables. The pool refills in the background and drains with the browser on idle-reap (it never keeps the process alive) |
| `KITE_CACHE_DIR` | `<tmp>/kitewright-cache` | Shared on-disk HTTP cache (`--disk-cache-dir`), stable across launches so repeat asset fetches hit cache. NOTE: per-session isolated contexts (cookie isolation) use an ephemeral cache; this benefits the browser's default context |
| `KITE_PREWARM_URL` | unset | If set, prewarm navigates a throwaway page to this origin to establish DNS+TLS+connection before the first real navigate. No-op when unset |
| `BROWSER_PREWARM` | unset | Set (any value) to launch + pre-warm the browser at server boot (otherwise prewarm fires when an MCP session initializes) |
| `RUST_LOG` | `info` | Log filter |

## Tools (v0.4)

All tools operate on the session's persistent page.

**Read**

- `browser_navigate {url, lite?}` — title, final URL, visible text (capped). `lite:true` enables **lite mode**: block images/media/fonts + common ad/analytics hosts (doubleclick, google-analytics, GTM, facebook pixel, …) via CDP `Network.setBlockedURLs` for a faster DOM-ready on heavy pages (30–70 % on heavy sites — text-only, do **not** use before a screenshot). Sticky for the session until changed. `extract` / `extract_markdown` default to lite when navigating (pixels irrelevant); `screenshot` / `pdf` never block resources
- `browser_extract {url?, selector, attribute?}` — text/attribute from elements matching a selector
- `browser_extract_markdown {url?}` — main content as Markdown ("readability" mode: headings/links/lists/code/tables, nav/script/style stripped, capped at ~20k chars)
- `browser_screenshot {url?, full_page?}` — PNG of the current page (`url` navigates first)
- `browser_pdf {url?, format?, landscape?, print_background?, display_header_footer?, header_template?, footer_template?, margin_top?, margin_bottom?, margin_left?, margin_right?, scale?, prefer_css_page_size?}` — print to PDF (CDP `Page.printToPDF`); the full puppeteer option set including running headers/footers (legal text, page numbers) and CSS-unit margins (`"35px"`/`"20mm"`). Returns a JSON envelope `{format, bytes, base64}` (MCP has no native PDF type — decode `base64` to get the file)
- `browser_set_content {html, wait_until?}` — load a raw HTML string into the current page (puppeteer `page.setContent`) via CDP `Page.setDocumentContent`; `wait_until` is `load` (default) / `domcontentloaded` / `networkidle0`. Pair with `browser_pdf` for an HTML→PDF render with no server round-trip. Handles large documents
- `browser_snapshot {diff?}` — accessibility-tree snapshot (roles, names, states) capped at ~15k chars; `diff:true` returns only what changed since the previous snapshot in this session (first call is the baseline)

**Debug**

- `browser_console {clear?}` — console messages (log/warn/error/info) captured on the page since the last call; `clear:true` empties the buffer
- `browser_network {clear?, filter?}` — network requests (method, url, status, resourceType) captured on the page; `filter` substring-matches the URL

**Interact**

- `browser_click {selector, timeout_ms?}` — scroll into view + click the first match
- `browser_type {selector, text, clear?, press_enter?, timeout_ms?}` — focus and type into an element
- `browser_fill_form {fields: [{selector, value}], timeout_ms?}` — fill several inputs in one call (per-field ok/error summary)
- `browser_fill_secret {selector, secret_ref, press_enter?, timeout_ms?}` — type a secret (password) whose plaintext **never enters the tool call**: `secret_ref` is `env:NAME` (a server env var) or `file:/path` (opt-in via `KITE_ALLOW_SECRET_FILES`). Resolved server-side, then typed
- `browser_select_option {selector, value?, label?, timeout_ms?}` — pick an `<option>` by value or visible label (fires `change`)
- `browser_hover {selector, timeout_ms?}` — move the mouse to an element's center (reveals CSS `:hover` menus)
- `browser_press_key {key}` — send Enter / Tab / Escape / ArrowDown / … to the focused element
- `browser_navigate_back {}` — history back, returns the new title + URL
- `browser_handle_dialog {accept, prompt_text?}` — pre-arm auto-accept/dismiss for the next JS dialog(s)
- `browser_wait_for {selector?, text?, timeout_ms?}` — poll until a selector matches or text appears (default 10s, max 30s)

**State**

- `browser_save_state {}` — capture cookies + localStorage + URL as a JSON string you can persist
- `browser_restore_state {state}` — set cookies immediately; apply localStorage on/after navigating to its origin ("log in once, reuse across sessions")

**Assert**

- `browser_assert {condition_selector?, condition_text?, should_exist?, timeout_ms?}` — structured `{passed, checked, found, elapsed_ms}` (never errors on a failed condition; `should_exist:false` asserts absence)

### Selector syntax

`click` / `type` / `fill_form` / `select_option` / `hover` / `wait_for` / `extract` / `assert` accept three selector forms (the same forms back the internal element-handle API — `query` / `query_all` returning `ElementRef` handles with `.click()` / `.type_str()` / `.text()` / `.attribute()` / `.bounding_box()` — that will underpin the Wave-3 Puppeteer-style npm facade):

- **CSS** (default) — e.g. `#login`, `button.primary`, `input[name="email"]`
- `text=<visible text>` — first visible element whose trimmed text contains it
- `role=<role>[name="<accessible name>"]` — element matching an ARIA role + accessible name, e.g. `role=button[name="Submit"]`

Role resolution is a pragmatic JS heuristic (implicit-role element map + accessible name from `aria-label` / `aria-labelledby` / associated `<label>` / text / `value` / `placeholder` / `title` / `alt`), not a full ARIA computed-name implementation — it covers the common interactive roles agents target. Plain CSS extract still returns all matches; a `text=`/`role=` extract returns the single resolved element.

## @kitewright/node (Puppeteer-compatible, experimental)

`bindings/node` is a [napi-rs](https://napi.rs) native addon that puts a
**Puppeteer-shaped** facade over `kitewright-engine` — the browser lifecycle,
CDP, and waiting heuristics run natively in the shared Rust core, so the JS
layer is thin. It targets the common **HTML→PDF** flow (e.g. an invoice/report
service that uses Puppeteer only for rendering). For that flow the migration is
a one-line import change:

```js
// import puppeteer from 'puppeteer'
import puppeteer from '@kitewright/node'

const browser = await puppeteer.launch({ headless: true, args: ['--no-sandbox'] })
const context = await browser.createBrowserContext()   // per-invoice isolation
const page = await context.newPage()
await page.setContent(html, { waitUntil: 'networkidle0', timeout: 0 })
await page.evaluate(() => document.fonts.ready)         // promise-returning
const pdf = await page.pdf({                            // → Node Buffer
  format: 'a4', printBackground: true, displayHeaderFooter: true,
  footerTemplate: legalFooterHtml,                      // legal text + page numbers
  margin: { top: '20px', bottom: '35px' },
})
await page.close(); await context.close(); await browser.close()
```

Each `Page` is one persistent engine session in its **own** Chromium browser
context, which is exactly the per-`createBrowserContext()` isolation Puppeteer
promises. `page.setContent` + `page.pdf(footerTemplate)` produce a valid,
multi-page PDF with running footers (verified end-to-end in
`bindings/node/test/invoice.e2e.mjs`, which mirrors invoice-service's real flow
and writes `test/out/invoice.pdf`).

### Compatibility matrix

| Supported | Not supported (throws a clear error) |
| --- | --- |
| `puppeteer.launch({ headless, args, executablePath })` | request interception (`setRequestInterception`) |
| `browser.newPage()` / `browser.createBrowserContext()` / `browser.close()` | tracing (`page.tracing`) |
| `context.newPage()` / `context.close()` | device/viewport emulation (`setViewport`, `emulate`) |
| `page.setContent(html, { waitUntil })` (`load` / `domcontentloaded` / `networkidle0`) | `page.screenshot` (use the MCP `browser_screenshot` tool) |
| `page.evaluate(fn \| string)` incl. promise-returning bodies | `page.waitForSelector`, `addScriptTag`, `exposeFunction` |
| `page.pdf(options)` — full option set (`format`, `landscape`, `printBackground`, `displayHeaderFooter`, `header/footerTemplate`, `margin`, `scale`, `preferCssPageSize`) → Buffer | `puppeteer.connect` (remote browser) |
| `page.goto(url)` / `page.close()` | `--no-sandbox` is honored; other Chromium `args` are ignored; `headless:false` is ignored (always headless) |

Notes: `waitUntil: 'networkidle0'` is approximated as load + a short settle
(inline `setContent` content, no interception). Unsupported methods throw
rather than silently no-op so callers discover gaps immediately.

### Build (local, this platform)

The addon is built separately from the core cargo workspace (it is listed under
`[workspace] exclude`, so `cargo clippy --workspace` / the Rust tests never touch
the napi toolchain):

```bash
cd bindings/node
npm install
npx napi build --release --js binding.js --dts binding.d.ts   # emits kitewright-node.node
BROWSER_EXECUTABLE=/path/to/chrome node --test test/invoice.e2e.mjs
```

## kite-pdf — HTML/Typst → PDF

`kite-pdf` is a focused **document → PDF** render service and CLI built on the
same engine (crate `crates/pdf`, binary `kite-pdf`). It has two backends,
selected at build time via Cargo features and at run time per request:

- **Chromium** — `html`/`url` → PDF via the shared `kitewright-engine` (headless
  Chromium, the full `Page.printToPDF` option set: header/footer templates,
  margins, landscape, backgrounds, scale, CSS page size).
- **Typst** — a [Typst](https://typst.app) `template` + JSON `data` → PDF with
  **no browser ever spawned**. The compiler and fonts are embedded in the
  binary; rendering is pure CPU, language-agnostic, and reproducible.

### One crate, three build shapes (same binary name)

| Build | Features | Backends | Approx size | For whom |
| --- | --- | --- | --- | --- |
| `kite-pdf` (default) | `chromium` + `typst` | HTML **and** Typst | ~43 MB (macOS arm64, release+LTO) | You want both; one binary renders anything. |
| `kite-pdf-chromium` | `--no-default-features --features chromium` | HTML only | smallest binary (no Typst/fonts) + runtime browser | You only render HTML/URLs; skip the Typst compiler + bundled fonts. |
| `kite-pdf-lite` | `--no-default-features --features typst` | Typst only | ~39 MB, **no browser** | You control the template; want a browser-free, distroless service. |

```bash
cargo build --release -p kite-pdf                                   # both backends
cargo build --release -p kite-pdf --no-default-features --features chromium
cargo build --release -p kite-pdf --no-default-features --features typst
```

### HTTP API

`POST /render` with a JSON body; responds with `application/pdf` bytes (200) or a
JSON `{ "error": "..." }` (400 client / 500 server). `GET /healthz` returns the
compiled-in backends. Bind address: `KITE_PDF_BIND` (default `0.0.0.0:8091`).

```jsonc
{
  "engine": "chromium" | "typst",   // optional; else inferred (html/url→chromium, template→typst)
  "html":   "<!doctype html>...",   // chromium
  "url":    "https://...",          // chromium
  "template": "= Invoice ...",      // typst source
  "data":   { "number": "INV-1" },  // JSON, exposed to the template as sys.inputs.data
  "format": "A4" | "Letter" | "Legal" | "A3",
  "landscape": false,
  "print_background": false,
  "display_header_footer": false,
  "header_template": "<div>...</div>",
  "footer_template": "<div>... <span class=\"pageNumber\"></span> ...</div>",
  "margin": { "top": "20px", "bottom": "40px", "left": "15px", "right": "15px" }
}
```

Requesting a backend that was not compiled into the running binary returns a
clear **400** (e.g. `"typst backend not compiled in this build — use the full or
-lite build"`). In the Typst template, read the injected data with:

```typst
#let data = json(bytes(sys.inputs.data))
= Invoice #data.number
```

```bash
# Chromium: render an HTML string
curl -sX POST localhost:8091/render \
  -H 'content-type: application/json' \
  -d '{"html":"<h1>Hello</h1>"}' -o hello.pdf

# Typst: data-driven invoice, no browser touched
curl -sX POST localhost:8091/render \
  -H 'content-type: application/json' \
  -d '{"template":"#let d=json(bytes(sys.inputs.data))\n= Invoice #d.number","data":{"number":"INV-7"}}' \
  -o invoice.pdf
```

> **Note:** the render service ships with **no auth** by default — run it on a
> trusted network or behind a gateway. (Bearer-auth + rate-limit, mirroring the
> `kite` server's `HttpGuard`, is a TODO.)

### CLI

```bash
# Chromium: HTML file → PDF, with a footer template + margins
kite-pdf render --html-file invoice.html --footer-file footer.html \
  --margin-top 20px --margin-bottom 40px -o invoice.pdf

# Typst: template + data → PDF (no browser)
kite-pdf render --template invoice.typ --data invoice.json -o invoice.pdf

# Run the HTTP service (also the default with no arguments)
kite-pdf serve
```

### Docker

```bash
# Full service (slim Debian + Chromium; both backends). Build from the repo root:
docker build -f crates/pdf/Dockerfile      -t kite-pdf      .
# Browser-free, distroless, Typst-only:
docker build -f crates/pdf/Dockerfile.lite -t kite-pdf-lite .
docker run -p 8091:8091 kite-pdf
```

### Honest comparison

- **vs [Gotenberg](https://gotenberg.dev):** kite-pdf is the lightest
  self-hosted HTML→PDF option — a single small binary, lazy browser lifecycle,
  reaped when idle. Gotenberg wins when you need **office-document conversion**
  (DOCX/XLSX/ODT via LibreOffice) and a batteries-included API; kite-pdf
  deliberately does **not** do office formats.
- **vs [react-pdf](https://react-pdf.org) / client PDF libs:** the Typst path is
  **browser-free and language-agnostic** — no Node runtime, no React, no
  headless Chrome — just a template + JSON from any language over HTTP. You give
  up React's component model in exchange for a far smaller, faster, reproducible
  typesetting pipeline.
- **Where it concedes:** no office-doc (DOCX/XLSX) conversion, and the Chromium
  backend still needs a browser at runtime (the Typst/`-lite` backend does not).

## Roadmap

- [x] `snapshot` — accessibility-tree snapshot (token-budgeted)
- [x] `click` / `type` / `press_key`
- [x] `wait_for` (selector / text polling)
- [x] `fill_form` / `select_option` / `hover` / `navigate_back` / `handle_dialog`
- [x] Storage state (`save_state` / `restore_state`) — reuse a login across sessions
- [x] Role/text selectors (`text=`, `role=…[name="…"]`) alongside CSS
- [x] `assert` — structured pass/fail primitive for agent-driven feature tests
- [x] Markdown (readability) extraction mode
- [x] `pdf` — print the current page to PDF (`Page.printToPDF`)
- [x] `kite-pdf` — standalone HTML/Typst → PDF service + CLI (dual-backend, three feature-gated build shapes)
- [x] Actionability auto-waiting (visible / enabled / unobstructed / stable) with cause-specific errors
- [x] `console` / `network` capture for debugging
- [x] `snapshot {diff}` — only what changed since the last snapshot
- [x] Element-handle primitives (`query` / `query_all` → `ElementRef`) — foundation for the npm facade
- [x] Per-MCP-session browser contexts (cookie isolation) instead of per-call pages
- [x] Bearer-token auth + rate limiting
- [x] `bindings/node`: napi-rs Puppeteer-compatible facade (`@kitewright/node`, experimental) — launch/newPage/createBrowserContext/setContent/evaluate/pdf/goto/close; HTML→PDF flow proven end-to-end
- [ ] `bindings/node`: prebuilt per-platform binaries + npm publish
- [ ] Python bindings (PyO3)
- [ ] Benchmarks vs playwright-mcp (cold start, RSS, image size) for the README
- [ ] Prebuilt release binaries (macOS/Linux/Windows) + `cargo binstall` + Homebrew tap

## Non-goals

Kitewright is deliberately a lean **agent tool**, not a QA test framework. It will not:

- **Support multiple browser engines.** CDP / Chromium only, by design — no Firefox or WebKit. Speaking one protocol to one engine is what keeps the binary tiny and the lifecycle simple.
- **Ship a test runner, fixtures, trace viewer, or video capture.** Those belong to QA frameworks (Playwright Test, Cypress). Kitewright gives an agent primitives (`snapshot`, `assert`, storage state); the agent — or a thin script — is the runner.
- **Expose an arbitrary-JS `eval` tool.** Executing agent- or model-authored JavaScript against live sessions (with restored cookies) is a security footgun. Selector resolution and helpers run curated, fixed JS only.

## License

MIT
