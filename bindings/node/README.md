# @kitewright/node (Puppeteer-compatible, experimental)

A [napi-rs](https://napi.rs) native addon that puts a **Puppeteer-shaped**
facade over the Rust `kitewright-engine` crate — native in-process bindings, no
Node driver process. Session management, CDP, and waiting heuristics all run in
the shared Rust core; the JS layer (`index.js`) is thin.

It targets the common **HTML→PDF** use case (a service that uses Puppeteer only
for rendering, like invoice-service). For that flow it is a one-line import swap:

```js
import puppeteer from '@kitewright/node'; // was: from 'puppeteer'

const browser = await puppeteer.launch({ headless: true, args: ['--no-sandbox'] });
const context = await browser.createBrowserContext();
const page = await context.newPage();
await page.setContent(html, { waitUntil: 'networkidle0', timeout: 0 });
await page.evaluate(() => document.fonts.ready);
const pdf = await page.pdf({
  format: 'a4', printBackground: true, displayHeaderFooter: true,
  footerTemplate: legalFooterHtml, margin: { top: '20px', bottom: '35px' },
}); // → Node Buffer
await page.close(); await context.close(); await browser.close();
```

## Supported / not supported

**Supported:** `launch` · `browser.newPage` / `createBrowserContext` / `close` ·
`context.newPage` / `close` · `page.setContent` (waitUntil `load` /
`domcontentloaded` / `networkidle0`) · `page.evaluate` (function or string,
including promise-returning bodies) · `page.pdf` (full option set: `format`,
`landscape`, `printBackground`, `displayHeaderFooter`, `header/footerTemplate`,
`margin`, `scale`, `preferCssPageSize`) · `page.goto` · `page.close`.

**Not supported (throws a clear error):** request interception, tracing,
device/viewport emulation, `page.screenshot`, `waitForSelector`, `addScriptTag`,
`exposeFunction`, `puppeteer.connect`. `--no-sandbox` is honored; other Chromium
`args` are ignored; `headless:false` is ignored (always headless).
`networkidle0` is approximated as load + a short settle.

## Build & test (local, this platform)

Built separately from the core cargo workspace (listed under `[workspace]
exclude`), so the Rust CI (`cargo clippy --workspace`, engine/server tests) never
touches the napi toolchain.

```bash
npm install
npx napi build --release --js binding.js --dts binding.d.ts   # → kitewright-node.node
BROWSER_EXECUTABLE=/path/to/chrome-headless-shell node --test test/invoice.e2e.mjs
```

The e2e test reproduces invoice-service's real flow and writes the rendered PDF
to `test/out/invoice.pdf` for eyeballing.

## Status

Experimental, local build only. Prebuilt per-platform binaries + npm publish are
on the roadmap. The `.node` addon and generated `binding.d.ts` are build
artifacts (see the repo `.gitignore`).
