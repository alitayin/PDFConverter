import assert from 'node:assert/strict';
import { test } from 'node:test';
import { acceptsName, conversionGroups, conversionModes, inputAccept, outputName } from '../src/lib/conversion.ts';
import { expectedConversionKinds } from './expected-conversion-kinds.mjs';

test('packaged integration manifest matches every UI conversion mode', () => {
  assert.deepEqual(
    conversionModes.map(({ kind }) => kind).sort(),
    [...expectedConversionKinds].sort(),
  );
});

test('image to PDF mode accepts supported static raster files regardless of extension case', () => {
  assert.equal(conversionModes.find((mode) => mode.kind === 'image_to_pdf')?.output, 'PDF');
  assert.equal(inputAccept('image_to_pdf'), '.png,.jpg,.jpeg,.bmp,.gif,.webp,.tif,.tiff');
  for (const name of ['scan.png', 'scan.PNG', 'photo.jpg', 'photo.JpEg', 'bitmap.bmp', 'poster.gif', 'poster.WEBP', 'archive.tif', 'archive.TIFF']) {
    assert.equal(acceptsName('image_to_pdf', name), true, name);
  }
  for (const name of ['document.pdf', 'photo.svg', 'photo.jpg.exe', 'png']) {
    assert.equal(acceptsName('image_to_pdf', name), false, name);
  }
});

test('each image keeps its own PDF output name without changing existing modes', () => {
  assert.equal(outputName('image_to_pdf', 'holiday.photo.JPEG', { imageFormat: 'jpg' }), 'holiday.photo.pdf');
  assert.equal(outputName('image_to_pdf', 'scan.png', {}), 'scan.pdf');
  assert.equal(inputAccept('docx_to_pdf'), '.docx');
  assert.equal(inputAccept('pdf_to_pptx'), '.pdf');
  assert.equal(acceptsName('docx_to_pdf', 'draft.docx'), true);
  assert.equal(acceptsName('pdf_to_txt', 'manual.pdf'), true);
});

test('image merge names one PDF after the first selected image without adding a mode', () => {
  assert.equal(outputName('image_to_pdf', 'chapter.cover.PNG', { mergeImages: true }), 'chapter.cover-merged.pdf');
  assert.equal(outputName('image_to_pdf', 'chapter.cover.PNG', { mergeImages: false }), 'chapter.cover.pdf');
  assert.equal(conversionModes.filter((mode) => mode.kind === 'image_to_pdf').length, 1);
});

test('PPT and PPTX have separate input modes', () => {
  const mode = conversionModes.find((item) => item.kind === 'pptx_to_pdf');
  assert.equal(conversionModes.length, expectedConversionKinds.length);
  assert.equal(mode?.output, 'PDF');
  assert.match(mode?.description ?? '', /presentation pages/);
  assert.equal(inputAccept('pptx_to_pdf'), '.pptx');
  for (const name of ['slides.pptx', 'slides.PPTX', 'meeting.final.PpTx']) {
    assert.equal(acceptsName('pptx_to_pdf', name), true, name);
  }
  for (const name of ['slides.ppt', 'slides.pdf', 'slides.pptx.exe', 'pptx']) {
    assert.equal(acceptsName('pptx_to_pdf', name), false, name);
  }
  assert.equal(outputName('pptx_to_pdf', 'meeting.final.PPTX', {}), 'meeting.final.pdf');
  assert.equal(inputAccept('pdf_to_pptx'), '.pdf');
  assert.equal(inputAccept('ppt_to_pdf'), '.ppt');
  assert.equal(acceptsName('ppt_to_pdf', 'slides.ppt'), true);
  assert.equal(acceptsName('ppt_to_pdf', 'slides.pptx'), false);
});

test('office conversions have distinct source filters, groups, and output extensions', () => {
  assert.deepEqual(conversionGroups.map((group) => group.id), ['pdf', 'word', 'slides', 'spreadsheets', 'images']);
  for (const [kind, input, result] of [
    ['pdf_to_odt', 'notes.pdf', 'notes.odt'],
    ['pdf_to_odp', 'slides.pdf', 'slides.odp'],
    ['pdf_to_rtf', 'notes.pdf', 'notes.rtf'],
    ['pdf_to_flat_odt_xml', 'notes.pdf', 'notes.xml'],
    ['pdf_to_markdown', 'notes.pdf', 'notes.md'],
    ['pdf_to_doc', 'notes.pdf', 'notes.doc'],
    ['pdf_to_ppt', 'slides.pdf', 'slides.ppt'],
    ['doc_to_pdf', 'notes.doc', 'notes.pdf'],
    ['ppt_to_pdf', 'slides.ppt', 'slides.pdf'],
    ['txt_to_pdf', 'notes.txt', 'notes.pdf'],
    ['rtf_to_pdf', 'notes.rtf', 'notes.pdf'],
    ['html_to_pdf', 'notes.htm', 'notes.pdf'],
    ['markdown_to_pdf', 'notes.markdown', 'notes.pdf'],
    ['odt_to_pdf', 'notes.odt', 'notes.pdf'],
    ['odp_to_pdf', 'slides.odp', 'slides.pdf'],
    ['xlsx_to_pdf', 'metrics.xlsx', 'metrics.pdf'],
    ['ods_to_pdf', 'metrics.ods', 'metrics.pdf']
  ]) {
    assert.ok(conversionModes.some((mode) => mode.kind === kind), `${kind} is available`);
    assert.ok(acceptsName(kind, input), `${kind} accepts ${input}`);
    assert.equal(outputName(kind, input, {}), result);
    assert.equal(acceptsName(kind, `${input}.exe`), false);
  }
  assert.equal(acceptsName('odt_to_pdf', 'draft.doc'), false);
  assert.equal(acceptsName('odp_to_pdf', 'deck.ppt'), false);
  assert.equal(acceptsName('xlsx_to_pdf', 'data.xls'), false);
  assert.equal(acceptsName('ods_to_pdf', 'data.xlsx'), false);
  assert.equal(acceptsName('html_to_pdf', 'notes.html.exe'), false);
  assert.equal(acceptsName('markdown_to_pdf', 'notes.md.exe'), false);
  assert.equal(acceptsName('doc_to_pdf', 'book.docx.exe'), false);
});

test('PDF to Markdown is a PDF-only text-layer export with a stable MD name', () => {
  const mode = conversionModes.find((item) => item.kind === 'pdf_to_markdown');
  assert.equal(mode?.group, 'pdf');
  assert.equal(mode?.output, 'MD');
  assert.match(mode?.description ?? '', /text by page/);
  assert.equal(inputAccept('pdf_to_markdown'), '.pdf');
  assert.equal(acceptsName('pdf_to_markdown', 'paper.PDF'), true);
  assert.equal(acceptsName('pdf_to_markdown', 'paper.md'), false);
  assert.equal(outputName('pdf_to_markdown', 'paper.final.PDF', {}), 'paper.final.md');
});

test('PDF image options name each emitted format without changing file filters', () => {
  for (const format of ['png', 'jpg', 'gif', 'webp', 'bmp', 'tiff']) {
    assert.equal(outputName('pdf_to_image', 'pages.pdf', { imageFormat: format }), `pages.${format}`);
  }
  assert.equal(inputAccept('pdf_to_image'), '.pdf');
  assert.equal(inputAccept('image_to_pdf'), '.png,.jpg,.jpeg,.bmp,.gif,.webp,.tif,.tiff');
});

test('removed long-tail formats are absent from the product contract', () => {
  for (const kind of ['pdf_to_html', 'pdf_to_epub', 'epub_to_pdf', 'pdf_to_cbz', 'comic_to_pdf']) {
    assert.equal(conversionModes.some((mode) => mode.kind === kind), false);
  }
});

test('SVG vector conversion only accepts SVG and produces PDF', () => {
  assert.equal(inputAccept('svg_to_pdf'), '.svg');
  assert.equal(acceptsName('svg_to_pdf', 'drawing.SVG'), true);
  assert.equal(acceptsName('svg_to_pdf', 'drawing.svg.exe'), false);
  assert.equal(acceptsName('image_to_pdf', 'drawing.svg'), false);
  assert.equal(outputName('svg_to_pdf', 'drawing.SVG', {}), 'drawing.pdf');
});
