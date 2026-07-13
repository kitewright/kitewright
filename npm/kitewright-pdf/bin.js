#!/usr/bin/env node
"use strict";
// Launcher for the kite-pdf render service / CLI. Resolves the prebuilt
// `kite-pdf` binary for this platform (shipped as an optional per-platform
// dependency, the esbuild/napi pattern) and execs it, passing all args through.
// With no args, kite-pdf starts its HTTP render service (loopback by default);
// pass `render …` for the CLI. See `npx @kitewright/pdf --help`.
const { spawnSync } = require("node:child_process");

const PKG_BY_TARGET = {
  "darwin-arm64": "@kitewright/pdf-darwin-arm64",
  "darwin-x64": "@kitewright/pdf-darwin-x64",
  "linux-x64": "@kitewright/pdf-linux-x64-gnu",
  "win32-x64": "@kitewright/pdf-win32-x64-msvc",
};

function resolveBinary() {
  if (process.env.KITE_PDF_BINARY) return process.env.KITE_PDF_BINARY;
  const target = `${process.platform}-${process.arch}`;
  const exe = process.platform === "win32" ? "kite-pdf.exe" : "kite-pdf";
  const pkg = PKG_BY_TARGET[target];
  if (pkg) {
    try {
      return require.resolve(`${pkg}/${exe}`);
    } catch {
      /* optional dep not installed for this platform — fall through */
    }
  }
  return exe; // last resort: a `kite-pdf` already on PATH
}

const bin = resolveBinary();
const res = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });

if (res.error) {
  const target = `${process.platform}-${process.arch}`;
  process.stderr.write(
    `@kitewright/pdf: could not launch the kite-pdf binary (${bin}): ${res.error.message}\n` +
      (PKG_BY_TARGET[target]
        ? `The prebuilt package for ${target} may have failed to install. `
        : `No prebuilt binary is published for ${target}. `) +
      `Set KITE_PDF_BINARY=/path/to/kite-pdf, or build from source ` +
      `(cargo install --git https://github.com/kitewright/kitewright kite-pdf).\n`,
  );
  process.exit(1);
}
process.exit(res.status == null ? 1 : res.status);
