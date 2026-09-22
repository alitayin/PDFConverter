import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import { audit } from './real-quality-check.mjs';

let root;
let corpus;
let resultsRoot;
let manifest;
let records;
let examples;

function sha(data) {
  return createHash('sha256').update(data).digest('hex');
}

async function put(base, name, bytes) {
  await writeFile(join(base, name), bytes);
  return { file: name, sha256: sha(bytes) };
}

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

// Builds a small, structurally valid stored ZIP solely for checker unit tests.
function storedZip(entries) {
  const localParts = [];
  const centralParts = [];
  let offset = 0;
  for (const [name, value] of entries) {
    const nameBytes = Buffer.from(name, 'utf8');
    const data = Buffer.isBuffer(value) ? value : Buffer.from(value);
    const checksum = crc32(data);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(0x0800, 6);
    local.writeUInt16LE(0, 8);
    local.writeUInt32LE(checksum, 14);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);

    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(0x0800, 8);
    central.writeUInt16LE(0, 10);
    central.writeUInt32LE(checksum, 16);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt32LE(offset, 42);

    localParts.push(local, nameBytes, data);
    centralParts.push(central, nameBytes);
    offset += local.length + nameBytes.length + data.length;
  }
  const centralDirectory = Buffer.concat(centralParts);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(centralDirectory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...localParts, centralDirectory, end]);
}

function packageBytes(format, mimeOverride) {
  const odfMime = {
    odt: 'application/vnd.oasis.opendocument.text',
    odp: 'application/vnd.oasis.opendocument.presentation',
    ods: 'application/vnd.oasis.opendocument.spreadsheet'
  }[format];
  if (odfMime) {
    return storedZip([
      ['mimetype', mimeOverride ?? odfMime],
      ['content.xml', '<office:document-content/>'],
      ['META-INF/manifest.xml', '<manifest:manifest/>']
    ]);
  }
  const mainPart = {
    docx: 'word/document.xml',
    pptx: 'ppt/presentation.xml',
    xlsx: 'xl/workbook.xml'
  }[format];
  return storedZip([
    ['[Content_Types].xml', '<Types/>'],
    [mainPart, '<document/>']
  ]);
}

function epubBytes() {
  return storedZip([
    ['mimetype', 'application/epub+zip'],
    ['META-INF/container.xml', '<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>'],
    ['OEBPS/content.opf', '<package version="3.0"><manifest/><spine/></package>'],
    ['OEBPS/nav.xhtml', '<html xmlns="http://www.w3.org/1999/xhtml"><body><nav/></body></html>'],
    ['OEBPS/page-0001.xhtml', '<html xmlns="http://www.w3.org/1999/xhtml"><body>page 1</body></html>'],
    ['OEBPS/page-0002.xhtml', '<html xmlns="http://www.w3.org/1999/xhtml"><body>page 2</body></html>']
  ]);
}

function tiffBytes(marker = 0) {
  const entries = [
    [256, 4, 1, 1], [257, 4, 1, 1], [258, 3, 1, 8], [259, 3, 1, 1],
    [262, 3, 1, 1], [273, 4, 1, 122], [277, 3, 1, 1], [278, 4, 1, 1], [279, 4, 1, 1]
  ];
  const bytes = Buffer.alloc(123);
  bytes.write('II', 0);
  bytes.writeUInt16LE(42, 2);
  bytes.writeUInt32LE(8, 4);
  bytes.writeUInt16LE(entries.length, 8);
  entries.forEach(([tag, type, count, value], index) => {
    const offset = 10 + index * 12;
    bytes.writeUInt16LE(tag, offset);
    bytes.writeUInt16LE(type, offset + 2);
    bytes.writeUInt32LE(count, offset + 4);
    if (type === 3) bytes.writeUInt16LE(value, offset + 8);
    else bytes.writeUInt32LE(value, offset + 8);
  });
  bytes.writeUInt32LE(0, 118);
  bytes[122] = marker;
  return bytes;
}

function flatOdtXml(body = '<text:p>Agreement</text:p>', rootAttributes = '') {
  return `<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
 office:mimetype="application/vnd.oasis.opendocument.text" office:version="1.3" ${rootAttributes}>
 <office:body><office:text>${body}</office:text></office:body>
</office:document>`;
}

function cloned(value) {
  return structuredClone(value);
}

function review(pages = 1, outputPages = pages) {
  return {
    reviewer: 'Unit test', reviewedAt: '2026-09-19', accepted: true,
    layoutSeverity: 0, notes: 'Synthetic visual fixture only',
    sourcePages: pages, outputPages, pagesReviewed: Math.max(pages, outputPages),
    sourceImages: examples.screenshots.slice(0, pages),
    resultImages: examples.screenshots.slice(0, outputPages)
  };
}

function visualCase(format, kind, input, category, outputs, pages = 1) {
  const testManifest = cloned(manifest);
  const document = testManifest.documents[0];
  Object.assign(document, { id: `sample-${format}`, format, categories: [category], input });
  delete document.textReference;

  const testRecords = cloned(records);
  const row = testRecords.runs[0].results[0];
  Object.assign(row, {
    documentId: document.id, kind, inputSha256: input.sha256, outputs,
    review: review(pages)
  });
  delete row.outputText;
  delete row.outputTextMethod;
  return [testManifest, testRecords];
}

function addTextEvidence(document, row) {
  document.textReference = examples.reference;
  row.outputText = examples.outputText;
  row.outputTextMethod = 'Independent synthetic fixture';
  row.review.textOrderPass = true;
}

function completePdfCase() {
  const testManifest = cloned(manifest);
  const document = testManifest.documents[0];
  Object.assign(document, {
    id: 'sample-pdf', format: 'pdf', categories: ['contract'],
    input: examples.pdf, textReference: examples.reference
  });
  const testRecords = cloned(records);
  const template = testRecords.runs[0].results[0];
  const outputs = {
    pdf_to_docx: [examples.docxOutput],
    pdf_to_odt: [examples.odtOutput],
    pdf_to_rtf: [examples.rtfOutput],
    pdf_to_flat_odt_xml: [examples.flatOdtXmlOutput],
    pdf_to_txt: [examples.outputText],
    pdf_to_markdown: [examples.markdownOutput],
    pdf_to_epub: [examples.epubOutput],
    pdf_to_png: examples.pngPages,
    pdf_to_jpg: examples.jpgPages,
    pdf_to_bmp: examples.bmpPages,
    pdf_to_gif: examples.gifPages,
    pdf_to_webp: examples.webpPages,
    pdf_to_tiff: examples.tiffPages,
    pdf_to_pptx: [examples.pptxOutput],
    pdf_to_odp: [examples.odpOutput],
    pdf_to_cbz: [examples.cbzOutput],
    pdf_to_html: [examples.htmlZipOutput]
  };
  testRecords.runs[0].results = Object.entries(outputs).map(([kind, files]) => {
    const row = cloned(template);
    Object.assign(row, {
      documentId: document.id, kind, inputSha256: document.input.sha256,
      outputs: cloned(files), review: review(2, kind === 'pdf_to_txt' ? 1 : 2)
    });
    delete row.outputText;
    delete row.outputTextMethod;
    if (['pdf_to_docx', 'pdf_to_odt', 'pdf_to_rtf', 'pdf_to_flat_odt_xml', 'pdf_to_txt', 'pdf_to_markdown', 'pdf_to_epub', 'pdf_to_odp'].includes(kind)) {
      addTextEvidence(document, row);
    }
    return row;
  });
  return [testManifest, testRecords];
}

before(async () => {
  root = await mkdtemp(join(tmpdir(), 'real-quality-check-unit-'));
  corpus = join(root, 'corpus');
  resultsRoot = join(root, 'results');
  await mkdir(corpus);
  await mkdir(resultsRoot);

  const referenceText = 'Agreement 你好。Dates: 01/02/2026.';
  const reference = await put(corpus, 'reference.txt', referenceText);
  const permission = await put(corpus, 'permission.txt', 'Local synthetic QA fixture only.');
  const output = await put(resultsRoot, 'output.pdf', '%PDF-1.7\n% synthetic fixture, not a real PDF\n');
  const outputText = await put(resultsRoot, 'output.txt', referenceText);
  const measurementEvidence = await put(resultsRoot, 'metrics.json', JSON.stringify({
    startedAt: '2026-09-19T00:00:00.000Z', finishedAt: '2026-09-19T00:00:00.100Z',
    method: 'Synthetic unit-test data, not OS measurements', rssSamplesMiB: [10, 20, 19]
  }));
  const firstPng = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLttAAAAABJRU5ErkJggg==', 'base64');
  const secondPng = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+Z6i8AAAAASUVORK5CYII=', 'base64');
  const screenshot = await put(resultsRoot, 'page.png', firstPng);
  const secondScreenshot = await put(resultsRoot, 'page-2.png', secondPng);
  const ui = await put(resultsRoot, 'ui.png', firstPng);
  const pdf = await put(corpus, 'sample.pdf', '%PDF-1.7\n% synthetic fixture\n');
  const png = await put(corpus, 'sample.png', firstPng);
  const jpeg = await put(corpus, 'sample.jpeg', Buffer.from('ffd8ffd9', 'hex'));
  const tiffInput = await put(corpus, 'sample.tiff', tiffBytes(2));
  const txtInput = await put(corpus, 'notes.txt', referenceText);
  const rtfInput = await put(corpus, 'notes.rtf', `{\\rtf1\\ansi ${referenceText}}\r\n`);
  const htmlInput = await put(corpus, 'notes.html', `<!doctype html><html><body><p>${referenceText}</p></body></html>`);
  const invalidHtmlInput = await put(corpus, 'bad.html', Buffer.from([0xff, 0xfe]));

  const docxInput = await put(corpus, 'document.docx', packageBytes('docx'));
  const odtInput = await put(corpus, 'document.odt', packageBytes('odt'));
  const pptxInput = await put(corpus, 'slides.pptx', packageBytes('pptx'));
  const odpInput = await put(corpus, 'slides.odp', packageBytes('odp'));
  const xlsxInput = await put(corpus, 'sheet.xlsx', packageBytes('xlsx'));
  const odsInput = await put(corpus, 'sheet.ods', packageBytes('ods'));
  const docxOutput = await put(resultsRoot, 'converted.docx', packageBytes('docx'));
  const odtOutput = await put(resultsRoot, 'converted.odt', packageBytes('odt'));
  const rtfOutput = await put(resultsRoot, 'converted.rtf', '{\\rtf1\\ansi synthetic unit fixture}\r\n');
  const flatOdtXmlOutput = await put(resultsRoot, 'converted.xml', flatOdtXml());
  const markdownOutput = await put(resultsRoot, 'converted.md', `<!-- page: 1 -->\n\n${referenceText}\n`);
  const epubOutput = await put(resultsRoot, 'converted.epub', epubBytes());
  const brokenEpub = await put(resultsRoot, 'broken.epub', storedZip([
    ['mimetype', 'application/epub+zip'],
    ['META-INF/container.xml', '<container/>'],
    ['OEBPS/content.opf', '<package/>'],
    ['OEBPS/nav.xhtml', '<html/>']
  ]));
  const pptxOutput = await put(resultsRoot, 'converted.pptx', packageBytes('pptx'));
  const odpOutput = await put(resultsRoot, 'converted.odp', packageBytes('odp'));

  const firstJpg = Buffer.from('ffd8ffd9', 'hex');
  const secondJpg = Buffer.from('ffd8ff00ffd9', 'hex');
  const pngPages = [screenshot, secondScreenshot];
  const jpgPages = [
    await put(resultsRoot, 'page-1.jpg', firstJpg),
    await put(resultsRoot, 'page-2.jpg', secondJpg)
  ];
  const makeBmp = (marker) => {
    const value = Buffer.alloc(58, marker);
    value.write('BM', 0);
    value.writeUInt32LE(value.length, 2);
    value.writeUInt32LE(54, 10);
    return value;
  };
  const bmpPages = [
    await put(resultsRoot, 'page-1.bmp', makeBmp(0)),
    await put(resultsRoot, 'page-2.bmp', makeBmp(1))
  ];
  const bmpInput = await put(corpus, 'sample.bmp', makeBmp(3));
  const svgInput = await put(corpus, 'drawing.svg', '<svg xmlns="http://www.w3.org/2000/svg" width="120" height="80"><rect width="120" height="80" fill="red"/></svg>');
  const firstGif = Buffer.concat([Buffer.from('GIF89a'), Buffer.from([0, 0, 0x3b])]);
  const secondGif = Buffer.concat([Buffer.from('GIF87a'), Buffer.from([1, 0, 0x3b])]);
  const gifInput = await put(corpus, 'sample.gif', firstGif);
  const gifPages = [
    await put(resultsRoot, 'page-1.gif', firstGif),
    await put(resultsRoot, 'page-2.gif', secondGif)
  ];
  const makeWebp = (marker) => {
    const value = Buffer.alloc(20, marker);
    value.write('RIFF', 0);
    value.writeUInt32LE(value.length - 8, 4);
    value.write('WEBP', 8);
    value.write('VP8X', 12);
    return value;
  };
  const webpPages = [
    await put(resultsRoot, 'page-1.webp', makeWebp(0)),
    await put(resultsRoot, 'page-2.webp', makeWebp(1))
  ];
  const webpInput = await put(corpus, 'sample.webp', makeWebp(2));
  const tiffPages = [
    await put(resultsRoot, 'page-1.tiff', tiffBytes(0)),
    await put(resultsRoot, 'page-2.tiff', tiffBytes(1))
  ];
  const brokenTiff = await put(resultsRoot, 'broken.tiff', Buffer.from('49492a0008000000', 'hex'));
  const cbzOutput = await put(resultsRoot, 'pages.cbz', storedZip([
    ['0001.jpg', firstJpg], ['0002.jpg', secondJpg]
  ]));
  const cbzInput = await put(corpus, 'comic.cbz', storedZip([
    ['chapter/page-1.jpg', firstJpg], ['chapter/page-2.png', secondPng]
  ]));
  const zipInput = await put(corpus, 'comic.zip', storedZip([
    ['chapter/page-1.jpg', firstJpg], ['chapter/page-2.png', secondPng]
  ]));
  const htmlZipOutput = await put(resultsRoot, 'pages.zip', storedZip([
    ['index.html', `<!doctype html><meta http-equiv="Content-Security-Policy" content="default-src 'none'">
<link href="styles.css" rel="stylesheet"><img src="pages/page-0001.png"><img src="pages/page-0002.png">`],
    ['styles.css', 'body { color: black }'],
    ['pages/page-0001.png', firstPng], ['pages/page-0002.png', secondPng]
  ]));
  const brokenHtmlZip = await put(resultsRoot, 'broken-pages.zip', storedZip([
    ['index.html', `<!doctype html><meta http-equiv="Content-Security-Policy" content="default-src 'none'">
<link href="styles.css" rel="stylesheet"><img src="pages/page-0001.png"><script src="https://example.invalid/x.js"></script>`],
    ['styles.css', 'body { color: black }'],
    ['pages/page-0001.png', firstPng], ['pages/page-0002.png', secondPng]
  ]));
  const brokenCbz = await put(resultsRoot, 'broken.cbz', storedZip([
    ['0001.jpg', firstJpg], ['0003.jpg', secondJpg]
  ]));
  const nonJpegCbz = await put(resultsRoot, 'non-jpeg.cbz', storedZip([
    ['0001.jpg', firstJpg], ['0002.jpg', firstPng]
  ]));
  const fakeDocx = await put(corpus, 'fake.docx', Buffer.from('PK\x03\x04[Content_Types].xml word/document.xml'));
  const corruptDocxBytes = packageBytes('docx');
  corruptDocxBytes[corruptDocxBytes.indexOf('<Types/>')] ^= 0xff;
  const corruptDocx = await put(corpus, 'corrupt.docx', corruptDocxBytes);
  const wrongMimeOdp = await put(resultsRoot, 'wrong-mime.odp', packageBytes('odp', 'application/vnd.oasis.opendocument.text'));
  const truncatedRtf = await put(resultsRoot, 'truncated.rtf', '{\\rtf1\\ansi truncated');
  const genericXml = await put(resultsRoot, 'generic.xml', '<?xml version="1.0"?><document>not Flat ODF</document>');
  const externalFlatOdtXml = await put(
    resultsRoot,
    'external.xml',
    flatOdtXml('<text:p><text:a xlink:href="https://example.invalid">external</text:a></text:p>',
      'xmlns:xlink="http://www.w3.org/1999/xlink"')
  );
  const malformedFlatOdtXml = await put(
    resultsRoot,
    'malformed.xml',
    flatOdtXml().replace('</office:text>', '')
  );
  const dtdFlatOdtXml = await put(
    resultsRoot,
    'doctype.xml',
    flatOdtXml().replace('?>', '?><!DOCTYPE office:document [<!ENTITY leaked "x">]>')
  );

  examples = {
    permission, reference, output, outputText, measurementEvidence, ui, pdf, png, jpeg, bmpInput, svgInput,
    gifInput, webpInput, tiffInput, txtInput, rtfInput, htmlInput, invalidHtmlInput,
    screenshots: [screenshot, secondScreenshot], docxInput, odtInput, pptxInput, odpInput,
    xlsxInput, odsInput, docxOutput, odtOutput, rtfOutput, flatOdtXmlOutput,
    markdownOutput, epubOutput, brokenEpub,
    pptxOutput, odpOutput,
    pngPages, jpgPages, bmpPages, gifPages, webpPages, tiffPages, brokenTiff, cbzOutput, brokenCbz,
    cbzInput, zipInput, htmlZipOutput, brokenHtmlZip,
    nonJpegCbz, fakeDocx, corruptDocx, wrongMimeOdp, truncatedRtf, genericXml,
    externalFlatOdtXml, malformedFlatOdtXml, dtdFlatOdtXml
  };
  manifest = {
    schemaVersion: 1,
    releaseVersion: 'unit-test-only',
    documents: [{
      id: 'synthetic-word', title: 'Unit test only', language: 'zh', format: 'docx',
      categories: ['contract'], input: docxInput, textReference: reference,
      rights: {
        basis: 'written_permission', creator: 'Unit test', grantor: 'Unit test',
        sourceDescription: 'Generated in node:test', license: 'Unit test only',
        checkedAt: '2026-09-19', verifiedBy: 'Unit test', evidence: permission
      }
    }]
  };
  records = {
    schemaVersion: 1,
    runs: [{
      platform: 'macos-arm64', osVersion: 'test', appVersion: 'unit-test-only',
      appBinarySha256: sha('synthetic executable'), appPackageSha256: sha('synthetic package'),
      operator: 'Unit test', reviewedAt: '2026-09-19', execution: 'desktop_ui',
      results: [{
        documentId: 'synthetic-word', kind: 'docx_to_pdf', inputSha256: docxInput.sha256,
        elapsedMs: 100, peakRssMiB: 20, state: 'succeeded', uiEvidence: ui,
        measurementEvidence, outputs: [output], outputText,
        outputTextMethod: 'Independent synthetic unit fixture',
        review: { ...review(), textOrderPass: true }
      }]
    }]
  };
});

after(async () => { if (root) await rm(root, { recursive: true, force: true }); });

test('unit-only synthetic record checks every required artifact and computes retention', async () => {
  const result = await audit(cloned(manifest), cloned(records), corpus, resultsRoot, { strictCoverage: false });
  assert.deepEqual(result.errors, []);
  assert.equal(result.metrics[0].retention, 1);
  assert.equal(result.summary[0][1].succeeded, 1);
});

test('real-document release gate rejects a valid one-document unit fixture', async () => {
  const result = await audit(cloned(manifest), cloned(records), corpus, resultsRoot);
  assert.ok(result.errors.some((error) => error.includes('>=30 distinct real Chinese PDFs')));
  assert.ok(result.errors.some((error) => error.includes('missing real windows-x64 desktop run')));
});

test('missing private rights provenance cannot pass', async () => {
  const badManifest = cloned(manifest);
  delete badManifest.documents[0].rights.sourceDescription;
  const result = await audit(badManifest, cloned(records), corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('private source provenance')));
});

test('documents cannot escape their private corpus with parent traversal', async () => {
  const badManifest = cloned(manifest);
  badManifest.documents[0].input.file = '../document.docx';
  const result = await audit(badManifest, cloned(records), corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('expected a relative path without parent traversal')));
});

test('malformed category and screenshot fields produce errors without crashing', async () => {
  const badManifest = cloned(manifest);
  badManifest.documents[0].categories = {};
  const badRecords = cloned(records);
  badRecords.runs[0].results[0].review.resultImages = [null];
  const result = await audit(badManifest, badRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('expected one or more of')));
  assert.ok(result.errors.some((error) => error.includes('expected PNG or JPG screenshot')));
});

test('tampered input and output checksums are rejected', async () => {
  const badManifest = cloned(manifest);
  badManifest.documents[0].input.sha256 = sha('not the input');
  const badRecords = cloned(records);
  badRecords.runs[0].results[0].outputs[0].sha256 = sha('not the output');
  const result = await audit(badManifest, badRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('SHA-256 mismatch')));
});

test('absent output characters fail the retention threshold even with a success state', async () => {
  const badRecords = cloned(records);
  badRecords.runs[0].results[0].outputText = await put(resultsRoot, 'missing.txt', 'Agreement');
  const result = await audit(cloned(manifest), badRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('below the 98% minimum')));
});

test('numbers inconsistent with the hashed RSS/timing trace fail', async () => {
  const badRecords = cloned(records);
  badRecords.runs[0].results[0].peakRssMiB = 19;
  badRecords.runs[0].results[0].elapsedMs = 200;
  const result = await audit(cloned(manifest), badRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('reported duration differs')));
  assert.ok(result.errors.some((error) => error.includes('reported peak RSS differs')));
});

test('worker and unexplained failures cannot be counted as desktop success', async () => {
  const badRecords = cloned(records);
  badRecords.runs[0].execution = 'worker_cli';
  badRecords.runs[0].results[0].state = 'failed';
  badRecords.runs[0].results[0].errorCode = 'CONVERSION_FAILED';
  const result = await audit(cloned(manifest), badRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('only actual desktop UI conversions count')));
  assert.ok(result.errors.some((error) => error.includes('unexpected conversion failure')));
});

test('an explicitly declared protected-document error is counted but not as success', async () => {
  const protectedManifest = cloned(manifest);
  protectedManifest.documents[0].categories.push('protected');
  protectedManifest.documents[0].expectedFailures = { docx_to_pdf: ['UNSUPPORTED_FORMAT'] };
  const failedRecords = cloned(records);
  const row = failedRecords.runs[0].results[0];
  row.state = 'failed';
  row.errorCode = 'UNSUPPORTED_FORMAT';
  row.failureNote = 'Fixture exercises expected failure accounting, not a real protected file.';
  delete row.outputs;
  const result = await audit(protectedManifest, failedRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.deepEqual(result.errors, []);
  assert.equal(result.metrics.length, 0);
  assert.equal(result.summary[0][1].expectedFailure, 1);
});

test('no duplicate conversion records or screenshots for different pages', async () => {
  const badRecords = cloned(records);
  const row = badRecords.runs[0].results[0];
  row.review.sourcePages = 2;
  row.review.sourceImages = [row.review.sourceImages[0], row.review.sourceImages[0]];
  badRecords.runs[0].results.push(cloned(row));
  const result = await audit(cloned(manifest), badRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('duplicate conversion record')));
  assert.ok(result.errors.some((error) => error.includes('each reviewed page needs distinct screenshot evidence')));
});

test('one PDF record set covers every working-tree output direction', async () => {
  const [testManifest, testRecords] = completePdfCase();
  const result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.deepEqual(result.errors, []);
  assert.equal(result.summary.length, 17);
  assert.equal(result.metrics.length, 8);

  const badRecords = cloned(testRecords);
  badRecords.runs[0].results.find((row) => row.kind === 'pdf_to_pptx').review.outputPages = 1;
  const bad = await audit(testManifest, badRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(bad.errors.some((error) => error.includes('one destination page or slide per source page')));
});

test('PDF raster exports require one valid image per source page', async () => {
  for (const kind of ['pdf_to_bmp', 'pdf_to_gif', 'pdf_to_webp', 'pdf_to_tiff']) {
    const [testManifest, testRecords] = completePdfCase();
    testRecords.runs[0].results.find((row) => row.kind === kind).outputs.pop();
    const result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
    assert.ok(result.errors.some((error) => error.includes(`${kind}.outputs`) && error.includes('one image per source page')), kind);
  }
});

test('EPUB and TIFF outputs require their real container structures', async () => {
  const [epubManifest, epubRecords] = completePdfCase();
  epubRecords.runs[0].results.find((row) => row.kind === 'pdf_to_epub').outputs = [examples.brokenEpub];
  let result = await audit(epubManifest, epubRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('pdf_to_epub.outputs[0]') && error.includes('invalid or empty epub file')));

  const [tiffManifest, tiffRecords] = completePdfCase();
  tiffRecords.runs[0].results.find((row) => row.kind === 'pdf_to_tiff').outputs[0] = examples.brokenTiff;
  result = await audit(tiffManifest, tiffRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('pdf_to_tiff.outputs[0]') && error.includes('invalid or empty tiff file')));
});

test('CBZ requires consecutive JPEG entries and the exact source page count', async () => {
  const [testManifest, testRecords] = completePdfCase();
  const cbz = testRecords.runs[0].results.find((row) => row.kind === 'pdf_to_cbz');
  cbz.outputs = [examples.brokenCbz];
  let result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('invalid or empty cbz file')));

  cbz.outputs = [examples.nonJpegCbz];
  result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('invalid or empty cbz file')));

  const countMismatch = cloned(testRecords);
  const row = countMismatch.runs[0].results.find((item) => item.kind === 'pdf_to_cbz');
  row.outputs = [examples.cbzOutput];
  row.review.sourcePages = 1;
  row.review.outputPages = 1;
  row.review.pagesReviewed = 1;
  row.review.sourceImages = examples.screenshots.slice(0, 1);
  row.review.resultImages = examples.screenshots.slice(0, 1);
  result = await audit(testManifest, countMismatch, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('one sequentially numbered JPEG per source page')));
});

test('HTML ZIP requires offline resources and one referenced PNG per source page', async () => {
  const [testManifest, testRecords] = completePdfCase();
  let result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.deepEqual(result.errors, []);

  const row = testRecords.runs[0].results.find((item) => item.kind === 'pdf_to_html');
  row.outputs = [examples.brokenHtmlZip];
  result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('invalid or empty zip file')));

  row.outputs = [examples.htmlZipOutput];
  row.review.sourcePages = 1;
  row.review.outputPages = 1;
  row.review.pagesReviewed = 1;
  row.review.sourceImages = examples.screenshots.slice(0, 1);
  row.review.resultImages = examples.screenshots.slice(0, 1);
  result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('HTML ZIP must contain one referenced PNG per source page')));
});

test('BMP and CBZ/ZIP input require distinct page-by-page visual evidence', async () => {
  const [bmpManifest, bmpRecords] = visualCase('bmp', 'image_to_pdf', examples.bmpInput, 'image_mix', [examples.output]);
  assert.deepEqual((await audit(bmpManifest, bmpRecords, corpus, resultsRoot, { strictCoverage: false })).errors, []);

  for (const [format, input] of [['cbz', examples.cbzInput], ['zip', examples.zipInput]]) {
    const [testManifest, testRecords] = visualCase(format, 'comic_to_pdf', input, 'image_mix', [examples.output], 2);
    assert.deepEqual((await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false })).errors, [], format);
    testRecords.runs[0].results[0].review.sourcePages = 1;
    testRecords.runs[0].results[0].review.outputPages = 1;
    testRecords.runs[0].results[0].review.pagesReviewed = 1;
    testRecords.runs[0].results[0].review.sourceImages = examples.screenshots.slice(0, 1);
    testRecords.runs[0].results[0].review.resultImages = examples.screenshots.slice(0, 1);
    const mismatch = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
    assert.ok(mismatch.errors.some((error) => error.includes('comic ZIP must contain one image per source page')), format);
  }
});

test('SVG to PDF requires a single-page visual comparison and real SVG coverage', async () => {
  const [testManifest, testRecords] = visualCase('svg', 'svg_to_pdf', examples.svgInput, 'image_mix', [examples.output]);
  assert.deepEqual((await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false })).errors, []);
  testRecords.runs[0].results[0].review.outputPages = 2;
  assert.ok((await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false })).errors
    .some((error) => error.includes('one destination page or slide per source page')));
  const strict = await audit(cloned(manifest), cloned(records), corpus, resultsRoot);
  assert.ok(strict.errors.some((error) => error.includes('requires real static SVG vector artwork')));
});

test('offline HTML to PDF requires valid UTF-8, text retention and real release samples', async () => {
  const [testManifest, testRecords] = visualCase('html', 'html_to_pdf', examples.htmlInput, 'contract', [examples.output]);
  addTextEvidence(testManifest.documents[0], testRecords.runs[0].results[0]);
  assert.deepEqual((await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false })).errors, []);
  testManifest.documents[0].input = examples.invalidHtmlInput;
  testRecords.runs[0].results[0].inputSha256 = examples.invalidHtmlInput.sha256;
  assert.ok((await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false })).errors
    .some((error) => error.includes('invalid or empty html file')));
  const strict = await audit(cloned(manifest), cloned(records), corpus, resultsRoot);
  assert.ok(strict.errors.some((error) => error.includes('requires real offline HTML documents')));
});

test('fake OOXML and wrong ODF MIME types are rejected', async () => {
  const fakeManifest = cloned(manifest);
  fakeManifest.documents[0].input = examples.fakeDocx;
  const fakeRecords = cloned(records);
  fakeRecords.runs[0].results[0].inputSha256 = examples.fakeDocx.sha256;
  let result = await audit(fakeManifest, fakeRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('invalid or empty docx file')));

  fakeManifest.documents[0].input = examples.corruptDocx;
  fakeRecords.runs[0].results[0].inputSha256 = examples.corruptDocx.sha256;
  result = await audit(fakeManifest, fakeRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('invalid or empty docx file')));

  const [pdfManifest, pdfRecords] = completePdfCase();
  pdfRecords.runs[0].results.find((row) => row.kind === 'pdf_to_odp').outputs = [examples.wrongMimeOdp];
  result = await audit(pdfManifest, pdfRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('invalid or empty odp file')));
});

test('PDF text-document outputs reject truncated RTF and non-self-contained Flat ODF XML', async () => {
  const [testManifest, testRecords] = completePdfCase();
  testRecords.runs[0].results.find((row) => row.kind === 'pdf_to_rtf').outputs = [examples.truncatedRtf];
  testRecords.runs[0].results.find((row) => row.kind === 'pdf_to_flat_odt_xml').outputs = [examples.genericXml];
  let result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('pdf_to_rtf.outputs[0]') && error.includes('invalid or empty rtf file')));
  assert.ok(result.errors.some((error) => error.includes('pdf_to_flat_odt_xml.outputs[0]') && error.includes('invalid or empty xml file')));

  const externalRecords = cloned(testRecords);
  externalRecords.runs[0].results.find((row) => row.kind === 'pdf_to_rtf').outputs = [examples.rtfOutput];
  externalRecords.runs[0].results.find((row) => row.kind === 'pdf_to_flat_odt_xml').outputs = [examples.externalFlatOdtXml];
  result = await audit(testManifest, externalRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('pdf_to_flat_odt_xml.outputs[0]') && error.includes('invalid or empty xml file')));

  for (const invalidXml of [examples.malformedFlatOdtXml, examples.dtdFlatOdtXml]) {
    const malformedRecords = cloned(externalRecords);
    malformedRecords.runs[0].results.find((row) => row.kind === 'pdf_to_flat_odt_xml').outputs = [invalidXml];
    result = await audit(testManifest, malformedRecords, corpus, resultsRoot, { strictCoverage: false });
    assert.ok(result.errors.some((error) => error.includes('pdf_to_flat_odt_xml.outputs[0]') && error.includes('invalid or empty xml file')));
  }
});

test('picture-only presentations and supported static image inputs need visual evidence, not text', async () => {
  for (const [format, kind, input, category] of [
    ['pptx', 'pptx_to_pdf', examples.pptxInput, 'presentation_image'],
    ['odp', 'odp_to_pdf', examples.odpInput, 'presentation_image'],
    ['png', 'image_to_pdf', examples.png, 'contract'],
    ['jpeg', 'image_to_pdf', examples.jpeg, 'contract'],
    ['bmp', 'image_to_pdf', examples.bmpInput, 'image_mix'],
    ['gif', 'image_to_pdf', examples.gifInput, 'image_mix'],
    ['webp', 'image_to_pdf', examples.webpInput, 'image_mix'],
    ['tiff', 'image_to_pdf', examples.tiffInput, 'image_mix']
  ]) {
    const [testManifest, testRecords] = visualCase(format, kind, input, category, [examples.output]);
    const result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
    assert.deepEqual(result.errors, [], `${format} conversion should need images, not text`);
  }
});

test('TXT and RTF to PDF require independent text and visual evidence', async () => {
  for (const [format, kind, input] of [
    ['txt', 'txt_to_pdf', examples.txtInput],
    ['rtf', 'rtf_to_pdf', examples.rtfInput]
  ]) {
    const testManifest = cloned(manifest);
    const document = testManifest.documents[0];
    Object.assign(document, {
      id: `sample-${format}`,
      format,
      categories: ['contract'],
      input,
      textReference: examples.reference
    });
    const testRecords = cloned(records);
    const row = testRecords.runs[0].results[0];
    Object.assign(row, {
      documentId: document.id,
      kind,
      inputSha256: input.sha256,
      outputs: [examples.output],
      outputText: examples.outputText,
      outputTextMethod: 'Independent synthetic fixture',
      review: { ...review(), textOrderPass: true }
    });

    const result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
    assert.deepEqual(result.errors, [], `${format} conversion should require text and pages`);
  }
});

test('text-bearing PPTX and ODP require independent source and output text evidence', async () => {
  for (const [format, kind, input] of [
    ['pptx', 'pptx_to_pdf', examples.pptxInput],
    ['odp', 'odp_to_pdf', examples.odpInput]
  ]) {
    const [testManifest, testRecords] = visualCase(format, kind, input, 'presentation_text', [examples.output]);
    const absent = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
    assert.ok(absent.errors.some((error) => error.includes('textReference')), format);
    assert.ok(absent.errors.some((error) => error.includes('outputText')), format);
    addTextEvidence(testManifest.documents[0], testRecords.runs[0].results[0]);
    const complete = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
    assert.deepEqual(complete.errors, []);
    assert.equal(complete.metrics[0].retention, 1);
  }
});

test('ODT, XLSX, and ODS to PDF require independent text and page-by-page visual evidence', async () => {
  for (const [format, kind, input, category] of [
    ['odt', 'odt_to_pdf', examples.odtInput, 'contract'],
    ['xlsx', 'xlsx_to_pdf', examples.xlsxInput, 'spreadsheet_text'],
    ['ods', 'ods_to_pdf', examples.odsInput, 'spreadsheet_text']
  ]) {
    const [testManifest, testRecords] = visualCase(format, kind, input, category, [examples.output]);
    addTextEvidence(testManifest.documents[0], testRecords.runs[0].results[0]);
    const complete = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
    assert.deepEqual(complete.errors, [], format);

    const absent = cloned(testRecords);
    delete absent.runs[0].results[0].outputText;
    delete absent.runs[0].results[0].review.resultImages;
    const result = await audit(testManifest, absent, corpus, resultsRoot, { strictCoverage: false });
    assert.ok(result.errors.some((error) => error.includes('outputText')), format);
    assert.ok(result.errors.some((error) => error.includes('screenshot evidence is required for every page')), format);
  }
});

test('spreadsheet and ODP exports require exact output page counts', async () => {
  for (const [format, kind, input, category] of [
    ['xlsx', 'xlsx_to_pdf', examples.xlsxInput, 'spreadsheet_print_area'],
    ['ods', 'ods_to_pdf', examples.odsInput, 'spreadsheet_print_area'],
    ['odp', 'odp_to_pdf', examples.odpInput, 'presentation_shapes']
  ]) {
    const [testManifest, testRecords] = visualCase(format, kind, input, category, [examples.output], 2);
    if (format !== 'odp') addTextEvidence(testManifest.documents[0], testRecords.runs[0].results[0]);
    testRecords.runs[0].results[0].review.outputPages = 1;
    testRecords.runs[0].results[0].review.resultImages = examples.screenshots.slice(0, 1);
    const result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
    assert.ok(result.errors.some((error) => error.includes('one destination page or slide per source page')), format);
  }
});

test('extra image outputs and missing visual page evidence cannot pass', async () => {
  const [testManifest, testRecords] = visualCase('png', 'image_to_pdf', examples.png, 'contract', [examples.output, examples.output]);
  testRecords.runs[0].results[0].review.sourcePages = 2;
  const result = await audit(testManifest, testRecords, corpus, resultsRoot, { strictCoverage: false });
  assert.ok(result.errors.some((error) => error.includes('exactly one output file')));
  assert.ok(result.errors.some((error) => error.includes('exactly one source page')));
  assert.ok(result.errors.some((error) => error.includes('screenshot evidence is required for every page')));
});

test('strict release coverage requires every new real input class and category', async () => {
  const result = await audit(cloned(manifest), cloned(records), corpus, resultsRoot);
  for (const format of ['ODT', 'XLSX', 'ODS']) {
    assert.ok(result.errors.some((error) => error.includes(`requires real ${format} documents`)), format);
  }
  for (const format of ['BMP', 'GIF', 'WEBP']) {
    assert.ok(result.errors.some((error) => error.includes(`requires real ${format} images`)), format);
  }
  assert.ok(result.errors.some((error) => error.includes('requires a real CBZ or ZIP image archive')));
  for (const format of ['PPTX', 'ODP']) {
    for (const category of ['presentation_image', 'presentation_text', 'presentation_shapes']) {
      assert.ok(result.errors.some((error) => error.includes(`requires real ${format} presentations with ${category}`)), `${format}/${category}`);
    }
  }
  for (const format of ['XLSX', 'ODS']) {
    for (const category of ['spreadsheet_text', 'spreadsheet_formulas', 'spreadsheet_charts', 'spreadsheet_print_area']) {
      assert.ok(result.errors.some((error) => error.includes(`requires real ${format} spreadsheets with ${category}`)), `${format}/${category}`);
    }
  }
  const oldManifest = cloned(manifest);
  oldManifest.documents[0].format = 'doc';
  const old = await audit(oldManifest, cloned(records), corpus, resultsRoot, { strictCoverage: false });
  assert.ok(old.errors.some((error) => error.includes('expected one of pdf, docx, odt, pptx, odp, xlsx, ods')));
});
