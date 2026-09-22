import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { isAbsolute, join } from 'node:path';
import { test } from 'node:test';

const require = createRequire(import.meta.url);
const { RustBridge } = require('./bridge.cjs');
const binary = process.env.MINIMALPDF_RUST_ENGINE;

function textPdf(value) {
  const content = `BT /F1 12 Tf 72 720 Td (${value}) Tj ET`;
  const objects = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>',
    `<< /Length ${Buffer.byteLength(content)} >>\nstream\n${content}\nendstream`,
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>',
  ];
  let pdf = '%PDF-1.4\n';
  const offsets = [0];
  for (const [index, object] of objects.entries()) {
    offsets.push(Buffer.byteLength(pdf));
    pdf += `${index + 1} 0 obj\n${object}\nendobj\n`;
  }
  const xref = Buffer.byteLength(pdf);
  pdf += `xref\n0 ${offsets.length}\n0000000000 65535 f \n`;
  pdf += offsets.slice(1).map((offset) => `${String(offset).padStart(10, '0')} 00000 n \n`).join('');
  pdf += `trailer\n<< /Root 1 0 R /Size ${offsets.length} >>\nstartxref\n${xref}\n%%EOF\n`;
  return pdf;
}

test('Electron JSONL bridge converts a PDF using an isolated Rust engine', {
  skip: !binary || !isAbsolute(binary) || !existsSync(binary) ? 'set MINIMALPDF_RUST_ENGINE to a built absolute engine path' : false,
  timeout: 45_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), 'minimal-pdf-electron-integration-'));
    const bridge = new RustBridge(binary);
  let progressTimeout;
  try {
    const input = join(root, 'local sample.pdf');
    await writeFile(input, textPdf('electron bridge works'));
    const check = await bridge.invoke('get_self_check');
    assert.equal(check.status, 'ready');
    assert.deepEqual((await bridge.invoke('inspect_input_files', { paths: [input] })).map((file) => file.path), [await realpath(input)]);

    const finished = new Promise((resolve, reject) => {
      progressTimeout = setTimeout(() => reject(new Error('conversion progress timed out')), 30_000);
      bridge.on('progress', (progress) => {
        if (progress.state !== 'succeeded' && progress.state !== 'failed') return;
        clearTimeout(progressTimeout);
        resolve(progress);
      });
    });
    const jobId = await bridge.invoke('start_job', {
      request: { kind: 'pdf_to_txt', inputs: [input], outputDir: root, options: {} },
    });
    const result = await finished;
    assert.equal(result.job_id, jobId);
    assert.equal(result.state, 'succeeded', result.message);
    assert.equal(result.outputs.length, 1);
    assert.match(await readFile(result.outputs[0], 'utf8'), /electron bridge works/);
  } finally {
    clearTimeout(progressTimeout);
    await bridge.close();
    await rm(root, { recursive: true, force: true });
  }
});
