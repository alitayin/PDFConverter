import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { deflateSync } from 'node:zlib';
import { existsSync } from 'node:fs';
import { mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const binaryName = process.platform === 'win32' ? 'pdf_to_txt_worker.exe' : 'pdf_to_txt_worker';
const args = process.argv.slice(2);
const keepFixtures = args.includes('--keep');
const workerArg = args.indexOf('--worker');
if (args.some((arg, index) => arg !== '--keep' && arg !== '--worker' && (workerArg < 0 || index !== workerArg + 1)) ||
    (workerArg !== -1 && !args[workerArg + 1])) {
  throw new Error('Usage: node scripts/quality-smoke.mjs [--worker /absolute/path] [--keep]');
}
if (workerArg !== -1 && !isAbsolute(args[workerArg + 1])) {
  throw new Error('--worker requires an absolute path');
}
const workerPath = resolve(workerArg === -1
  ? process.env.PDF_TO_TXT_WORKER ?? join(repoRoot, 'src-tauri', 'target', 'debug', binaryName)
  : args[workerArg + 1]);
if (!existsSync(workerPath)) {
  throw new Error(`Worker not found: ${workerPath}. Build with cargo build --locked --manifest-path src-tauri/Cargo.toml --bin pdf_to_txt_worker`);
}

function literal(value) {
  assert.match(value, /^[\x00-\x7f]*$/);
  return `(${value.replace(/[\\()]/g, '\\$&')})`;
}

function utf16(value) {
  return `<FEFF${Buffer.from(value, 'utf16le').swap16().toString('hex').toUpperCase()}>`;
}

function text(value) {
  return `BT /F1 12 Tf 72 720 Td ${literal(value)} Tj ET`;
}

function streamObject(spec) {
  const source = Buffer.isBuffer(spec.content) ? spec.content : Buffer.from(spec.content, 'ascii');
  const encoded = spec.filter === 'FlateDecode' && !spec.raw
    ? deflateSync(source)
    : spec.filter === 'ASCIIHexDecode' && !spec.raw
      ? Buffer.from(`${source.toString('hex')}>`, 'ascii')
      : source;
  const dictionary = `<< /Length ${spec.declaredLength ?? encoded.length}${spec.filter ? ` /Filter /${spec.filter}` : ''}${spec.dictionary ? ` ${spec.dictionary}` : ''} >>\nstream\n`;
  return Buffer.concat([Buffer.from(dictionary), encoded, Buffer.from('\nendstream')]);
}

// These are intentionally tiny, independently generated PDF objects with a real xref table.
function makePdf(pages, { toUnicode, extraObjects = [], form } = {}) {
  let nextId = 3;
  const entries = pages.map((page) => ({
    pageId: nextId++,
    streams: (Array.isArray(page) ? page : [page]).map((spec) => ({
      id: nextId++,
      spec: typeof spec === 'string' ? { content: spec } : spec
    }))
  }));
  const fontId = nextId++;
  const cmapId = toUnicode ? nextId++ : undefined;
  const formId = form ? nextId++ : undefined;
  const extraIds = extraObjects.map(() => nextId++);
  const objects = new Map([
    [1, Buffer.from('<< /Type /Catalog /Pages 2 0 R >>')],
    [2, Buffer.from(`<< /Type /Pages /Kids [${entries.map((entry) => `${entry.pageId} 0 R`).join(' ')}] /Count ${entries.length} >>`)],
    [fontId, Buffer.from(`<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding${cmapId ? ` /ToUnicode ${cmapId} 0 R` : ''} >>`)]
  ]);
  for (const entry of entries) {
    const contents = entry.streams.length === 1
      ? `${entry.streams[0].id} 0 R`
      : `[${entry.streams.map((stream) => `${stream.id} 0 R`).join(' ')}]`;
    objects.set(entry.pageId, Buffer.from(`<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 ${fontId} 0 R >>${formId ? ` /XObject << /F0 ${formId} 0 R >>` : ''} >> /Contents ${contents} >>`));
    for (const stream of entry.streams) objects.set(stream.id, streamObject(stream.spec));
  }
  if (cmapId) objects.set(cmapId, streamObject({ content: toUnicode }));
  if (formId) objects.set(formId, streamObject({
    content: form.content,
    filter: form.filter,
    dictionary: `/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /Font << /FormFont ${fontId} 0 R >> >>`
  }));
  extraObjects.forEach((object, index) => objects.set(extraIds[index], object));

  const chunks = [Buffer.from('%PDF-1.7\n')];
  const offsets = [0];
  let length = chunks[0].length;
  for (let id = 1; id < nextId; id++) {
    const object = Buffer.concat([Buffer.from(`${id} 0 obj\n`), objects.get(id), Buffer.from('\nendobj\n')]);
    offsets[id] = length;
    chunks.push(object);
    length += object.length;
  }
  const xref = length;
  chunks.push(Buffer.from(`xref\n0 ${nextId}\n0000000000 65535 f \n${offsets.slice(1).map((offset) => `${String(offset).padStart(10, '0')} 00000 n \n`).join('')}trailer\n<< /Root 1 0 R /Size ${nextId} >>\nstartxref\n${xref}\n%%EOF\n`));
  return Buffer.concat(chunks);
}

function makeIndexedPdf({ corruptIndex = false, incremental = false } = {}) {
  const values = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /Contents 4 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >>'
  ];
  const offsets = values.map((_, index) => values.slice(0, index).join(' ').length + (index > 0 ? 1 : 0));
  const header = Buffer.from(values.map((_, index) => `${index + 1} ${offsets[index]}`).join(' ') + ' ');
  const objectStream = deflateSync(Buffer.concat([header, Buffer.from(values.join(' '))]));
  const content = Buffer.from(text('modern'));
  const chunks = [Buffer.from('%PDF-1.7\n')];
  let length = chunks[0].length;
  const positions = new Map();
  function addObject(id, body) {
    positions.set(id, length);
    const object = Buffer.concat([Buffer.from(`${id} 0 obj\n`), body, Buffer.from('\nendobj\n')]);
    chunks.push(object);
    length += object.length;
  }
  addObject(4, Buffer.concat([Buffer.from(`<< /Length ${content.length} >>\nstream\n`), content, Buffer.from('\nendstream')]));
  addObject(5, Buffer.concat([Buffer.from(`<< /Type /ObjStm /N 3 /First ${header.length} /Length ${objectStream.length} /Filter /FlateDecode >>\nstream\n`), objectStream, Buffer.from('\nendstream')]));
  positions.set(6, length);

  const index = Buffer.alloc(7 * 7);
  index.writeUInt16BE(65535, 5);
  for (let id = 1; id <= 3; id++) {
    index.writeUInt8(2, id * 7);
    index.writeUInt32BE(5, id * 7 + 1);
    index.writeUInt16BE(corruptIndex && id === 3 ? 1 : id - 1, id * 7 + 5);
  }
  for (const id of [4, 5, 6]) {
    index.writeUInt8(1, id * 7);
    index.writeUInt32BE(positions.get(id), id * 7 + 1);
  }
  const compressedIndex = deflateSync(index);
  addObject(6, Buffer.concat([
    Buffer.from(`<< /Type /XRef /Root 1 0 R /Size 7 /W [1 4 2]${incremental ? ' /Prev 9' : ''} /Filter /FlateDecode /Length ${compressedIndex.length} >>\nstream\n`),
    compressedIndex,
    Buffer.from('\nendstream')
  ]));
  chunks.push(Buffer.from(`startxref\n${positions.get(6)}\n%%EOF\n`));
  return Buffer.concat(chunks);
}

function appendIndexedRevision(input, updatedText, { freeContent = false, omitRoot = false } = {}) {
  const marker = Buffer.from('startxref\n');
  const markerOffset = input.lastIndexOf(marker);
  assert.notEqual(markerOffset, -1);
  const offsetStart = markerOffset + marker.length;
  const offsetEnd = input.indexOf(10, offsetStart);
  const previous = Number(input.subarray(offsetStart, offsetEnd).toString('ascii'));
  assert.ok(Number.isSafeInteger(previous));

  const content = Buffer.from(text(updatedText));
  const replacement = freeContent
    ? Buffer.alloc(0)
    : Buffer.concat([
        Buffer.from(`4 0 obj\n<< /Length ${content.length} >>\nstream\n`),
        content,
        Buffer.from('\nendstream\nendobj\n')
      ]);
  const xrefOffset = input.length + replacement.length;
  const rows = Buffer.alloc(14);
  if (freeContent) {
    rows.writeUInt16BE(1, 5);
  } else {
    rows.writeUInt8(1, 0);
    rows.writeUInt32BE(input.length, 1);
  }
  rows.writeUInt8(1, 7);
  rows.writeUInt32BE(xrefOffset, 8);
  const xref = deflateSync(rows);
  const revision = Buffer.concat([
    Buffer.from(`8 0 obj\n<< /Type /XRef${omitRoot ? '' : ' /Root 1 0 R'} /Size 9 /W [1 4 2] /Index [4 1 8 1] /Prev ${previous} /Filter /FlateDecode /Length ${xref.length} >>\nstream\n`),
    xref,
    Buffer.from(`\nendstream\nendobj\nstartxref\n${xrefOffset}\n%%EOF\n`)
  ]);
  return Buffer.concat([input, replacement, revision]);
}

function appendTableRevision(input, updatedText, { freeContent = false, trailerExtra = '' } = {}) {
  const marker = Buffer.from('startxref\n');
  const markerOffset = input.lastIndexOf(marker);
  assert.notEqual(markerOffset, -1);
  const offsetStart = markerOffset + marker.length;
  const offsetEnd = input.indexOf(10, offsetStart);
  const previous = Number(input.subarray(offsetStart, offsetEnd).toString('ascii'));
  assert.ok(Number.isSafeInteger(previous) && previous > 0);

  const content = Buffer.from(text(updatedText));
  const replacement = freeContent
    ? Buffer.alloc(0)
    : Buffer.concat([
        Buffer.from('4 0 obj\n'),
        streamObject({ content }),
        Buffer.from('\nendobj\n')
      ]);
  const row = freeContent
    ? '0000000000 00001 f \n'
    : `${String(input.length).padStart(10, '0')} 00000 n \n`;
  const xrefOffset = input.length + replacement.length;
  const revision = Buffer.from(`xref\n4 1\n${row}trailer\n<< /Size 6 /Root 1 0 R /Prev ${previous}${trailerExtra} >>\nstartxref\n${xrefOffset}\n%%EOF\n`);
  return Buffer.concat([input, replacement, revision]);
}

const shortPdf = makePdf([text('one')]);
const threePages = makePdf([text('first'), text('second'), text('third')]);
const cases = [
  { name: 'english-basic', pdf: shortPdf, expected: 'one' },
  { name: 'explicit-double-space', pdf: makePdf([text('two  spaces')]), expected: 'two  spaces' },
  { name: 'split-tj-word', pdf: makePdf(['BT /F1 12 Tf 72 720 Td (Dumm) Tj (y) Tj ET']), expected: 'Dummy' },
  { name: 'tj-array-fragments', pdf: makePdf(['BT /F1 12 Tf 72 720 Td [(H) (e) (l) (l) (o)] TJ ET']), expected: 'Hello' },
  { name: 'tj-array-explicit-space', pdf: makePdf(['BT /F1 12 Tf 72 720 Td [(Hello ) 140 (world)] TJ ET']), expected: 'Hello world' },
  { name: 'escaped-parentheses', pdf: makePdf([text('read (this)')]), expected: 'read (this)' },
  { name: 'winansi-currency', pdf: makePdf(['BT /F1 12 Tf 72 720 Td <802095> Tj ET']), expected: '\u20ac \u2022' },
  { name: 'utf16-chinese', pdf: makePdf([`BT /F1 12 Tf 72 720 Td ${utf16('\u4f60\u597d')} Tj ET`]), expected: '\u4f60\u597d' },
  { name: 'adjacent-cjk-fragments', pdf: makePdf([`BT /F1 12 Tf 72 720 Td ${utf16('\u4f60')} Tj ${utf16('\u597d')} Tj ET`]), expected: '\u4f60\u597d' },
  { name: 'utf16-japanese', pdf: makePdf([`BT /F1 12 Tf 72 720 Td ${utf16('\u65e5\u672c\u8a9e')} Tj ET`]), expected: '\u65e5\u672c\u8a9e' },
  { name: 'utf16-korean', pdf: makePdf([`BT /F1 12 Tf 72 720 Td ${utf16('\ud55c\uad6d\uc5b4')} Tj ET`]), expected: '\ud55c\uad6d\uc5b4' },
  { name: 'to-unicode-map', pdf: makePdf(['BT /F1 12 Tf 72 720 Td <0102> Tj ET'], { toUnicode: '1 begincodespacerange <00> <FF> endcodespacerange 2 beginbfchar <01> <4F60> <02> <597D> endbfchar' }), expected: '\u4f60\u597d' },
  { name: 'form-local-cjk-cmap', pdf: makePdf(['/F0 Do'], { toUnicode: '1 begincodespacerange <0000> <FFFF> endcodespacerange 2 beginbfchar <0001> <4F60> <0002> <597D> endbfchar', form: { content: 'BT /FormFont 12 Tf 72 720 Td <00010002> Tj ET', filter: 'FlateDecode' } }), expected: '\u4f60\u597d' },
  { name: 'form-page-reading-order', pdf: makePdf(['BT /F1 12 Tf 72 720 Td (before) Tj ET /F0 Do BT /F1 12 Tf 72 700 Td (after) Tj ET'], { form: { content: 'BT /FormFont 12 Tf 72 720 Td (inside) Tj ET' } }), expected: 'before\ninside\nafter' },
  { name: 'star-line-break', pdf: makePdf(['BT /F1 12 Tf 72 720 Td (first) Tj T* (second) Tj ET']), expected: 'first\nsecond' },
  { name: 'vertical-text-move', pdf: makePdf(['BT /F1 12 Tf 72 720 Td (top) Tj 0 -24 Td (bottom) Tj ET']), expected: 'top\nbottom' },
  { name: 'repeated-relative-text-moves', pdf: makePdf(['BT /F1 12 Tf 54 748 Td (first) Tj 0 -13 Td (second) Tj 0 -13 Td (third) Tj ET']), expected: 'first\nsecond\nthird' },
  { name: 'independent-text-object-baselines', pdf: makePdf(['BT /F1 12 Tf 54 748 Td (first) Tj ET BT /F1 12 Tf 54 735 Td (second) Tj ET BT /F1 12 Tf 54 735 Td ( and) Tj ET BT /F1 12 Tf 54 722 Td (third) Tj ET']), expected: 'first\nsecond and\nthird' },
  { name: 'multiple-content-streams', pdf: makePdf([[text('part one'), text('part two')]]), expected: 'part one\npart two' },
  { name: 'two-page-separator', pdf: makePdf([text('front'), text('back')]), expected: 'front\n\nback' },
  { name: 'page-range', pdf: threePages, pages: '2-3', expected: 'second\n\nthird' },
  { name: 'discontiguous-pages', pdf: threePages, pages: '1,3', expected: 'first\n\nthird' },
  { name: 'flate-text-stream', pdf: makePdf([{ content: text('compressed'), filter: 'FlateDecode' }]), expected: 'compressed' },
  { name: 'ascii-hex-stream', pdf: makePdf([{ content: text('hex stream'), filter: 'ASCIIHexDecode' }]), expected: 'hex stream' },
  { name: 'table-incremental-updated', pdf: appendTableRevision(makePdf([text('original')]), 'updated'), expected: 'updated' },
  { name: 'table-incremental-latest-wins', pdf: appendTableRevision(appendTableRevision(makePdf([text('original')]), 'intermediate'), 'latest'), expected: 'latest' },
  { name: 'table-incremental-freed-content', pdf: appendTableRevision(appendTableRevision(makePdf([text('original')]), 'intermediate'), '', { freeContent: true }), error: 'CORRUPTED_PDF' },
  { name: 'table-incremental-hybrid-marker', pdf: appendTableRevision(makePdf([text('original')]), 'latest', { trailerExtra: ' /XRefStm 9' }), error: 'UNSUPPORTED_PDF_STRUCTURE' },
  { name: 'indexed-compressed-page', pdf: makeIndexedPdf(), expected: 'modern' },
  { name: 'indexed-incremental-updated', pdf: appendIndexedRevision(makeIndexedPdf(), 'updated'), expected: 'updated' },
  { name: 'indexed-incremental-freed-content', pdf: appendIndexedRevision(makeIndexedPdf(), 'unused', { freeContent: true }), error: 'CORRUPTED_PDF' },
  { name: 'indexed-incremental-missing-root', pdf: appendIndexedRevision(makeIndexedPdf(), 'unused', { omitRoot: true }), error: 'CORRUPTED_PDF' },
  { name: 'indexed-invalid-object', pdf: makeIndexedPdf({ corruptIndex: true }), error: 'CORRUPTED_PDF' },
  { name: 'indexed-invalid-prev-object', pdf: makeIndexedPdf({ incremental: true }), error: 'UNSUPPORTED_PDF_STRUCTURE' },
  { name: 'no-text-layer', pdf: makePdf(['q 0 0 20 20 re f Q']), error: 'NO_TEXT_LAYER' },
  { name: 'selected-page-without-text', pdf: makePdf([text('visible'), 'q 0 0 20 20 re f Q']), pages: '2', error: 'NO_TEXT_LAYER' },
  { name: 'invalid-page-range', pdf: shortPdf, pages: '3-1', error: 'INVALID_PAGES' },
  { name: 'page-outside-document', pdf: shortPdf, pages: '2', error: 'INVALID_PAGES' },
  { name: 'invalid-pdf-header', pdf: Buffer.from('not a PDF\n'), error: 'CORRUPTED_PDF' },
  { name: 'empty-page-tree', pdf: makePdf([]), error: 'CORRUPTED_PDF' },
  { name: 'unsupported-filter', pdf: makePdf([{ content: text('unsupported'), filter: 'LZWDecode' }]), error: 'CONVERSION_FAILED' },
  { name: 'broken-flate-stream', pdf: makePdf([{ content: 'not zlib data', filter: 'FlateDecode', raw: true }]), error: 'CONVERSION_FAILED' },
  { name: 'object-stream', pdf: makePdf([text('valid')], { extraObjects: [streamObject({ content: '', dictionary: '/Type /ObjStm /N 0 /First 0' })] }), error: 'UNSUPPORTED_PDF_STRUCTURE' },
  { name: 'unsupported-worker-kind', pdf: shortPdf, kind: 'pdf_to_image', error: 'UNSUPPORTED_FORMAT' },
  { name: 'unsupported-protocol', pdf: shortPdf, protocol: 2, error: 'WORKER_PROTOCOL_UNSUPPORTED' },
  { name: 'missing-input', pdf: shortPdf, missingInput: true, error: 'INPUT_NOT_FOUND' },
  { name: 'relative-input', pdf: shortPdf, input: 'relative.pdf', error: 'PATH_NOT_ALLOWED' },
  { name: 'output-write-failure', pdf: shortPdf, outputIsDirectory: true, error: 'OUTPUT_WRITE_FAILED' },
  { name: 'invalid-json-request', pdf: shortPdf, rawRequest: '{invalid', error: 'INVALID_WORKER_REQUEST' }
];

function runWorker(payload, { timeoutMs = 5000, signal } = {}) {
  return new Promise((resolveWorker, rejectWorker) => {
    const child = spawn(workerPath, [], { stdio: ['pipe', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    let stopped;
    const stop = (reason) => {
      if (!stopped) {
        stopped = reason;
        child.kill(reason === 'WORKER_TIMEOUT' ? 'SIGKILL' : 'SIGTERM');
      }
    };
    const onAbort = () => stop('WORKER_CANCELLED');
    signal?.addEventListener('abort', onAbort, { once: true });
    const timeout = setTimeout(() => stop('WORKER_TIMEOUT'), timeoutMs);
    child.stdout.setEncoding('utf8');
    child.stderr.setEncoding('utf8');
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
      if (stdout.length > 1_000_000) stop('WORKER_OUTPUT_TOO_LARGE');
    });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('error', rejectWorker);
    child.on('close', (exitCode) => {
      clearTimeout(timeout);
      signal?.removeEventListener('abort', onAbort);
      if (stopped) {
        const error = new Error(stopped);
        error.code = stopped;
        rejectWorker(error);
      } else if (exitCode !== 0) {
        rejectWorker(new Error(`Worker exited ${exitCode}: ${stderr.trim()}`));
      } else {
        resolveWorker(stdout.trim().split('\n').map((line) => JSON.parse(line)));
      }
    });
    if (signal?.aborted) onAbort();
    // Leaving stdin open is used only to test the harness's process stop/timeout behavior.
    if (payload !== undefined) child.stdin.end(`${typeof payload === 'string' ? payload : JSON.stringify(payload)}\n`);
  });
}

async function runCase(item, index, root) {
  const input = join(root, `${item.name}.pdf`);
  const output = item.outputIsDirectory ? root : join(root, `${item.name}.txt`);
  await writeFile(input, item.pdf);
  const jobId = `quality_${index + 1}`;
  const request = item.rawRequest ?? {
    protocol: item.protocol ?? 1,
    job_id: jobId,
    kind: item.kind ?? 'pdf_to_txt',
    input: item.input ?? (item.missingInput ? join(root, 'missing.pdf') : input),
    output,
    options: { pages: item.pages ?? '\u5168\u90e8' }
  };
  const events = await runWorker(request);
  assert.ok(events.length > 0, 'worker must emit a JSONL event');
  for (const [eventIndex, event] of events.entries()) {
    assert.equal(event.seq, eventIndex + 1, 'events must have consecutive sequence numbers');
    assert.equal(event.total, 1);
    assert.equal(event.job_id, item.rawRequest ? 'unknown' : jobId);
  }
  const final = events.at(-1);
  if (item.error) {
    assert.equal(final.state, 'failed');
    assert.equal(final.error_code, item.error);
    assert.equal(final.completed, 0);
    if (item.outputIsDirectory) assert.equal((await stat(output)).isDirectory(), true);
    else assert.equal(existsSync(output), false, 'failure left an output file');
  } else {
    assert.deepEqual(events.map((event) => event.phase), ['reading', 'extracting', 'writing', 'done']);
    assert.equal(final.state, 'succeeded');
    assert.equal(final.completed, 1);
    assert.deepEqual(final.outputs, [output]);
    assert.equal(await readFile(output, 'utf8'), item.expected);
  }
}

async function main() {
  const root = await mkdtemp(join(tmpdir(), 'minimal-pdf-quality-'));
  let passed = 0;
  try {
    for (const [index, item] of cases.entries()) {
      try {
        await runCase(item, index, root);
        passed++;
        console.log(`ok ${passed} - ${item.name}`);
      } catch (error) {
        console.error(`not ok - ${item.name}: ${error.stack ?? error}`);
        process.exitCode = 1;
      }
    }
    try {
      const controller = new AbortController();
      const pending = runWorker(undefined, { signal: controller.signal });
      setTimeout(() => controller.abort(), 30);
      await assert.rejects(pending, { code: 'WORKER_CANCELLED' });
      passed++;
      console.log(`ok ${passed} - process-cancel-while-waiting`);
      await assert.rejects(runWorker(undefined, { timeoutMs: 120 }), { code: 'WORKER_TIMEOUT' });
      passed++;
      console.log(`ok ${passed} - process-timeout-while-waiting`);
    } catch (error) {
      console.error(`not ok - process control: ${error.stack ?? error}`);
      process.exitCode = 1;
    }
    console.log(`${passed}/${cases.length + 2} checks passed`);
  } finally {
    if (keepFixtures) console.log(`Fixtures retained: ${root}`);
    else await rm(root, { recursive: true, force: true });
  }
}

await main();
