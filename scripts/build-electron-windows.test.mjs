import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { sha256 } from './check-windows-pdfium.mjs';
import { assertOfficeReleaseReady, assertStagedLayout, inspectBundledOffice } from './build-electron-windows.mjs';

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'minimal-pdf-electron-win-layout-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const engine = join(root, 'minimal-pdf-converter.exe');
  const runtime = join(root, 'pdfium-runtime');
  writeFileSync(engine, 'test engine');
  mkdirSync(runtime);
  const office = join(root, 'office');
  const officeSuite = join(office, 'LibreOffice');
  const officeProgram = join(officeSuite, 'program');
  mkdirSync(officeProgram, { recursive: true });
  for (const file of ['LICENSE', 'NOTICE']) writeFileSync(join(officeSuite, file), `${file} text`);
  const soffice = join(officeProgram, 'soffice.exe');
  writeFileSync(soffice, 'test office');
  return { root, engine, runtime, office, officeSuite, soffice };
}

test('Electron Windows stage keeps the verified PDFium runtime beside the exact Rust engine', (t) => {
  const { root, engine, runtime } = fixture(t);
  const inspected = [];
  const inspect = (...args) => inspected.push(args);
  assertStagedLayout(root, sha256(engine), 'signed', inspect);
  assert.deepEqual(inspected, [[runtime, 'signed']]);
  assert.equal(inspectBundledOffice(join(root, 'office')).hash, sha256(join(root, 'office', 'LibreOffice', 'program', 'soffice.exe')));
  assert.throws(() => assertStagedLayout(root, '0'.repeat(64), 'signed', inspect), /differs from the just-built/);
  writeFileSync(join(root, 'unapproved.dll'), 'x');
  assert.throws(() => assertStagedLayout(root, sha256(engine), 'signed', inspect), /unexpected entries/);
});

test('Windows Office package rejects missing, substituted, and symlinked runtimes', (t) => {
  const { root, office, officeSuite, soffice } = fixture(t);
  assert.equal(inspectBundledOffice(office, sha256(soffice)).executable, soffice);
  assert.throws(() => inspectBundledOffice(office, '0'.repeat(64)), /differs from the staged runtime/);
  rmSync(join(officeSuite, 'NOTICE'));
  assert.throws(() => inspectBundledOffice(office), /real, nonempty file/);
  writeFileSync(join(officeSuite, 'NOTICE'), 'notice');
  rmSync(soffice);
  assert.throws(() => inspectBundledOffice(office), /real, nonempty file/);
  writeFileSync(soffice, 'test office');
  const linkedOffice = join(root, 'linked-office');
  try {
    symlinkSync(office, linkedOffice, process.platform === 'win32' ? 'junction' : 'dir');
  } catch (error) {
    if (process.platform !== 'win32' || !['EPERM', 'EACCES'].includes(error.code)) throw error;
    t.diagnostic('Windows requires symlink privileges for the symlink rejection assertion');
    return;
  }
  assert.throws(() => inspectBundledOffice(linkedOffice), /missing or symlinked/);
});

test('Windows installer includes the Office runtime in unpacked resources', () => {
  const config = readFileSync(new URL('../electron-builder.yml', import.meta.url), 'utf8');
  const windows = config.split(/^win:\s*$/m)[1]?.split(/^nsis:\s*$/m)[0];
  assert.ok(windows, 'Windows builder configuration is required');
  assert.match(windows, /^  icon: public\/ayst-arc-mark\.png\s*$/m);
  assert.match(windows, /^    - from: electron\/bin\/windows\/office\n      to: office\s*$/m);
  assert.match(windows, /^    - from: electron\/bin\/windows\n      to: bin\s*$/m);
});

test('strict Windows release cannot bypass missing Office provenance and nested signatures', () => {
  assert.doesNotThrow(() => assertOfficeReleaseReady(false));
  assert.throws(() => assertOfficeReleaseReady(true), /official LibreOffice MSI provenance, nested signatures/);
});

test('Electron Windows staging refuses symlinked directories and executables', (t) => {
  const { root, engine, runtime } = fixture(t);
  const link = join(root, '..', `electron-win-layout-link-${Date.now()}`);
  t.after(() => rmSync(link, { force: true }));
  symlinkSync(root, link);
  assert.throws(() => assertStagedLayout(link, sha256(engine), 'source', () => {}), /real directory/);
  rmSync(engine);
  symlinkSync(join(runtime, 'pdfium.dll'), engine);
  assert.throws(() => assertStagedLayout(root, '0'.repeat(64), 'source', () => {}), /real, nonempty file/);
});

test('Windows NSIS builder cannot claim verification on macOS', {
  skip: process.platform === 'win32',
}, () => {
  assert.throws(() => execFileSync(process.execPath, ['scripts/build-electron-windows.mjs'], {
    encoding: 'utf8', stdio: 'pipe', timeout: 10_000,
  }), (error) => error.status === 2 && /Windows x64 build host/.test(error.stderr));
});
