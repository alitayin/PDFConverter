import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { isAbsolute, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { crc32 } from 'node:zlib';
import { expectedConversionKinds } from '../scripts/expected-conversion-kinds.mjs';

const require = createRequire(import.meta.url);
const { RustBridge } = require('./bridge.cjs');
const electronExecutable = require('electron');
const electronAppRoot = fileURLToPath(new URL('..', import.meta.url));
const engine = process.env.MINIMALPDF_RUST_ENGINE;
const coveredKinds = new Set();
let bridgeSequence = 0;

function storedZip(entries) {
  const localParts = [];
  const centralParts = [];
  let offset = 0;
  for (const [name, value] of entries) {
    const nameBytes = Buffer.from(name, 'utf8');
    const data = Buffer.isBuffer(value) ? value : Buffer.from(value, 'utf8');
    const checksum = crc32(data) >>> 0;
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(0x0800, 6);
    local.writeUInt16LE(0, 8);
    local.writeUInt16LE(0, 10);
    local.writeUInt16LE(33, 12);
    local.writeUInt32LE(checksum, 14);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    local.writeUInt16LE(0, 28);
    localParts.push(local, nameBytes, data);

    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(0x0800, 8);
    central.writeUInt16LE(0, 10);
    central.writeUInt16LE(0, 12);
    central.writeUInt16LE(33, 14);
    central.writeUInt32LE(checksum, 16);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt32LE(offset, 42);
    centralParts.push(central, nameBytes);
    offset += local.length + nameBytes.length + data.length;
  }

  const directory = Buffer.concat(centralParts);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...localParts, directory, end]);
}

function makeXlsx() {
  return storedZip([
    ['[Content_Types].xml', '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>'],
    ['_rels/.rels', '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>'],
    ['xl/workbook.xml', '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Numbers" sheetId="1" r:id="rId1"/></sheets></workbook>'],
    ['xl/_rels/workbook.xml.rels', '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>'],
    ['xl/worksheets/sheet1.xml', '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Converted</t></is></c></row></sheetData></worksheet>'],
  ]);
}

function makeOds() {
  return storedZip([
    ['mimetype', 'application/vnd.oasis.opendocument.spreadsheet'],
    ['content.xml', '<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.3"><office:body><office:spreadsheet><table:table table:name="Numbers"><table:table-row><table:table-cell office:value-type="string"><text:p>Converted</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>'],
    ['META-INF/manifest.xml', '<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>'],
  ]);
}

function makePdf() {
  const content = 'BT /F1 12 Tf 72 720 Td (Office conversion smoke) Tj ET';
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
  return pdf + `trailer\n<< /Root 1 0 R /Size ${offsets.length} >>\nstartxref\n${xref}\n%%EOF\n`;
}

async function makeBridge(root, name) {
  bridgeSequence += 1;
  const parent = join(root, 'bridge-work', `${name}-${bridgeSequence}`);
  await mkdir(parent, { recursive: true });
  return new RustBridge(engine, {
    env: {
      ...process.env,
      MINIMALPDF_ELECTRON_EXECUTABLE: process.env.MINIMALPDF_ELECTRON_EXECUTABLE || electronExecutable,
      MINIMALPDF_ELECTRON_APP_ROOT: process.env.MINIMALPDF_ELECTRON_APP_ROOT ?? electronAppRoot,
    },
  });
}

async function convert(root, kind, input, options = {}) {
  const bridge = await makeBridge(root, kind);
  let timer;
  try {
    const finished = new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error(`${kind}: conversion timed out`)), 90_000);
      bridge.on('progress', (event) => {
        if (['succeeded', 'failed', 'cancelled', 'timed_out'].includes(event.state)) resolve(event);
      });
      bridge.on('terminated', (reason) => reject(new Error(`${kind}: engine terminated: ${reason}`)));
    });
    // start_job can reject before we await the event; still consume bridge shutdown errors.
    void finished.catch(() => {});
    const id = await bridge.invoke('start_job', {
      request: { kind, inputs: Array.isArray(input) ? input : [input], outputDir: root, options },
    });
    const result = await finished;
    assert.equal(result.job_id, id);
    assert.equal(result.state, 'succeeded', `${kind}: ${result.error_code ?? ''} ${result.message}`);
    assert.equal(result.outputs.length, 1);
    coveredKinds.add(kind);
    return result.outputs[0];
  } finally {
    clearTimeout(timer);
    await bridge.close();
  }
}

test('local format directions produce readable file signatures across Electron and Rust', {
  skip: !engine || !isAbsolute(engine) || !existsSync(engine) ? 'set MINIMALPDF_RUST_ENGINE to a built absolute engine path' : false,
  timeout: 240_000,
}, async (t) => {
  const root = await mkdtemp(join(tmpdir(), 'minimalpdf-formats-'));
  try {
    const pdf = join(root, 'input.pdf');
    await writeFile(pdf, makePdf());

    const txt = await convert(root, 'pdf_to_txt', pdf);
    assert.match(await readFile(txt, 'utf8'), /Office conversion smoke/);
    const markdown = await convert(root, 'pdf_to_markdown', pdf);
    assert.match(await readFile(markdown, 'utf8'), /Office conversion smoke/);
    const image = await convert(root, 'pdf_to_image', pdf, { imageFormat: 'png', dpi: 150 });
    assert.deepEqual((await readFile(image)).subarray(0, 8), Buffer.from('89504e470d0a1a0a', 'hex'));
    const jpeg = await convert(root, 'pdf_to_image', pdf, { imageFormat: 'jpg', dpi: 150 });
    assert.deepEqual((await readFile(jpeg)).subarray(0, 3), Buffer.from('ffd8ff', 'hex'));
    const bmp = await convert(root, 'pdf_to_image', pdf, { imageFormat: 'bmp', dpi: 150 });
    assert.equal((await readFile(bmp)).subarray(0, 2).toString('ascii'), 'BM');
    const gif = await convert(root, 'pdf_to_image', pdf, { imageFormat: 'gif', dpi: 150 });
    assert.match((await readFile(gif)).subarray(0, 6).toString('ascii'), /^GIF8[79]a$/);
    const webp = await convert(root, 'pdf_to_image', pdf, { imageFormat: 'webp', dpi: 150 });
    const webpBytes = await readFile(webp);
    assert.equal(webpBytes.subarray(0, 4).toString('ascii'), 'RIFF');
    assert.equal(webpBytes.subarray(8, 12).toString('ascii'), 'WEBP');
    const tiff = await convert(root, 'pdf_to_image', pdf, { imageFormat: 'tiff', dpi: 150 });
    assert.deepEqual((await readFile(tiff)).subarray(0, 4), Buffer.from('49492a00', 'hex'));
    const imagePdf = await convert(root, 'image_to_pdf', image);
    assert.match((await readFile(imagePdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const jpegPdf = await convert(root, 'image_to_pdf', jpeg);
    assert.match((await readFile(jpegPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const bmpPdf = await convert(root, 'image_to_pdf', bmp);
    assert.match((await readFile(bmpPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const gifPdf = await convert(root, 'image_to_pdf', gif);
    assert.match((await readFile(gifPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const webpPdf = await convert(root, 'image_to_pdf', webp);
    assert.match((await readFile(webpPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const tiffPdf = await convert(root, 'image_to_pdf', tiff);
    assert.match((await readFile(tiffPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const svg = join(root, 'drawing.svg');
    await writeFile(svg, '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 120 80" width="120" height="80"><rect x="10" y="10" width="100" height="60" fill="#df3434"/></svg>');
    const svgPdf = await convert(root, 'svg_to_pdf', svg);
    assert.match((await readFile(svgPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const html = join(root, 'local.html');
    await writeFile(html, '<!doctype html><html><head><style>h1 { color: red }</style></head><body><h1>Offline HTML print</h1></body></html>');
    const htmlPdf = await convert(root, 'html_to_pdf', html);
    assert.match((await readFile(htmlPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const md = join(root, 'local.markdown');
    await writeFile(md, '# Offline Markdown\n\nA paragraph with **bold** text and a list.\n\n- One\n- Two\n');
    const markdownPdf = await convert(root, 'markdown_to_pdf', md);
    assert.match((await readFile(markdownPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);

    const bridge = await makeBridge(root, 'check');
    let officeAvailable;
    try {
      const check = await bridge.invoke('get_self_check');
      officeAvailable = check.checks.some((entry) => entry.id === 'office_runtime' && entry.status === 'passed');
    } finally {
      await bridge.close();
    }
    if (process.env.MINIMALPDF_REQUIRE_OFFICE === '1') {
      assert.ok(officeAvailable, 'Office runtime required for document format directions');
    }
    if (!officeAvailable) {
      t.diagnostic('Office runtime unavailable; non-Office directions checked, Office directions skipped');
      return;
    }

    const pptx = await convert(root, 'pdf_to_pptx', pdf);
    assert.deepEqual((await readFile(pptx)).subarray(0, 4), Buffer.from('504b0304', 'hex'));
    const docx = await convert(root, 'pdf_to_docx', pdf);
    assert.deepEqual((await readFile(docx)).subarray(0, 4), Buffer.from('504b0304', 'hex'));
    const legacyDoc = await convert(root, 'pdf_to_doc', pdf);
    assert.deepEqual((await readFile(legacyDoc)).subarray(0, 8), Buffer.from('d0cf11e0a1b11ae1', 'hex'));
    const legacyWordPdf = await convert(root, 'doc_to_pdf', legacyDoc);
    assert.match((await readFile(legacyWordPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const legacyPpt = await convert(root, 'pdf_to_ppt', pdf);
    assert.deepEqual((await readFile(legacyPpt)).subarray(0, 8), Buffer.from('d0cf11e0a1b11ae1', 'hex'));
    const legacySlidesPdf = await convert(root, 'ppt_to_pdf', legacyPpt);
    assert.match((await readFile(legacySlidesPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const wordPdf = await convert(root, 'docx_to_pdf', docx);
    assert.match((await readFile(wordPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const txtPdf = await convert(root, 'txt_to_pdf', txt);
    assert.match((await readFile(txtPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const slidesPdf = await convert(root, 'pptx_to_pdf', pptx);
    assert.match((await readFile(slidesPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const odt = await convert(root, 'pdf_to_odt', pdf);
    assert.deepEqual((await readFile(odt)).subarray(0, 4), Buffer.from('504b0304', 'hex'));
    const odtPdf = await convert(root, 'odt_to_pdf', odt);
    assert.match((await readFile(odtPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const odp = await convert(root, 'pdf_to_odp', pdf);
    assert.deepEqual((await readFile(odp)).subarray(0, 4), Buffer.from('504b0304', 'hex'));
    const odpPdf = await convert(root, 'odp_to_pdf', odp);
    assert.match((await readFile(odpPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const rtf = await convert(root, 'pdf_to_rtf', pdf);
    assert.equal((await readFile(rtf)).subarray(0, 6).toString('ascii'), '{\\rtf1');
    const rtfPdf = await convert(root, 'rtf_to_pdf', rtf);
    assert.match((await readFile(rtfPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const flatOdtXml = await convert(root, 'pdf_to_flat_odt_xml', pdf);
    const flatOdtXmlText = await readFile(flatOdtXml, 'utf8');
    assert.match(flatOdtXmlText, /<office:document\b/);
    assert.match(flatOdtXmlText, /office:mimetype="application\/vnd\.oasis\.opendocument\.text"/);
    const xlsx = join(root, 'spreadsheet-xlsx.xlsx');
    await writeFile(xlsx, makeXlsx());
    const xlsxPdf = await convert(root, 'xlsx_to_pdf', xlsx);
    assert.match((await readFile(xlsxPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);
    const ods = join(root, 'spreadsheet-ods.ods');
    await writeFile(ods, makeOds());
    const odsPdf = await convert(root, 'ods_to_pdf', ods);
    assert.match((await readFile(odsPdf)).subarray(0, 8).toString('ascii'), /^%PDF-/);

    assert.deepEqual(
      [...coveredKinds].sort(),
      [...expectedConversionKinds].sort(),
      'packaged integration coverage must include every UI conversion mode',
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('image merge option produces one multi-page PDF through the Electron bridge', {
  skip: !engine || !isAbsolute(engine) || !existsSync(engine) ? 'set MINIMALPDF_RUST_ENGINE to a built absolute engine path' : false,
  timeout: 90_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), 'minimalpdf-image-merge-'));
  try {
    const pdf = join(root, 'source.pdf');
    await writeFile(pdf, makePdf());
    const png = await convert(root, 'pdf_to_image', pdf, { imageFormat: 'png', dpi: 150 });
    const bmp = await convert(root, 'pdf_to_image', pdf, { imageFormat: 'bmp', dpi: 150 });
    const jpg = await convert(root, 'pdf_to_image', pdf, { imageFormat: 'jpg', dpi: 150 });

    const merged = await convert(root, 'image_to_pdf', [png, bmp, jpg], { mergeImages: true });
    assert.match(merged, /source-merged\.pdf$/);
    const data = await readFile(merged);
    assert.match(data.subarray(0, 8).toString('ascii'), /^%PDF-/);
    assert.equal((data.toString('latin1').match(/\/Type \/Page \/Parent/g) ?? []).length, 3);
    assert.match(data.toString('latin1'), /\/Count 3/);
    assert.equal((await readdir(root)).filter((name) => name.endsWith('-merged.pdf')).length, 1);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('PDF to Markdown crosses the Electron bridge with stable page boundaries', {
  skip: !engine || !isAbsolute(engine) || !existsSync(engine) ? 'set MINIMALPDF_RUST_ENGINE to a built absolute engine path' : false,
  timeout: 30_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), 'minimalpdf-markdown-'));
  try {
    const pdf = join(root, 'input.pdf');
    await writeFile(pdf, makePdf());

    const markdown = await convert(root, 'pdf_to_markdown', pdf);

    assert.ok(markdown.endsWith('.md'));
    assert.equal(
      await readFile(markdown, 'utf8'),
      '<!-- page: 1 -->\n\nOffice conversion smoke\n',
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('cancelling HTML print through Rust stops the worker and leaves no output', {
  skip: !engine || !isAbsolute(engine) || !existsSync(engine) ? 'set MINIMALPDF_RUST_ENGINE to a built absolute engine path' : false,
  timeout: 30_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), 'minimalpdf-html-cancel-'));
  const bridge = await makeBridge(root, 'html-cancel');
  let timer;
  try {
    const input = join(root, 'cancel.html');
    await writeFile(input, '<h1>Cancel offline printing</h1>');
    let startedPrinting;
    const printing = new Promise((resolve) => { startedPrinting = resolve; });
    const finished = new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error('HTML print cancellation timed out')), 20_000);
      bridge.on('progress', (event) => {
        if (event.state === 'running' && event.phase === 'converting') startedPrinting();
        if (['succeeded', 'failed', 'cancelled', 'timed_out'].includes(event.state)) resolve(event);
      });
    });
    const jobId = await bridge.invoke('start_job', {
      request: { kind: 'html_to_pdf', inputs: [input], outputDir: root, options: {} },
    });
    await Promise.race([printing, finished.then((event) => { throw new Error(`print ended before cancellation: ${event.state}`); })]);
    await new Promise((resolve) => setTimeout(resolve, 150));
    await bridge.invoke('cancel_job', { jobId });
    const result = await finished;
    assert.equal(result.job_id, jobId);
    assert.equal(result.state, 'cancelled');
    assert.equal(result.outputs, undefined);
    assert.equal((await readdir(root)).some((name) => name.endsWith('.pdf')), false);
  } finally {
    clearTimeout(timer);
    await bridge.close();
    await rm(root, { recursive: true, force: true });
  }
});
