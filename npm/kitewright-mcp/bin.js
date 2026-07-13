#!/usr/bin/env node
"use strict";
// Launcher for the kitewright MCP server. Resolves the prebuilt `kite` binary
// for this platform (shipped as an optional per-platform dependency, the same
// pattern esbuild/napi use) and execs it, passing through all args. With no
// args it defaults to `--stdio` so `npx kitewright-mcp` starts a stdio MCP
// server — the form MCP clients (Claude Code, Cursor) launch.
const { spawnSync } = require("node:child_process");

// platform-arch -> optional dependency package that ships the binary
const PKG_BY_TARGET = {
  "darwin-arm64": "@kitewright/mcp-darwin-arm64",
  "darwin-x64": "@kitewright/mcp-darwin-x64",
  "linux-x64": "@kitewright/mcp-linux-x64-gnu",
  "win32-x64": "@kitewright/mcp-win32-x64-msvc",
};

function resolveBinary() {
  // Explicit override wins (local dev / custom builds).
  if (process.env.KITE_BINARY) return process.env.KITE_BINARY;

  const target = `${process.platform}-${process.arch}`;
  const exe = process.platform === "win32" ? "kite.exe" : "kite";
  const pkg = PKG_BY_TARGET[target];
  if (pkg) {
    try {
      return require.resolve(`${pkg}/${exe}`);
    } catch {
      /* optional dep not installed for this platform — fall through */
    }
  }
  // Last resort: a `kite` already on PATH (e.g. `cargo install`).
  return exe;
}

const args = process.argv.slice(2);
if (args.length === 0) args.push("--stdio");

const bin = resolveBinary();
const res = spawnSync(bin, args, { stdio: "inherit" });

if (res.error) {
  const target = `${process.platform}-${process.arch}`;
  process.stderr.write(
    `kitewright-mcp: could not launch the kite binary (${bin}): ${res.error.message}\n` +
      (PKG_BY_TARGET[target]
        ? `The prebuilt package for ${target} may have failed to install. `
        : `No prebuilt binary is published for ${target}. `) +
      `Set KITE_BINARY=/path/to/kite, or install it with \`cargo install --git https://github.com/kitewright/kitewright kitewright\`.\n`,
  );
  process.exit(1);
}
process.exit(res.status == null ? 1 : res.status);
