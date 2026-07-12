// @kitewright/node — a Puppeteer-compatible (experimental) facade over the
// Rust kitewright-engine, loaded as a napi-rs native addon.
//
// This thin JS layer wraps the native classes so that:
//   * `page.evaluate(fn)` accepts a real JS function (Puppeteer style): it is
//     stringified and run in the page, and its JSON result is parsed back.
//   * `page.pdf()` returns a Node Buffer.
//   * unsupported Puppeteer APIs throw a clear, actionable error instead of
//     silently doing nothing (see the compatibility matrix in README.md).
//
// One-line migration: `import puppeteer from '@kitewright/node'` in place of
// `import puppeteer from 'puppeteer'` for the launch → setContent → pdf flow.

'use strict';

const path = require('path');

// Single-platform local build: load the addon directly. (A published package
// would resolve a per-platform prebuilt here.)
const native = require(path.join(__dirname, 'kitewright-node.node'));

function unsupported(name, hint) {
  return function () {
    throw new Error(
      `@kitewright/node: ${name} is not supported by the Puppeteer-compatible ` +
        `facade.${hint ? ' ' + hint : ''} Supported: launch, newPage, ` +
        `createBrowserContext, setContent, evaluate, pdf, goto, close.`
    );
  };
}

class Page {
  constructor(inner) {
    this._inner = inner;
  }

  /** Puppeteer page.setContent(html, { waitUntil, timeout }). */
  async setContent(html, options) {
    return this._inner.setContent(String(html), options || null);
  }

  /**
   * Puppeteer page.evaluate(fnOrString, ...args). A function is serialized to
   * source and invoked in the page; a string is evaluated as an expression.
   * Promise-returning bodies (e.g. `() => document.fonts.ready`) are awaited.
   */
  async evaluate(fn, ...args) {
    let expression;
    if (typeof fn === 'function') {
      const argList = args
        .map((a) => JSON.stringify(a === undefined ? null : a))
        .join(',');
      expression = `(${fn.toString()})(${argList})`;
    } else {
      expression = String(fn);
    }
    const json = await this._inner.evaluate(expression);
    if (json === '' || json === 'null' || json === undefined) return undefined;
    return JSON.parse(json);
  }

  /** Puppeteer page.pdf(options) → Buffer. */
  async pdf(options) {
    return this._inner.pdf(options || null);
  }

  /** Puppeteer page.goto(url, options). */
  async goto(url, options) {
    return this._inner.goto(String(url), options || null);
  }

  /** Puppeteer page.close(). */
  async close() {
    return this._inner.close();
  }
}

// Common Puppeteer Page methods we do NOT support: fail loudly.
Page.prototype.setViewport = unsupported('page.setViewport', 'Viewport/emulation is not implemented.');
Page.prototype.setRequestInterception = unsupported('page.setRequestInterception', 'Request interception is not implemented.');
Page.prototype.emulate = unsupported('page.emulate', 'Device emulation is not implemented.');
Page.prototype.emulateMediaType = unsupported('page.emulateMediaType');
Page.prototype.tracing = unsupported('page.tracing', 'Tracing is not implemented.');
Page.prototype.screenshot = unsupported('page.screenshot', 'Use the kite MCP browser_screenshot tool instead.');
Page.prototype.waitForSelector = unsupported('page.waitForSelector');
Page.prototype.addScriptTag = unsupported('page.addScriptTag');
Page.prototype.exposeFunction = unsupported('page.exposeFunction');

class BrowserContext {
  constructor(inner) {
    this._inner = inner;
  }

  /** Puppeteer context.newPage(). */
  async newPage() {
    return new Page(this._inner.newPage());
  }

  /** Puppeteer context.close(). */
  async close() {
    return this._inner.close();
  }
}

class Browser {
  constructor(inner) {
    this._inner = inner;
  }

  /** Puppeteer browser.newPage(). */
  async newPage() {
    return new Page(this._inner.newPage());
  }

  /** Puppeteer browser.createBrowserContext() (per-invoice isolation). */
  async createBrowserContext() {
    return new BrowserContext(this._inner.createBrowserContext());
  }

  /** Legacy alias kept for older Puppeteer callers. */
  async createIncognitoBrowserContext() {
    return this.createBrowserContext();
  }

  /** Puppeteer browser.close(). */
  async close() {
    return this._inner.close();
  }
}

Browser.prototype.pages = unsupported('browser.pages');
Browser.prototype.userAgent = unsupported('browser.userAgent');
Browser.prototype.version = unsupported('browser.version');

/** Puppeteer puppeteer.launch({ headless, args, executablePath }) → Browser. */
async function launch(options) {
  const inner = await native.launch(options || null);
  return new Browser(inner);
}

/** Puppeteer puppeteer.connect() is not supported. */
const connect = unsupported('puppeteer.connect', 'Connecting to a remote browser is not implemented.');

const puppeteer = { launch, connect, Browser, BrowserContext, Page };

module.exports = puppeteer;
module.exports.default = puppeteer;
module.exports.launch = launch;
module.exports.connect = connect;
module.exports.Browser = Browser;
module.exports.BrowserContext = BrowserContext;
module.exports.Page = Page;
