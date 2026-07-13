# @kitewright/mcp

Run the [kitewright](https://github.com/kitewright/kitewright) browser-automation
MCP server with **npx** — no build step, no cargo. A tiny launcher that resolves
the prebuilt `kite` binary for your platform and starts it over stdio.

## Use it

**Claude Code**

```bash
claude mcp add kitewright -- npx -y @kitewright/mcp
```

**Cursor / any MCP client** (`mcp.json`)

```jsonc
{
  "mcpServers": {
    "kitewright": {
      "command": "npx",
      "args": ["-y", "@kitewright/mcp"]
    }
  }
}
```

That's it — `npx @kitewright/mcp` with no args starts a stdio MCP server. Pass
flags through for other modes (e.g. `npx @kitewright/mcp --http` for the HTTP
transport).

## How it works

The prebuilt `kite` binary ships as an optional per-platform dependency
(`@kitewright/mcp-darwin-arm64`, `-darwin-x64`, `-linux-x64-gnu`,
`-win32-x64-msvc`) — npm installs only the one matching your OS/arch, the same
pattern esbuild and napi use. The launcher `require.resolve`s it and execs it.

- Override the binary with `KITE_BINARY=/path/to/kite` (local dev / custom build).
- If no prebuilt package covers your platform, install from source:
  `cargo install --git https://github.com/kitewright/kitewright kitewright`.

## Config

All kitewright env vars apply (`KITE_HEADLESS`, `KITE_IDLE_TIMEOUT_SECS`,
`KITE_ALLOW_SECRET_FILES`, …) — see the main
[README](https://github.com/kitewright/kitewright#readme).
