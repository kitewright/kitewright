# @kitewright/pdf

Run the [kitewright](https://github.com/kitewright/kitewright) **HTML/Typst → PDF**
render service + CLI with **npx** — no build step, no cargo. A tiny launcher that
resolves the prebuilt `kite-pdf` binary for your platform.

## Use it

```bash
# HTTP render service (loopback by default):
npx -y @kitewright/pdf
# POST http://127.0.0.1:8091/render  { "html": "<h1>hi</h1>" }  → application/pdf

# one-shot CLI:
npx -y @kitewright/pdf render --html '<h1>Invoice</h1>' -o out.pdf
```

## Security defaults (network service)

- **Binds loopback** (`127.0.0.1:8091`). Set `KITE_PDF_BIND=0.0.0.0` to expose —
  it then **requires `KITE_PDF_AUTH_TOKEN`** (or `KITE_PDF_INSECURE=1`) or it
  refuses to start, because the endpoint fetches URLs on your behalf.
- **SSRF-filtered**: caller `url`s to loopback/private/link-local/metadata hosts
  are blocked (`KITE_PDF_ALLOW_PRIVATE_IPS=1` for trusted networks).
- **Concurrency-capped** (`KITE_PDF_MAX_CONCURRENCY`, default 4).

## How it works

The prebuilt `kite-pdf` binary ships as an optional per-platform dependency
(`@kitewright/pdf-darwin-arm64`, …); npm installs only the one for your OS/arch.
Override with `KITE_PDF_BINARY=/path/to/kite-pdf`. The Chromium backend uses a
system Chromium (or `BROWSER_EXECUTABLE`); the Typst backend is self-contained.
