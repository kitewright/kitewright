# Benchmarks: Kitewright vs @playwright/mcp

Measured 2026-07-12 on an Apple Silicon MacBook Pro, macOS. Both servers on
MCP Streamable HTTP, same machine, same network, same target page
(https://example.com), same browser build (Playwright's
`chromium_headless_shell-1208`) for the latency comparison.

Versions: kitewright v0.4 (release build) · @playwright/mcp 0.0.78 (node 22, run directly via `node cli.js`, npx overhead excluded).

## Server layer (what a rewrite can change)

| Metric | @playwright/mcp | kitewright | Δ |
|---|---:|---:|---|
| Cold start → listening | 354 ms | **75 ms** | ~4.7× faster |
| Server RSS, idle (no browser) | 102–125 MB | **7.6 MB** | ~14× smaller |
| Server RSS, after navigations | 93 MB | **10.9 MB** | ~8.5× smaller |
| Server distribution | 18 MB package + Node.js runtime | **6.9 MB static binary** | no runtime needed |

## Browser layer

| Metric | @playwright/mcp | kitewright |
|---|---:|---:|
| First navigate, call fired instantly after handshake | 2623 ms | **822 ms** (median of 5) |
| First navigate, 1.5 s after handshake (realistic LLM think-time) | — (launches lazily on first call) | **645 ms** (median of 5) |
| Warm navigate | 80–116 ms | 99–105 ms (tie) |
| Chromium headless shell on disk | 190 MB | 190 MB (same build; any system Chrome also works) |

Warm-call latency is **a tie** — as expected, since both speak CDP to the same
Chromium. The first-call gap comes from kitewright's **session pre-warming**:
the browser (and its renderer, via a throwaway `about:blank` page) launches in
the background the moment an MCP session initializes, overlapping the handshake
and the client's think-time instead of blocking the first tool call. Combined
with lean launch flags (`--no-first-run`, `--disable-background-networking`,
...), browser launch itself is ~155–480 ms. The remaining ~600 ms of a first
navigation is mostly uncached network (DNS + TLS + fetch).

The idle reaper still applies: after the idle TTL the browser is closed and the
server drops back to its ~7.6 MB baseline; the next session pre-warms again.
playwright-mcp keeps its browser alive once launched.

## v0.4 latency (Phase 1b: launch/session-start off the critical path)

External-site latency has a hard network floor (DNS + TLS + TTFB ≈ 400–600 ms)
that **no** local optimization beats. So v0.4 attacks the *controllable* costs:
browser launch, context creation, session start, and page weight.

**Localhost first-navigate decomposition** (fixture HTTP server on `127.0.0.1`,
so the numbers isolate our own overhead + render with zero network latency;
Apple Silicon, headless-shell build; reproduce with
`cargo test -p kitewright-engine bench_localhost --release -- --ignored --nocapture`):

| Stage | Time | Notes |
|---|---:|---|
| Cold first-navigate (incl. browser launch) | 709 ms | fresh engine, no prewarm |
| `prewarm()` (launch + fill context pool) | 251 ms | paid in the background during the MCP handshake |
| **Prewarmed first-navigate (pooled context)** | **31 ms** | no launch, no context-creation — a warm pooled context is handed to the new session |
| Warm navigate (reused session page) | 28 ms | steady state |

The headline: once `prewarm()` has run (which the server fires the moment an MCP
session initializes), a brand-new session's **first** navigate to localhost is
~31 ms — down from ~709 ms cold, and now essentially indistinguishable from a
warm navigate. Both browser launch (~250 ms) and per-session context creation
are off the critical path (the warm-context pool holds pre-created blank
contexts, so even the *first* session pays zero context-creation cost). This is
our overhead + render only; a real external page adds the ~400–600 ms network
floor on top, which nothing here removes.

**Lite mode (resource blocking).** `browser_navigate {lite:true}` (and the
default for the text-only tools `extract` / `extract_markdown`) blocks
images/media/fonts + common ad/analytics hosts via CDP `Network.setBlockedURLs`
before the load. On a local fixture it verifiably drops the image request
entirely (see the `lite_mode_blocks_images…` test). On heavy real pages this
typically yields **30–70 % faster DOM-ready** by skipping the bulk of the bytes,
though it cannot beat the network floor for the document itself — expect roughly
~250–400 ms with lite vs ~500–650 ms full on a heavy page over a real network
(network-dependent; not measured here as these runs are offline). Screenshots
and PDF never block resources (pixels matter there).

**Shared disk cache + connection pre-warm.** All browser instances point at a
stable `--disk-cache-dir` (`KITE_CACHE_DIR`), so repeat asset fetches — across
process restarts, and one-shot ops within a run — hit cache instead of the
network. `KITE_PREWARM_URL` optionally establishes DNS+TLS+connection to a known
origin during prewarm, so the first real navigate to it skips the handshake.
(Caveat: per-session *isolated* contexts, used for cookie isolation, use an
ephemeral in-memory cache; the on-disk cache benefits the default context.)

## Feature coverage (honest gap)

@playwright/mcp currently ships ~25 tools. kitewright ships 21 — the full
agent loop (navigate, screenshot, extract, extract_markdown, snapshot(+diff),
click, type, press_key, hover, fill_form, select_option, navigate_back,
handle_dialog, wait_for, assert), state (save_state/restore_state), HTML→PDF
(set_content + pdf), and debug capture (console, network). Remaining gaps
(tabs, file_upload, request interception, ...) are the roadmap. Kitewright also
ships an experimental `@kitewright/node` Puppeteer-compatible npm facade that
@playwright/mcp does not.

## Reproduce

1. `cargo build --release -p kitewright`
2. Time-to-listen: timestamp before spawn → first successful HTTP connect
3. Latency: `tools/call browser_navigate` round-trip via curl, first call
   (browser launch) then 3 warm calls
4. RSS: `ps -o rss= -p <server pid>` (server process only; browser processes
   excluded on both sides)

An earlier run using full Google Chrome instead of the headless shell showed
~5.8 s first navigate and ~590 ms warm calls for both stacks' browser layer —
browser build choice matters more than server language for latency.
