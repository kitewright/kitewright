// End-to-end proof that invoice-service's real Puppeteer flow works, unchanged,
// against @kitewright/node — a one-line import swap. Reproduces:
//   launch → createBrowserContext → newPage → setContent(networkidle0)
//   → evaluate(() => document.fonts.ready) → pdf({footer, margins, ...})
//   → page.close → context.close → browser.close
//
// Run with: BROWSER_EXECUTABLE=<chrome> node --test test/invoice.e2e.mjs

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

// The whole point: import the Puppeteer-shaped default export by package name.
import puppeteer from '@kitewright/node';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, '..', '..', '..');
const FOOTER = readFileSync(join(REPO_ROOT, 'testdata', 'invoice-footer.html'), 'utf8');
const OUT_DIR = join(__dirname, 'out');
const OUT_PDF = join(OUT_DIR, 'invoice.pdf');

// A representative static invoice: heading, company block, an itemized table
// with rows + totals, and a CSS background color (exercises printBackground).
const REAL_INVOICE_HTML = `<!doctype html>
<html><head><meta charset="utf-8"><title>Invoice INV-2026-0042</title>
<style>
  html, body { margin: 0; font-family: Arial, Helvetica, sans-serif; color: #1a1a1a; }
  .sheet { padding: 40px; }
  h1 { color: #0b5; margin: 0 0 8px; letter-spacing: 1px; }
  .company { background: #eef6ff; padding: 20px; border-radius: 8px; margin: 16px 0 8px; }
  .muted { color: #555; }
  table { width: 100%; border-collapse: collapse; margin-top: 28px; }
  thead th { background: #0b5; color: #fff; text-align: left; padding: 10px; }
  tbody td { padding: 10px; border-bottom: 1px solid #e2e2e2; }
  tbody tr:nth-child(even) { background: #fafafa; }
  .num { text-align: right; }
  .totals { margin-top: 20px; text-align: right; }
  .totals .grand { font-size: 18px; font-weight: 700; color: #0b5; }
</style></head>
<body><div class="sheet">
  <h1>INVOICE</h1>
  <div class="muted">Invoice #INV-2026-0042 · Date: 2026-07-11 · Due: 2026-07-25</div>
  <div class="company">
    <strong>Skuad Pte. Ltd.</strong><br>
    68 Circular Road, #02-01, Singapore 049422<br>
    billing@skuad.io
  </div>
  <table>
    <thead><tr><th>Description</th><th class="num">Qty</th><th class="num">Unit</th><th class="num">Amount</th></tr></thead>
    <tbody>
      <tr><td>Employer of Record — July 2026</td><td class="num">3</td><td class="num">$499.00</td><td class="num">$1,497.00</td></tr>
      <tr><td>Compliance &amp; payroll processing</td><td class="num">3</td><td class="num">$49.00</td><td class="num">$147.00</td></tr>
      <tr><td>Benefits administration</td><td class="num">3</td><td class="num">$29.00</td><td class="num">$87.00</td></tr>
      <tr><td>Onboarding (one-time)</td><td class="num">1</td><td class="num">$150.00</td><td class="num">$150.00</td></tr>
    </tbody>
  </table>
  <div class="totals">
    <div>Subtotal: $1,881.00</div>
    <div>Tax (0%): $0.00</div>
    <div class="grand">Total Due: $1,881.00</div>
  </div>
</div></body></html>`;

test('invoice-service Puppeteer flow produces a valid PDF via @kitewright/node', async () => {
  assert.ok(
    process.env.BROWSER_EXECUTABLE,
    'set BROWSER_EXECUTABLE to a chrome/chrome-headless-shell binary'
  );

  const browser = await puppeteer.launch({ headless: true, args: ['--no-sandbox'] });
  const context = await browser.createBrowserContext();
  const page = await context.newPage();

  await page.setContent(REAL_INVOICE_HTML, { waitUntil: 'networkidle0', timeout: 0 });

  // Promise-returning evaluate, exactly as invoice-service does.
  await page.evaluate(() => document.fonts.ready);

  // Sanity: the content is really in the page (setContent worked).
  const heading = await page.evaluate(() => document.querySelector('h1').textContent);
  assert.equal(heading, 'INVOICE');

  const buf = await page.pdf({
    format: 'a4',
    printBackground: true,
    displayHeaderFooter: true,
    headerTemplate: '',
    footerTemplate: FOOTER,
    margin: { top: '20px', bottom: '35px' },
    landscape: false,
  });

  await page.close();
  await context.close();
  await browser.close();

  // --- assertions ---
  assert.ok(Buffer.isBuffer(buf), 'pdf() must return a Buffer');
  assert.equal(buf.slice(0, 5).toString('latin1'), '%PDF-', 'must start with %PDF-');
  assert.ok(buf.length > 4096, `PDF too small: ${buf.length} bytes`);
  const tail = buf.slice(-1024).toString('latin1');
  assert.ok(tail.includes('%%EOF'), 'PDF must end with %%EOF trailer');
  // Valid structure: at least one page object.
  assert.ok(
    buf.toString('latin1').includes('/Type /Page') || buf.toString('latin1').includes('/Type/Page'),
    'PDF must contain a page object'
  );

  // Save for eyeballing.
  mkdirSync(OUT_DIR, { recursive: true });
  writeFileSync(OUT_PDF, buf);
  console.log(`wrote ${buf.length} bytes to ${OUT_PDF}`);
});
