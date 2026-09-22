import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { lstat, mkdtemp, readFile, readdir, rm, symlink, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { test } from 'node:test';

const require = createRequire(import.meta.url);
const electron = require('electron');
const fixture = fileURLToPath(new URL('./offline-print.fixture.cjs', import.meta.url));

async function runElectron(input, output, mode = 'success') {
  return new Promise((resolve, reject) => {
    const child = spawn(electron, [fixture, input, output, mode], {
      env: { ...process.env, ELECTRON_RUN_AS_NODE: '' },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    const timeout = setTimeout(() => child.kill('SIGKILL'), 40_000);
    child.stdout.on('data', (chunk) => { stdout += chunk.toString().slice(0, 4096); });
    child.stderr.on('data', (chunk) => { stderr += chunk.toString().slice(0, 4096); });
    child.once('error', reject);
    child.once('close', (exitCode) => {
      clearTimeout(timeout);
      resolve({ exitCode, stdout, stderr });
    });
  });
}

async function withFiles(html, callback) {
  const root = await mkdtemp(join(tmpdir(), 'minimalpdf-offline-print-test-'));
  const input = join(root, 'input.html');
  const output = join(root, 'converted.pdf');
  try {
    await writeFile(input, html);
    return await callback({ root, input, output });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

async function assertNoPartials(root) {
  const entries = await readdir(root);
  assert.ok(entries.every((entry) => !entry.startsWith('.minimalpdf-html-')), entries.join(', '));
}

test('Chromium prints CSS to a real PDF without embedding source HTML in the title', { timeout: 50_000 }, async () => {
  const embeddedPng = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLttAAAAABJRU5ErkJggg==';
  await withFiles(`<!doctype html><html><head><style>h1{color:rgb(189,20,20)}@page{margin:20mm}</style></head><body><h1>Offline layout proof</h1><p>Text remains selectable.</p><img width="40" height="40" alt="embedded image" src="data:image/png;base64,${embeddedPng}"></body></html>`, async ({ root, input, output }) => {
    const result = await runElectron(input, output);
    assert.equal(result.exitCode, 0, result.stderr);
    assert.match(result.stdout, /HTML_PRINT_RESULT=/);
    const pdf = await readFile(output);
    assert.equal(pdf.subarray(0, 5).toString('ascii'), '%PDF-');
    assert.match(pdf.toString('latin1'), /\/Count 1\b/);
    assert.match(pdf.toString('latin1'), /\/Title \(Local HTML\)/);
    assert.match(pdf.toString('latin1'), /\/Subtype \/Image\b/);
    assert.doesNotMatch(pdf.toString('latin1'), /data:text\/html/);
    await assertNoPartials(root);
  });
});

test('external CSS, image, frame, script and file URL never reach network', { timeout: 50_000 }, async () => {
  let hits = 0;
  const server = createServer((_request, response) => {
    hits += 1;
    response.writeHead(200, { 'content-type': 'text/html' });
    response.end('Network access must stay blocked');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  try {
    const url = `http://127.0.0.1:${server.address().port}`;
    await withFiles('', async ({ root, input, output }) => {
      const secret = join(root, 'private.html');
      await writeFile(secret, '<h1>Never render local file resources</h1>');
      await writeFile(input, `<!doctype html><html><head><style>@import url('${url}/import.css');</style></head><body><img src="${url}/image.png"><iframe src="${url}/frame"></iframe><iframe src="${pathToFileURL(secret).href}"></iframe><script>document.title='SCRIPT_EXECUTED';fetch('${url}/script');document.write('SCRIPT EXECUTED')</script><p>Still prints offline.</p></body></html>`);
      const result = await runElectron(input, output);
      assert.equal(result.exitCode, 0, result.stderr);
      assert.equal(hits, 0, 'network requests escaped the isolated print session');
      const pdf = await readFile(output);
      assert.equal(pdf.subarray(0, 5).toString('ascii'), '%PDF-');
      assert.match(pdf.toString('latin1'), /\/Title \(Local HTML\)/, 'inline JavaScript changed the document title');
      await assertNoPartials(root);
    });
  } finally {
    server.close();
  }
});

test('pre-existing output is never overwritten and temporary output is cleaned', { timeout: 50_000 }, async () => {
  await withFiles('<h1>Do not overwrite</h1>', async ({ root, input, output }) => {
    await writeFile(output, 'existing file');
    const result = await runElectron(input, output);
    assert.equal(result.exitCode, 1);
    assert.match(result.stderr, /EEXIST/);
    assert.equal(await readFile(output, 'utf8'), 'existing file');
    await assertNoPartials(root);
  });
});

test('oversized and non-PDF renderer output is rejected without partials', { timeout: 90_000 }, async () => {
  await withFiles('<h1>Wrong PDF</h1>', async ({ root, input, output }) => {
    for (const mode of ['oversized-output', 'invalid-output']) {
      const result = await runElectron(input, output, mode);
      assert.equal(result.exitCode, 1, mode);
      assert.match(result.stderr, /HTML_PRINT_INVALID_PDF/);
      assert.equal((await readdir(root)).includes('converted.pdf'), false);
      await assertNoPartials(root);
    }
  });
});

test('oversized and invalid UTF-8 HTML inputs are rejected', { timeout: 90_000 }, async () => {
  await withFiles(Buffer.alloc(4 * 1024 * 1024 + 1, 65), async ({ root, input, output }) => {
    const oversized = await runElectron(input, output);
    assert.equal(oversized.exitCode, 1);
    assert.match(oversized.stderr, /HTML_INPUT_TOO_LARGE_OR_INVALID/);
    await writeFile(input, Buffer.from([0xc3, 0x28]));
    const invalid = await runElectron(input, output);
    assert.equal(invalid.exitCode, 1);
    assert.equal((await readdir(root)).includes('converted.pdf'), false);
    await assertNoPartials(root);
  });
});

test('symbolic-link input is rejected before Chromium opens it', { timeout: 50_000 }, async () => {
  await withFiles('<h1>Safe source</h1>', async ({ root, input, output }) => {
    const real = join(root, 'real.html');
    const alias = join(root, 'alias.html');
    await writeFile(real, '<h1>Do not follow</h1>');
    await rm(input);
    await symlink(real, alias);
    const result = await runElectron(alias, output);
    assert.equal(result.exitCode, 1);
    assert.match(result.stderr, /HTML_INPUT_TOO_LARGE_OR_INVALID/);
    assert.equal((await readdir(root)).includes('converted.pdf'), false);
    assert.equal((await lstat(alias)).isSymbolicLink(), true);
    await assertNoPartials(root);
  });
});

test('cancellation, timeout and retry never leave failed output', { timeout: 110_000 }, async () => {
  await withFiles('<h1>Cancellation and retry</h1>', async ({ root, input, output }) => {
    for (const mode of ['abort-before', 'abort-during', 'timeout']) {
      const result = await runElectron(input, output, mode);
      assert.equal(result.exitCode, 1, `${mode}: ${result.stderr}`);
      assert.match(result.stderr, /HTML_PRINT_(?:CANCELLED|TIMEOUT)/);
      assert.equal((await readdir(root)).includes('converted.pdf'), false);
      await assertNoPartials(root);
    }
    const retry = await runElectron(input, output, 'retry');
    assert.equal(retry.exitCode, 0, retry.stderr);
    assert.equal((await readFile(output)).subarray(0, 5).toString('ascii'), '%PDF-');
    await assertNoPartials(root);
  });
});
