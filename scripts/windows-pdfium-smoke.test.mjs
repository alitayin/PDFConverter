import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, mkdirSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { buildWindowsRenderFixture, buildWorkerRequest, verifyInstalledPdfium } from './windows-pdfium-smoke.mjs';

test('self-made Windows PDF quality fixture has a valid xref and page assets', () => {
  const pdf = buildWindowsRenderFixture();
  const text = pdf.toString('latin1');
  const xrefMatch = text.match(/startxref\n(\d+)\n%%EOF\n$/);
  assert.ok(xrefMatch);
  const xref = Number(xrefMatch[1]);
  assert.ok(text.slice(xref).startsWith('xref\n0 11\n'));
  const rows = text.slice(xref).split('\n').slice(3, 13);
  assert.equal(rows.length, 10);
  for (const [index, entry] of rows.entries()) {
    const offset = Number(entry.slice(0, 10));
    assert.ok(text.slice(offset).startsWith(`${index + 1} 0 obj\n`));
  }
  assert.ok(text.includes('/Subtype /Image /Width 2 /Height 2'));
  assert.ok(text.includes('/ExtGState /ca 0.5'));
  assert.ok(text.includes('/Rotate 90'));
  assert.ok(text.includes('/MediaBox [0 0 160 100] /CropBox [20 10 120 90]'));
  assert.ok(text.includes('20 10 m 120 90 l 120 10 l h f'));
  assert.ok(pdf.includes(Buffer.from([0, 255, 0, 0, 255, 0])));
});

test('installed-app smoke refuses to stage a missing PDFium runtime', (t) => {
  const root = mkdtempSync(join(tmpdir(), 'minimal-pdf-installed-smoke-test-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const installDir = join(root, 'installed');
  const source = join(root, 'source');
  mkdirSync(installDir);
  mkdirSync(source);
  const app = join(installDir, 'minimal-pdf-converter.exe');
  writeFileSync(app, 'test executable');
  writeFileSync(join(source, 'pdfium.dll'), 'unapproved fixture');

  assert.throws(() => verifyInstalledPdfium(app, source), /installed PDFium runtime is missing/);
  assert.equal(existsSync(join(installDir, 'pdfium-runtime')), false);
  assert.deepEqual(readdirSync(installDir), ['minimal-pdf-converter.exe']);
});

test('Windows worker smoke request includes bounded, JSON-safe required limits', () => {
  const request = buildWorkerRequest('C:\\input.pdf', 'C:\\output', { pages: [1], dpi: 150, format: 'png' });
  const encoded = JSON.stringify(request);
  const decoded = JSON.parse(encoded);
  assert.deepEqual(decoded.pages, [1]);
  assert.equal(decoded.max_selected_pages, 10_000);
  assert.equal(decoded.max_output_bytes, Number.MAX_SAFE_INTEGER);
  assert.ok(Number.isSafeInteger(decoded.max_output_bytes));
  assert.ok(decoded.max_output_bytes > 512 * 1024 * 1024);
});
