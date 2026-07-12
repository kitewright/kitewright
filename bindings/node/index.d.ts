// Type declarations for the @kitewright/node Puppeteer-compatible facade.
// The native option/result shapes are generated in ./binding.d.ts.

export interface LaunchOptions {
  /** Accepted for compatibility; kitewright always runs headless. */
  headless?: boolean;
  /** Chromium args. Only `--no-sandbox` is honored. */
  args?: string[];
  /** Path to a Chrome/Chromium/chrome-headless-shell binary. */
  executablePath?: string;
}

export interface SetContentOptions {
  /** "load" (default) | "domcontentloaded" | "networkidle0". */
  waitUntil?: 'load' | 'domcontentloaded' | 'networkidle0' | string;
  /** Accepted for compatibility; the engine time-boxes internally. */
  timeout?: number;
}

export interface GotoOptions {
  waitUntil?: string;
  timeout?: number;
}

export interface PdfMargin {
  top?: string;
  bottom?: string;
  left?: string;
  right?: string;
}

export interface PdfOptions {
  /** "a4" (default) | "letter" | "legal" | "a3". */
  format?: string;
  landscape?: boolean;
  printBackground?: boolean;
  displayHeaderFooter?: boolean;
  headerTemplate?: string;
  footerTemplate?: string;
  margin?: PdfMargin;
  scale?: number;
  preferCssPageSize?: boolean;
}

export class Page {
  setContent(html: string, options?: SetContentOptions): Promise<void>;
  evaluate<T = unknown>(fn: (...args: any[]) => T | Promise<T>, ...args: any[]): Promise<T>;
  evaluate<T = unknown>(script: string): Promise<T>;
  pdf(options?: PdfOptions): Promise<Buffer>;
  goto(url: string, options?: GotoOptions): Promise<void>;
  close(): Promise<void>;
}

export class BrowserContext {
  newPage(): Promise<Page>;
  close(): Promise<void>;
}

export class Browser {
  newPage(): Promise<Page>;
  createBrowserContext(): Promise<BrowserContext>;
  createIncognitoBrowserContext(): Promise<BrowserContext>;
  close(): Promise<void>;
}

export function launch(options?: LaunchOptions): Promise<Browser>;

declare const puppeteer: {
  launch: typeof launch;
  Browser: typeof Browser;
  BrowserContext: typeof BrowserContext;
  Page: typeof Page;
};

export default puppeteer;
