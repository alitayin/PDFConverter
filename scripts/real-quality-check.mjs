import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { open, readFile, realpath, stat } from 'node:fs/promises';
import { dirname, extname, isAbsolute, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { inflateRawSync } from 'node:zlib';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const platforms = ['windows-x64', 'macos-arm64', 'macos-x64'];
const pdfCategories = ['contract', 'resume', 'notice', 'paper', 'simple_table', 'two_column', 'image_mix', 'different_fonts', 'protected'];
const presentationCategories = ['presentation_image', 'presentation_text', 'presentation_shapes'];
const spreadsheetCategories = ['spreadsheet_text', 'spreadsheet_formulas', 'spreadsheet_charts', 'spreadsheet_print_area', 'protected'];
const operationsByFormat = {
  pdf: [
    'pdf_to_docx', 'pdf_to_odt', 'pdf_to_rtf', 'pdf_to_flat_odt_xml', 'pdf_to_txt',
    'pdf_to_markdown', 'pdf_to_epub',
    'pdf_to_png', 'pdf_to_jpg', 'pdf_to_bmp', 'pdf_to_gif', 'pdf_to_webp', 'pdf_to_tiff',
    'pdf_to_pptx', 'pdf_to_odp', 'pdf_to_cbz', 'pdf_to_html'
  ],
  docx: ['docx_to_pdf'],
  odt: ['odt_to_pdf'],
  pptx: ['pptx_to_pdf'],
  odp: ['odp_to_pdf'],
  xlsx: ['xlsx_to_pdf'],
  ods: ['ods_to_pdf'],
  png: ['image_to_pdf'],
  jpg: ['image_to_pdf'],
  jpeg: ['image_to_pdf'],
  bmp: ['image_to_pdf'],
  gif: ['image_to_pdf'],
  webp: ['image_to_pdf'],
  tif: ['image_to_pdf'],
  tiff: ['image_to_pdf'],
  svg: ['svg_to_pdf'],
  cbz: ['comic_to_pdf'],
  zip: ['comic_to_pdf'],
  txt: ['txt_to_pdf'],
  rtf: ['rtf_to_pdf'],
  html: ['html_to_pdf'],
  htm: ['html_to_pdf'],
  md: ['markdown_to_pdf'],
  markdown: ['markdown_to_pdf'],
  epub: ['epub_to_pdf']
};
const outputFormatByOperation = {
  docx_to_pdf: 'pdf', odt_to_pdf: 'pdf', pptx_to_pdf: 'pdf', odp_to_pdf: 'pdf',
  xlsx_to_pdf: 'pdf', ods_to_pdf: 'pdf', image_to_pdf: 'pdf', svg_to_pdf: 'pdf', comic_to_pdf: 'pdf', html_to_pdf: 'pdf',
  pdf_to_docx: 'docx', pdf_to_odt: 'odt', pdf_to_rtf: 'rtf',
  pdf_to_flat_odt_xml: 'xml', pdf_to_txt: 'txt', pdf_to_markdown: 'md',
  pdf_to_epub: 'epub', markdown_to_pdf: 'pdf', epub_to_pdf: 'pdf',
  pdf_to_png: 'png', pdf_to_jpg: 'jpg', pdf_to_bmp: 'bmp',
  pdf_to_gif: 'gif', pdf_to_webp: 'webp', pdf_to_tiff: 'tiff', pdf_to_pptx: 'pptx',
  pdf_to_odp: 'odp', pdf_to_cbz: 'cbz', pdf_to_html: 'zip'
};
const textOperations = new Set([
  'pdf_to_docx', 'pdf_to_odt', 'pdf_to_rtf', 'pdf_to_flat_odt_xml',
  'pdf_to_odp', 'pdf_to_txt', 'pdf_to_markdown', 'pdf_to_epub',
  'docx_to_pdf', 'odt_to_pdf', 'txt_to_pdf', 'rtf_to_pdf', 'html_to_pdf', 'markdown_to_pdf', 'xlsx_to_pdf', 'ods_to_pdf'
]);
const pagedImageOperations = new Set([
  'pdf_to_png', 'pdf_to_jpg', 'pdf_to_bmp', 'pdf_to_gif', 'pdf_to_webp', 'pdf_to_tiff'
]);
const oneOutputOperations = new Set(
  Object.keys(outputFormatByOperation).filter((operation) => !pagedImageOperations.has(operation))
);
const exactPageOperations = new Set([
  'pdf_to_pptx', 'pdf_to_odp', 'pdf_to_cbz', 'pdf_to_html',
  'pptx_to_pdf', 'odp_to_pdf', 'xlsx_to_pdf', 'ods_to_pdf', 'image_to_pdf', 'svg_to_pdf', 'comic_to_pdf'
]);
const flowingDocumentOperations = new Set([
  'pdf_to_docx', 'pdf_to_odt', 'pdf_to_rtf', 'pdf_to_flat_odt_xml',
  'pdf_to_epub', 'docx_to_pdf', 'odt_to_pdf', 'txt_to_pdf', 'rtf_to_pdf'
]);
const shaPattern = /^[a-f0-9]{64}$/;
const datePattern = /^\d{4}-\d{2}-\d{2}$/;
const MAX_ZIP_CENTRAL_DIRECTORY = 16 * 1024 * 1024;
const MAX_ZIP_ENTRIES = 10_000;
const MAX_ARCHIVE_ENTRY_BYTES = 64 * 1024 * 1024;
const MAX_FLAT_ODT_XML_BYTES = 64 * 1024 * 1024;

function issue(errors, location, reason) {
  errors.push(`${location}: ${reason}`);
}

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isString(value) {
  return typeof value === 'string' && value.trim().length > 0;
}

function isDate(value) {
  if (typeof value !== 'string' || !datePattern.test(value)) return false;
  const date = new Date(`${value}T00:00:00Z`);
  return !Number.isNaN(date.getTime()) && date.toISOString().slice(0, 10) === value && date <= new Date();
}

function isHttps(value) {
  if (!isString(value)) return false;
  try { return new URL(value).protocol === 'https:'; } catch { return false; }
}

function requireValue(errors, location, value, predicate, description) {
  if (!predicate(value)) issue(errors, location, description);
}

async function insidePath(base, name) {
  if (!isString(name) || isAbsolute(name) || name.split(/[\\/]/).includes('..')) {
    throw new Error('expected a relative path without parent traversal');
  }
  const root = await realpath(base);
  const file = await realpath(resolve(root, name));
  if (!file.startsWith(`${root}${sep}`)) throw new Error('path resolves outside the corpus');
  if (file === repoRoot || file.startsWith(`${repoRoot}${sep}`)) {
    throw new Error('private corpus and evaluation artifacts must remain outside the Git repository');
  }
  return file;
}

async function hashFile(file) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest('hex');
}

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

async function zipEntries(handle, size) {
  if (size < 22) return null;
  const tailSize = Math.min(size, 65_557);
  const tail = Buffer.alloc(tailSize);
  await handle.read(tail, 0, tail.length, size - tail.length);
  let end = -1;
  for (let index = tail.length - 22; index >= 0; index--) {
    if (tail.readUInt32LE(index) === 0x06054b50) {
      end = index;
      break;
    }
  }
  if (end < 0 || end + 22 > tail.length) return null;
  const disk = tail.readUInt16LE(end + 4);
  const centralDisk = tail.readUInt16LE(end + 6);
  const entriesOnDisk = tail.readUInt16LE(end + 8);
  const entryCount = tail.readUInt16LE(end + 10);
  const centralSize = tail.readUInt32LE(end + 12);
  const centralOffset = tail.readUInt32LE(end + 16);
  const commentLength = tail.readUInt16LE(end + 20);
  if (disk !== 0 || centralDisk !== 0 || entriesOnDisk !== entryCount ||
      entryCount === 0xffff || centralSize === 0xffffffff || centralOffset === 0xffffffff ||
      entryCount > MAX_ZIP_ENTRIES || centralSize > MAX_ZIP_CENTRAL_DIRECTORY ||
      end + 22 + commentLength !== tail.length || centralOffset + centralSize > size) return null;

  const central = Buffer.alloc(centralSize);
  await handle.read(central, 0, central.length, centralOffset);
  const decoder = new TextDecoder('utf-8', { fatal: true });
  const entries = [];
  const names = new Set();
  let cursor = 0;
  try {
    for (let index = 0; index < entryCount; index++) {
      if (cursor + 46 > central.length || central.readUInt32LE(cursor) !== 0x02014b50) return null;
      const flags = central.readUInt16LE(cursor + 8);
      const method = central.readUInt16LE(cursor + 10);
      const checksum = central.readUInt32LE(cursor + 16);
      const compressedSize = central.readUInt32LE(cursor + 20);
      const uncompressedSize = central.readUInt32LE(cursor + 24);
      const nameLength = central.readUInt16LE(cursor + 28);
      const extraLength = central.readUInt16LE(cursor + 30);
      const entryCommentLength = central.readUInt16LE(cursor + 32);
      const diskStart = central.readUInt16LE(cursor + 34);
      const localOffset = central.readUInt32LE(cursor + 42);
      const recordLength = 46 + nameLength + extraLength + entryCommentLength;
      if (nameLength === 0 || cursor + recordLength > central.length || diskStart !== 0 ||
          compressedSize === 0xffffffff || uncompressedSize === 0xffffffff ||
          (flags & 0x0001) !== 0 || ![0, 8].includes(method) || localOffset + 30 > centralOffset) return null;
      const name = decoder.decode(central.subarray(cursor + 46, cursor + 46 + nameLength));
      if (name.includes('\0') || name.startsWith('/') || name.split('/').includes('..') || names.has(name)) return null;

      const local = Buffer.alloc(30);
      await handle.read(local, 0, local.length, localOffset);
      if (local.readUInt32LE(0) !== 0x04034b50 || local.readUInt16LE(6) !== flags ||
          local.readUInt16LE(8) !== method) return null;
      const localNameLength = local.readUInt16LE(26);
      const localExtraLength = local.readUInt16LE(28);
      const dataOffset = localOffset + 30 + localNameLength + localExtraLength;
      if (dataOffset + compressedSize > centralOffset) return null;
      const localNameBytes = Buffer.alloc(localNameLength);
      await handle.read(localNameBytes, 0, localNameBytes.length, localOffset + 30);
      if (!localNameBytes.equals(central.subarray(cursor + 46, cursor + 46 + nameLength))) return null;

      names.add(name);
      entries.push({ name, method, checksum, compressedSize, uncompressedSize, dataOffset });
      cursor += recordLength;
    }
  } catch {
    return null;
  }
  return cursor === central.length ? entries : null;
}

async function readZipEntry(handle, entry, maximum = MAX_ARCHIVE_ENTRY_BYTES) {
  if (!entry || entry.compressedSize > maximum || entry.uncompressedSize > maximum) return null;
  const compressed = Buffer.alloc(entry.compressedSize);
  await handle.read(compressed, 0, compressed.length, entry.dataOffset);
  try {
    const contents = entry.method === 0
      ? compressed
      : inflateRawSync(compressed, { maxOutputLength: maximum });
    return contents.length === entry.uncompressedSize && crc32(contents) === entry.checksum
      ? contents
      : null;
  } catch {
    return null;
  }
}

async function hasArchiveParts(handle, entries, format) {
  if (!Array.isArray(entries)) return false;
  const parts = new Map(entries.map((entry) => [entry.name, entry]));
  const required = {
    docx: ['[Content_Types].xml', 'word/document.xml'],
    pptx: ['[Content_Types].xml', 'ppt/presentation.xml'],
    xlsx: ['[Content_Types].xml', 'xl/workbook.xml'],
    odt: ['mimetype', 'content.xml', 'META-INF/manifest.xml'],
    odp: ['mimetype', 'content.xml', 'META-INF/manifest.xml'],
    ods: ['mimetype', 'content.xml', 'META-INF/manifest.xml'],
    epub: ['mimetype', 'META-INF/container.xml', 'OEBPS/content.opf', 'OEBPS/nav.xhtml']
  }[format];
  if (required && !required.every((part) => parts.has(part))) return false;
  if (required) {
    for (const part of required) {
      if (!await readZipEntry(handle, parts.get(part))) return false;
    }
  }
  const odfMime = {
    odt: 'application/vnd.oasis.opendocument.text',
    odp: 'application/vnd.oasis.opendocument.presentation',
    ods: 'application/vnd.oasis.opendocument.spreadsheet'
  }[format];
  if (odfMime) {
    const mimetype = await readZipEntry(handle, parts.get('mimetype'), 256);
    return mimetype?.toString('utf8') === odfMime;
  }
  if (format === 'epub') {
    if (entries[0]?.name !== 'mimetype' || entries[0]?.method !== 0 ||
        (await readZipEntry(handle, entries[0], 256))?.toString('utf8') !== 'application/epub+zip') return false;
    const pages = entries.filter((entry) => /^OEBPS\/page-\d{4}\.xhtml$/.test(entry.name));
    if (pages.length === 0) return false;
    for (const [index, entry] of pages.entries()) {
      if (entry.name !== `OEBPS/page-${String(index + 1).padStart(4, '0')}.xhtml` ||
          !await readZipEntry(handle, entry)) return false;
    }
    return true;
  }
  if (required) return true;
  if (format !== 'cbz') return false;
  if (entries.length === 0) return false;
  for (const [index, entry] of entries.entries()) {
    if (entry.name !== `${String(index + 1).padStart(4, '0')}.jpg`) return false;
    const image = await readZipEntry(handle, entry);
    if (!image || image.length < 4 || image[0] !== 0xff || image[1] !== 0xd8 ||
        image[image.length - 2] !== 0xff || image[image.length - 1] !== 0xd9) return false;
  }
  return true;
}

async function archivePageCount(file) {
  const info = await stat(file);
  const handle = await open(file, 'r');
  try {
    const entries = await zipEntries(handle, info.size);
    return await hasArchiveParts(handle, entries, 'cbz') ? entries.length : null;
  } finally {
    await handle.close();
  }
}

async function comicPageCount(file) {
  const info = await stat(file);
  if (info.size > 512 * 1024 * 1024) return null;
  const handle = await open(file, 'r');
  try {
    const entries = await zipEntries(handle, info.size);
    if (!entries || entries.length > 1024) return null;
    let pages = 0;
    let expanded = 0;
    for (const entry of entries) {
      expanded += entry.uncompressedSize;
      if (expanded > 512 * 1024 * 1024) return null;
      if (entry.name.endsWith('/')) {
        if (entry.uncompressedSize !== 0) return null;
        continue;
      }
      if (entry.name.toLowerCase() === 'comicinfo.xml') {
        if (entry.uncompressedSize > 1024 * 1024 || !await readZipEntry(handle, entry, 1024 * 1024)) return null;
        continue;
      }
      const extension = extname(entry.name).slice(1).toLowerCase();
      if (!['png', 'jpg', 'jpeg', 'bmp', 'gif', 'webp', 'tif', 'tiff'].includes(extension) ||
          ++pages > 200 || entry.uncompressedSize > MAX_ARCHIVE_ENTRY_BYTES) return null;
      const image = await readZipEntry(handle, entry);
      if (!image || !imageSignature(image, extension)) return null;
    }
    return pages || null;
  } finally {
    await handle.close();
  }
}

function imageSignature(image, extension) {
  if (extension === 'png') return image.subarray(0, 8).equals(Buffer.from('89504e470d0a1a0a', 'hex'));
  if (extension === 'jpg' || extension === 'jpeg') return image.length >= 4 &&
    image[0] === 0xff && image[1] === 0xd8 && image[image.length - 2] === 0xff && image[image.length - 1] === 0xd9;
  if (extension === 'bmp') return image.length >= 54 && image.subarray(0, 2).toString() === 'BM' && image.readUInt32LE(2) === image.length;
  if (extension === 'gif') return ['GIF87a', 'GIF89a'].includes(image.subarray(0, 6).toString());
  if (extension === 'webp') return image.length >= 20 && image.subarray(0, 4).toString() === 'RIFF' && image.subarray(8, 12).toString() === 'WEBP';
  return image.subarray(0, 4).equals(Buffer.from('49492a00', 'hex')) ||
    image.subarray(0, 4).equals(Buffer.from('4d4d002a', 'hex'));
}

async function htmlArchivePageCount(file) {
  const info = await stat(file);
  if (info.size > 512 * 1024 * 1024) return null;
  const handle = await open(file, 'r');
  try {
    const entries = await zipEntries(handle, info.size);
    if (!entries || entries.length < 3 || entries.length > 202 ||
        entries[0].name !== 'index.html' || entries[1].name !== 'styles.css') return null;
    const htmlBytes = await readZipEntry(handle, entries[0], 1024 * 1024);
    const cssBytes = await readZipEntry(handle, entries[1], 1024 * 1024);
    if (!htmlBytes || !cssBytes) return null;
    const html = new TextDecoder('utf-8', { fatal: true }).decode(htmlBytes);
    const css = new TextDecoder('utf-8', { fatal: true }).decode(cssBytes);
    if (!html.includes("default-src 'none'") || !html.includes('href="styles.css"') ||
        /<script\b|https?:\/\/|<iframe\b/i.test(html) || /@import|url\s*\(/i.test(css)) return null;
    for (let index = 2; index < entries.length; index++) {
      const name = `pages/page-${String(index - 1).padStart(4, '0')}.png`;
      if (entries[index].name !== name || !html.includes(`src="${name}"`)) return null;
      const image = await readZipEntry(handle, entries[index]);
      if (!image || !imageSignature(image, 'png')) return null;
    }
    return entries.length - 2;
  } catch {
    return null;
  } finally {
    await handle.close();
  }
}

function xmlNameAt(source, offset) {
  return /^[A-Za-z_][A-Za-z0-9_.:-]*/.exec(source.slice(offset))?.[0] ?? null;
}

function decodeXmlEntities(source) {
  let output = '';
  for (let index = 0; index < source.length;) {
    if (source[index] !== '&') {
      if (source[index] === '<' || source.charCodeAt(index) === 0) return null;
      output += source[index++];
      continue;
    }
    const end = source.indexOf(';', index + 1);
    if (end < 0) return null;
    const entity = source.slice(index + 1, end);
    const named = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'" }[entity];
    let value = named;
    if (value === undefined && /^#\d+$/.test(entity)) value = Number(entity.slice(1));
    if (value === undefined && /^#x[\da-f]+$/i.test(entity)) value = Number.parseInt(entity.slice(2), 16);
    if (typeof value === 'number') {
      if (!Number.isSafeInteger(value) || value <= 0 || value > 0x10ffff ||
          (value >= 0xd800 && value <= 0xdfff)) return null;
      value = String.fromCodePoint(value);
    }
    if (value === undefined) return null;
    output += value;
    index = end + 1;
  }
  return output;
}

function parseXmlStartTag(source) {
  let cursor = 0;
  const skipSpace = () => {
    while (/\s/.test(source[cursor] ?? '')) cursor++;
  };
  skipSpace();
  const name = xmlNameAt(source, cursor);
  if (!name) return null;
  cursor += name.length;
  const attributes = new Map();
  while (cursor < source.length) {
    skipSpace();
    if (source[cursor] === '/' && source.slice(cursor + 1).trim() === '') {
      return { name, attributes, empty: true };
    }
    if (cursor >= source.length) return { name, attributes, empty: false };
    const attributeName = xmlNameAt(source, cursor);
    if (!attributeName || attributes.has(attributeName)) return null;
    cursor += attributeName.length;
    skipSpace();
    if (source[cursor++] !== '=') return null;
    skipSpace();
    const quote = source[cursor++];
    if (quote !== '"' && quote !== "'") return null;
    const end = source.indexOf(quote, cursor);
    if (end < 0) return null;
    const value = decodeXmlEntities(source.slice(cursor, end));
    if (value === null) return null;
    attributes.set(attributeName, value);
    cursor = end + 1;
  }
  return { name, attributes, empty: false };
}

function xmlTagEnd(source, offset) {
  let quote = null;
  for (let index = offset; index < source.length; index++) {
    const character = source[index];
    if (quote !== null) {
      if (character === quote) quote = null;
    } else if (character === '"' || character === "'") {
      quote = character;
    } else if (character === '>') {
      return index;
    }
  }
  return -1;
}

function isFlatOdtXml(contents) {
  const officeNamespace = 'urn:oasis:names:tc:opendocument:xmlns:office:1.0';
  const stack = [];
  let cursor = 0;
  let rootSeen = false;
  let rootClosed = false;
  let hasBody = false;
  let hasText = false;
  while (cursor < contents.length) {
    const start = contents.indexOf('<', cursor);
    const text = contents.slice(cursor, start < 0 ? contents.length : start);
    if ((stack.length === 0 && text.trim() !== '') ||
        (stack.length > 0 && decodeXmlEntities(text) === null)) return false;
    if (start < 0) break;
    if (contents.startsWith('<!--', start)) {
      const end = contents.indexOf('-->', start + 4);
      if (end < 0 || contents.slice(start + 4, end).includes('--')) return false;
      cursor = end + 3;
      continue;
    }
    if (contents.startsWith('<![CDATA[', start)) {
      if (stack.length === 0) return false;
      const end = contents.indexOf(']]>', start + 9);
      if (end < 0) return false;
      cursor = end + 3;
      continue;
    }
    if (contents.startsWith('<?', start)) {
      const end = contents.indexOf('?>', start + 2);
      if (end < 0) return false;
      cursor = end + 2;
      continue;
    }
    if (contents.startsWith('<!', start)) return false;
    const end = xmlTagEnd(contents, start + 1);
    if (end < 0) return false;
    const raw = contents.slice(start + 1, end);
    if (raw.startsWith('/')) {
      const name = raw.slice(1).trim();
      if (!xmlNameAt(name, 0) || xmlNameAt(name, 0) !== name || stack.pop() !== name) return false;
      if (stack.length === 0) rootClosed = true;
    } else {
      if (rootClosed) return false;
      const tag = parseXmlStartTag(raw);
      if (!tag) return false;
      if (stack.length === 0) {
        if (rootSeen || tag.empty || tag.name !== 'office:document' ||
            tag.attributes.get('xmlns:office') !== officeNamespace ||
            tag.attributes.get('office:mimetype') !== 'application/vnd.oasis.opendocument.text' ||
            !['1.2', '1.3', '1.4'].includes(tag.attributes.get('office:version'))) return false;
        rootSeen = true;
      } else if (stack.length === 1 && stack[0] === 'office:document' && tag.name === 'office:body') {
        hasBody = true;
      } else if (stack.length === 2 && stack[1] === 'office:body' && tag.name === 'office:text') {
        hasText = true;
      }
      for (const [name, value] of tag.attributes) {
        if (name === 'xmlns:office' && value !== officeNamespace) return false;
        if (name.split(':').at(-1) === 'href' && value !== '' && !value.startsWith('#')) return false;
      }
      if (!tag.empty) stack.push(tag.name);
    }
    cursor = end + 1;
  }
  return rootSeen && rootClosed && stack.length === 0 && hasBody && hasText;
}

async function sniff(file, format, source = false) {
  const info = await stat(file);
  if (!info.isFile() || info.size === 0) return false;
  const handle = await open(file, 'r');
  try {
    const head = Buffer.alloc(Math.min(info.size, 32));
    await handle.read(head, 0, head.length, 0);
    if (format === 'zip') return source ? await comicPageCount(file) !== null : await htmlArchivePageCount(file) !== null;
    if (format === 'cbz' && source) return await comicPageCount(file) !== null;
    if (format === 'pdf') return head.subarray(0, 5).toString() === '%PDF-';
    if (format === 'html' || format === 'htm') {
      if (info.size > 4 * 1024 * 1024) return false;
      const bytes = await readFile(file);
      if (bytes.includes(0)) return false;
      try {
        return new TextDecoder('utf-8', { fatal: true }).decode(bytes).trim().length > 0;
      } catch {
        return false;
      }
    }
    if (format === 'png') return head.subarray(0, 8).equals(Buffer.from('89504e470d0a1a0a', 'hex'));
    if (format === 'jpg' || format === 'jpeg') {
      const tail = Buffer.alloc(2);
      await handle.read(tail, 0, 2, info.size - 2);
      return head[0] === 0xff && head[1] === 0xd8 && tail[0] === 0xff && tail[1] === 0xd9;
    }
    if (format === 'bmp') {
      return info.size >= 54 && head.subarray(0, 2).toString() === 'BM' &&
        head.readUInt32LE(2) === info.size && head.readUInt32LE(10) >= 54;
    }
    if (format === 'gif') {
      const tail = Buffer.alloc(1);
      await handle.read(tail, 0, 1, info.size - 1);
      return ['GIF87a', 'GIF89a'].includes(head.subarray(0, 6).toString()) && tail[0] === 0x3b;
    }
    if (format === 'webp') {
      return info.size >= 20 && head.subarray(0, 4).toString() === 'RIFF' &&
        head.readUInt32LE(4) + 8 === info.size && head.subarray(8, 12).toString() === 'WEBP' &&
        ['VP8 ', 'VP8L', 'VP8X'].includes(head.subarray(12, 16).toString());
    }
    if (format === 'tif' || format === 'tiff') {
      if (info.size < 14) return false;
      const littleEndian = head.subarray(0, 4).equals(Buffer.from('49492a00', 'hex'));
      const bigEndian = head.subarray(0, 4).equals(Buffer.from('4d4d002a', 'hex'));
      if (!littleEndian && !bigEndian) return false;
      const read16 = littleEndian ? Buffer.prototype.readUInt16LE : Buffer.prototype.readUInt16BE;
      const read32 = littleEndian ? Buffer.prototype.readUInt32LE : Buffer.prototype.readUInt32BE;
      const firstIfd = read32.call(head, 4);
      if (firstIfd < 8 || firstIfd + 2 > info.size) return false;
      const countBytes = Buffer.alloc(2);
      await handle.read(countBytes, 0, 2, firstIfd);
      const entryCount = read16.call(countBytes, 0);
      return entryCount > 0 && entryCount <= 256 && firstIfd + 2 + entryCount * 12 + 4 <= info.size;
    }
    if (format === 'rtf') {
      if (info.size < 7 || head.subarray(0, 6).toString() !== '{\\rtf1') return false;
      const tail = Buffer.alloc(Math.min(info.size, 4096));
      await handle.read(tail, 0, tail.length, info.size - tail.length);
      return tail.toString().trimEnd().endsWith('}');
    }
    if (format === 'xml') {
      if (info.size > MAX_FLAT_ODT_XML_BYTES) return false;
      const bytes = Buffer.alloc(info.size);
      await handle.read(bytes, 0, bytes.length, 0);
      try {
        return isFlatOdtXml(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
      } catch {
        return false;
      }
    }
    if (['docx', 'odt', 'pptx', 'odp', 'xlsx', 'ods', 'epub', 'cbz'].includes(format)) {
      if (!head.subarray(0, 4).equals(Buffer.from('504b0304', 'hex'))) return false;
      return await hasArchiveParts(handle, await zipEntries(handle, info.size), format);
    }
    if (format === 'txt' || format === 'md') {
      const decoder = new TextDecoder('utf-8', { fatal: true });
      for await (const chunk of createReadStream(file)) decoder.decode(chunk, { stream: true });
      decoder.decode();
      return true;
    }
    return true;
  } finally {
    await handle.close();
  }
}

async function checkArtifact(errors, location, base, artifact, format, source = false) {
  if (!isObject(artifact) || !isString(artifact.file) || !shaPattern.test(artifact.sha256 ?? '')) {
    issue(errors, location, 'file and lowercase SHA-256 are required');
    return null;
  }
  try {
    const file = await insidePath(base, artifact.file);
    if (format && extname(file).toLowerCase() !== `.${format}`) {
      issue(errors, location, `expected .${format} extension`);
      return null;
    }
    if (format && !await sniff(file, format, source)) issue(errors, location, `invalid or empty ${format} file`);
    if (await hashFile(file) !== artifact.sha256) issue(errors, location, 'SHA-256 mismatch');
    return file;
  } catch (error) {
    issue(errors, location, error.message);
    return null;
  }
}

function normaliseText(value) {
  return value.normalize('NFKC').replace(/\p{White_Space}/gu, '');
}

function textRetention(reference, actual) {
  const expected = [...normaliseText(reference)];
  const found = [...normaliseText(actual)];
  if (expected.length === 0) return null;
  const counts = new Map();
  for (const char of found) counts.set(char, (counts.get(char) ?? 0) + 1);
  let matched = 0;
  for (const char of expected) {
    if ((counts.get(char) ?? 0) > 0) {
      matched++;
      counts.set(char, counts.get(char) - 1);
    }
  }
  return { matched, expected: expected.length, actual: found.length, retention: matched / expected.length };
}

function expectedOperations(document) {
  return operationsByFormat[document.format] ?? [];
}

async function checkDocument(document, index, base, errors) {
  const location = `documents[${index}]`;
  if (!isObject(document)) {
    issue(errors, location, 'expected a document object');
    return;
  }
  requireValue(errors, `${location}.id`, document.id, (v) => typeof v === 'string' && /^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(v), 'expected a stable lowercase ID');
  requireValue(errors, `${location}.title`, document.title, isString, 'document title is required');
  requireValue(errors, `${location}.language`, document.language, (v) => ['zh', 'en'].includes(v), 'expected zh or en');
  requireValue(errors, `${location}.format`, document.format, (v) => Object.hasOwn(operationsByFormat, v), `expected one of ${Object.keys(operationsByFormat).join(', ')}`);
  const categories = document.categories;
  const isProtected = Array.isArray(categories) && categories.includes('protected');
  const allowedCategories = ['pptx', 'odp'].includes(document.format)
    ? presentationCategories
    : ['xlsx', 'ods'].includes(document.format)
      ? spreadsheetCategories
      : pdfCategories;
  if (!Array.isArray(categories) || categories.length === 0 || categories.some((v) => !allowedCategories.includes(v))) {
    issue(errors, `${location}.categories`, `expected one or more of ${allowedCategories.join(', ')}`);
  }
  const rights = document.rights;
  if (!isObject(rights)) {
    issue(errors, `${location}.rights`, 'source and permission evidence are required');
  } else {
    requireValue(errors, `${location}.rights.basis`, rights.basis, (v) => ['public_license', 'written_permission'].includes(v), 'expected public_license or written_permission');
    requireValue(errors, `${location}.rights.creator`, rights.creator, isString, 'creator is required');
    requireValue(errors, `${location}.rights.verifiedBy`, rights.verifiedBy, isString, 'named human permission verifier is required');
    requireValue(errors, `${location}.rights.license`, rights.license, isString, 'specific license or written permission scope is required');
    requireValue(errors, `${location}.rights.checkedAt`, rights.checkedAt, isDate, 'valid, non-future verification date is required');
    if (rights.basis === 'public_license') {
      requireValue(errors, `${location}.rights.sourceUrl`, rights.sourceUrl, isHttps, 'HTTPS source URL is required');
      requireValue(errors, `${location}.rights.licenseUrl`, rights.licenseUrl, isHttps, 'HTTPS license/source declaration URL is required');
    }
    if (rights.basis === 'written_permission') {
      requireValue(errors, `${location}.rights.grantor`, rights.grantor, isString, 'permission grantor is required');
      requireValue(errors, `${location}.rights.sourceDescription`, rights.sourceDescription, isString, 'private source provenance is required');
    }
    await checkArtifact(errors, `${location}.rights.evidence`, base, rights.evidence);
  }
  const input = await checkArtifact(errors, `${location}.input`, base, document.input, document.format, true);
  const needsText = ['pdf', 'docx', 'odt', 'txt', 'rtf', 'html', 'htm', 'md', 'markdown', 'xlsx', 'ods'].includes(document.format) ||
    (['pptx', 'odp'].includes(document.format) && categories?.includes?.('presentation_text'));
  if (isProtected || !needsText) {
    if (document.textReference !== undefined) await checkArtifact(errors, `${location}.textReference`, base, document.textReference, 'txt');
  } else {
    const text = await checkArtifact(errors, `${location}.textReference`, base, document.textReference, 'txt');
    if (text && !normaliseText(await readFile(text, 'utf8')).length) issue(errors, `${location}.textReference`, 'must contain reference text');
  }
  const expectedFailures = document.expectedFailures ?? {};
  if (!isObject(expectedFailures) || Object.keys(expectedFailures).some((kind) => !expectedOperations(document).includes(kind))) {
    issue(errors, `${location}.expectedFailures`, 'unexpected operation');
  } else if (Object.keys(expectedFailures).length && !isProtected) {
    issue(errors, `${location}.expectedFailures`, 'only protected documents may have expected conversion failures');
  } else {
    for (const [kind, codes] of Object.entries(expectedFailures)) {
      if (!Array.isArray(codes) || codes.length === 0 || codes.some((code) => !/^[A-Z][A-Z_]+$/.test(code))) {
        issue(errors, `${location}.expectedFailures.${kind}`, 'expected a nonempty list of stable error codes');
      }
    }
  }
  return input;
}

async function checkResult(result, document, run, base, corpus, errors) {
  const location = `${run.platform}/${document?.id ?? '?'}/${result.kind}`;
  const expectedErrors = document?.expectedFailures?.[result.kind];
  requireValue(errors, `${location}.inputSha256`, result.inputSha256, (v) => v === document?.input?.sha256, 'input checksum differs from the approved sample');
  requireValue(errors, `${location}.elapsedMs`, result.elapsedMs, (v) => Number.isFinite(v) && v > 0 && v <= 900_000, 'measured elapsed time must be within 15 minutes');
  requireValue(errors, `${location}.peakRssMiB`, result.peakRssMiB, (v) => Number.isFinite(v) && v > 0 && v <= 1024, 'measured peak resident memory must not exceed 1024 MiB');
  const traceFile = await checkArtifact(errors, `${location}.measurementEvidence`, base, result.measurementEvidence, 'json');
  if (traceFile) {
    try {
      const trace = JSON.parse(await readFile(traceFile, 'utf8'));
      requireValue(errors, `${location}.measurementEvidence.method`, trace.method, isString, 'OS process-tree sampling method is required');
      const timestamp = (value) => typeof value === 'string' &&
        /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(value) &&
        !Number.isNaN(Date.parse(value)) && new Date(value).toISOString() === value;
      if (!timestamp(trace.startedAt) || !timestamp(trace.finishedAt) ||
          Date.parse(trace.finishedAt) - Date.parse(trace.startedAt) !== result.elapsedMs) {
        issue(errors, `${location}.measurementEvidence`, 'reported duration differs from the recorded timestamps');
      }
      if (!Array.isArray(trace.rssSamplesMiB) || trace.rssSamplesMiB.length < 2 ||
          trace.rssSamplesMiB.some((sample) => !Number.isFinite(sample) || sample <= 0) ||
          trace.rssSamplesMiB.reduce((maximum, sample) => Math.max(maximum, sample), 0) !== result.peakRssMiB) {
        issue(errors, `${location}.measurementEvidence`, 'reported peak RSS differs from at least two positive process-tree samples');
      }
    } catch (error) {
      issue(errors, `${location}.measurementEvidence`, `invalid JSON: ${error.message}`);
    }
  }
  if (result.state === 'failed') {
    if (!Array.isArray(expectedErrors) || !expectedErrors.includes(result.errorCode)) {
      issue(errors, `${location}.errorCode`, 'unexpected conversion failure');
    }
    requireValue(errors, `${location}.failureNote`, result.failureNote, isString, 'reviewer must explain an expected failure');
    if (result.outputs?.length) issue(errors, `${location}.outputs`, 'failed jobs must not leave final outputs');
    return;
  }
  if (result.state !== 'succeeded' || expectedErrors) {
    issue(errors, `${location}.state`, 'expected successful conversion or explicitly approved protected-file failure');
    return;
  }
  const format = outputFormatByOperation[result.kind];
  const outputFiles = [];
  if (!Array.isArray(result.outputs) || !result.outputs.length) {
    issue(errors, `${location}.outputs`, 'at least one output is required');
  } else {
    for (const [index, output] of result.outputs.entries()) {
      const file = await checkArtifact(errors, `${location}.outputs[${index}]`, base, output, format);
      if (file) outputFiles.push(file);
    }
  }
  const review = result.review;
  if (!isObject(review)) {
    issue(errors, `${location}.review`, 'human full-document visual review is required');
    return;
  }
  requireValue(errors, `${location}.review.reviewer`, review.reviewer, isString, 'reviewer is required');
  requireValue(errors, `${location}.review.reviewedAt`, review.reviewedAt, isDate, 'review date is required');
  requireValue(errors, `${location}.review.notes`, review.notes, isString, 'specific visual observations are required');
  requireValue(errors, `${location}.review.layoutSeverity`, review.layoutSeverity, (v) => Number.isInteger(v) && v >= 0 && v <= 1, 'only 0 (unchanged) or 1 (minor) is release-acceptable');
  requireValue(errors, `${location}.review.accepted`, review.accepted, (v) => v === true, 'reviewer must accept the result');
  requireValue(errors, `${location}.review.sourcePages`, review.sourcePages, (v) => Number.isInteger(v) && v > 0, 'positive source page count is required');
  requireValue(errors, `${location}.review.outputPages`, review.outputPages, (v) => Number.isInteger(v) && v > 0, 'positive output page count is required');
  const requiredPages = Math.max(review.sourcePages ?? 0, review.outputPages ?? 0);
  requireValue(errors, `${location}.review.pagesReviewed`, review.pagesReviewed, (v) => Number.isInteger(v) && v >= requiredPages, 'all source and output pages must be visually reviewed');
  if (['png', 'jpg', 'jpeg', 'bmp', 'gif', 'webp', 'tif', 'tiff', 'svg'].includes(document.format) && review.sourcePages !== 1) {
    issue(errors, `${location}.review.sourcePages`, 'each input image has exactly one source page');
  }
  if (oneOutputOperations.has(result.kind) && result.outputs?.length !== 1) {
    issue(errors, `${location}.outputs`, 'this conversion must create exactly one output file');
  }
  if (pagedImageOperations.has(result.kind)) {
    if (result.outputs?.length !== review.sourcePages || review.outputPages !== review.sourcePages) {
      issue(errors, `${location}.outputs`, 'one image per source page is required, in page order');
    }
  } else if (exactPageOperations.has(result.kind) && review.outputPages !== review.sourcePages) {
    issue(errors, `${location}.review.outputPages`, 'one destination page or slide per source page is required');
  } else if (flowingDocumentOperations.has(result.kind) && Math.abs((review.outputPages ?? 0) - (review.sourcePages ?? 0)) > 1) {
    issue(errors, `${location}.review.outputPages`, 'page count differs by more than one');
  }
  if (result.kind === 'pdf_to_cbz' && outputFiles.length === 1) {
    const archivedPages = await archivePageCount(outputFiles[0]);
    if (archivedPages !== review.sourcePages) {
      issue(errors, `${location}.outputs[0]`, 'CBZ must contain one sequentially numbered JPEG per source page');
    }
  }
  if (result.kind === 'pdf_to_html' && outputFiles.length === 1) {
    if (await htmlArchivePageCount(outputFiles[0]) !== review.sourcePages) {
      issue(errors, `${location}.outputs[0]`, 'HTML ZIP must contain one referenced PNG per source page');
    }
  }
  if (result.kind === 'comic_to_pdf') {
    try {
      const comicInput = await insidePath(corpus, document.input.file);
      if (await comicPageCount(comicInput) !== review.sourcePages) {
        issue(errors, `${location}.review.sourcePages`, 'comic ZIP must contain one image per source page');
      }
    } catch (error) {
      issue(errors, `${location}.review.sourcePages`, `cannot verify comic pages: ${error.message}`);
    }
  }
  for (const side of ['sourceImages', 'resultImages']) {
    if (!Array.isArray(review[side]) || review[side].length < (side === 'sourceImages' ? review.sourcePages : review.outputPages)) {
      issue(errors, `${location}.review.${side}`, 'screenshot evidence is required for every page');
      continue;
    }
    for (const [index, evidence] of review[side].entries()) {
      if (!['png', 'jpg'].includes(extname(evidence?.file ?? '').slice(1).toLowerCase())) issue(errors, `${location}.review.${side}[${index}]`, 'expected PNG or JPG screenshot');
      else await checkArtifact(errors, `${location}.review.${side}[${index}]`, base, evidence, extname(evidence.file).slice(1).toLowerCase());
    }
    if (new Set(review[side].map((evidence) => evidence?.sha256)).size !== review[side].length) {
      issue(errors, `${location}.review.${side}`, 'each reviewed page needs distinct screenshot evidence');
    }
  }
  const needsOutputText = textOperations.has(result.kind) ||
    (['pptx_to_pdf', 'odp_to_pdf'].includes(result.kind) && document.categories?.includes('presentation_text'));
  if (!needsOutputText) return;
  requireValue(errors, `${location}.review.textOrderPass`, review.textOrderPass, (v) => v === true, 'reading order must be checked and accepted');
  const directTextOutput = result.kind === 'pdf_to_txt' || result.kind === 'pdf_to_markdown';
  const outputText = directTextOutput
    ? result.outputs?.[0]
    : result.outputText;
  if (!directTextOutput) {
    requireValue(errors, `${location}.outputTextMethod`, result.outputTextMethod, isString, 'independent output text extraction method is required');
  }
  const textFile = await checkArtifact(
    errors,
    `${location}.outputText`,
    base,
    outputText,
    result.kind === 'pdf_to_markdown' ? 'md' : 'txt'
  );
  try {
    const reference = await insidePath(corpus, document.textReference.file);
    if (!textFile) return;
    const measure = textRetention(await readFile(reference, 'utf8'), await readFile(textFile, 'utf8'));
    if (!measure || measure.retention < 0.98) {
      issue(errors, `${location}.textRetention`, `${measure ? (measure.retention * 100).toFixed(2) : 0}% is below the 98% minimum`);
    }
    return measure;
  } catch (error) {
    issue(errors, `${location}.textRetention`, error.message);
  }
}

export async function audit(manifest, records, corpus, artifacts, { strictCoverage = true } = {}) {
  const errors = [];
  if (manifest?.schemaVersion !== 1 || !Array.isArray(manifest.documents)) {
    return { errors: ['manifest: expected schemaVersion 1 and documents array'], metrics: [] };
  }
  if (records?.schemaVersion !== 1 || !Array.isArray(records.runs)) {
    return { errors: ['records: expected schemaVersion 1 and runs array'], metrics: [] };
  }
  requireValue(errors, 'manifest.releaseVersion', manifest.releaseVersion, isString, 'candidate release version is required');
  const documents = new Map();
  const hashes = new Set();
  for (const [index, document] of manifest.documents.entries()) {
    await checkDocument(document, index, corpus, errors);
    if (!isObject(document)) continue;
    if (documents.has(document.id)) issue(errors, `documents[${index}].id`, 'duplicate sample ID');
    documents.set(document.id, document);
    if (hashes.has(document.input?.sha256)) issue(errors, `documents[${index}].input`, 'duplicate document bytes do not count as a distinct sample');
    hashes.add(document.input?.sha256);
  }
  if (strictCoverage) {
    const chinesePdfs = [...documents.values()].filter((doc) => doc.language === 'zh' && doc.format === 'pdf');
    if (chinesePdfs.length < 30) issue(errors, 'coverage', `requires >=30 distinct real Chinese PDFs for PDF to Word, found ${chinesePdfs.length}`);
    if (![...documents.values()].some((doc) => doc.language === 'en')) issue(errors, 'coverage', 'requires English documents');
    for (const format of ['docx', 'odt', 'txt', 'rtf', 'xlsx', 'ods']) {
      if (![...documents.values()].some((doc) => doc.format === format)) {
        issue(errors, 'coverage', `requires real ${format.toUpperCase()} documents for ${format.toUpperCase()} to PDF`);
      }
    }
    for (const format of ['png', 'jpg', 'bmp', 'gif', 'webp', 'tiff']) {
      if (![...documents.values()].some((doc) => doc.format === format ||
          (format === 'jpg' && doc.format === 'jpeg') ||
          (format === 'tiff' && doc.format === 'tif'))) {
        issue(errors, 'coverage', `requires real ${format.toUpperCase()} images for image to PDF`);
      }
    }
    if (![...documents.values()].some((doc) => doc.format === 'svg')) {
      issue(errors, 'coverage', 'requires real static SVG vector artwork for SVG to PDF');
    }
    if (![...documents.values()].some((doc) => ['html', 'htm'].includes(doc.format))) {
      issue(errors, 'coverage', 'requires real offline HTML documents for HTML to PDF');
    }
    if (![...documents.values()].some((doc) => ['md', 'markdown'].includes(doc.format))) {
      issue(errors, 'coverage', 'requires real Markdown documents for Markdown to PDF');
    }
    if (![...documents.values()].some((doc) => doc.format === 'epub')) {
      issue(errors, 'coverage', 'requires real EPUB books for EPUB to PDF');
    }
    if (![...documents.values()].some((doc) => ['cbz', 'zip'].includes(doc.format))) {
      issue(errors, 'coverage', 'requires a real CBZ or ZIP image archive for comic to PDF');
    }
    for (const format of ['pptx', 'odp']) {
      for (const category of presentationCategories) {
        if (![...documents.values()].some((doc) => doc.format === format && doc.categories?.includes(category))) {
          issue(errors, 'coverage', `requires real ${format.toUpperCase()} presentations with ${category}`);
        }
      }
    }
    for (const format of ['xlsx', 'ods']) {
      for (const category of spreadsheetCategories.filter((value) => value !== 'protected')) {
        if (![...documents.values()].some((doc) => doc.format === format && doc.categories?.includes(category))) {
          issue(errors, 'coverage', `requires real ${format.toUpperCase()} spreadsheets with ${category}`);
        }
      }
    }
    for (const category of pdfCategories) {
      if (!chinesePdfs.some((doc) => Array.isArray(doc.categories) && doc.categories.includes(category))) issue(errors, 'coverage', `Chinese PDFs must include ${category}`);
    }
  }
  const seenPlatforms = new Set();
  const metrics = [];
  const summary = new Map();
  for (const [index, run] of records.runs.entries()) {
    if (!isObject(run)) { issue(errors, `runs[${index}]`, 'expected an object'); continue; }
    if (!platforms.includes(run.platform)) issue(errors, `runs[${index}].platform`, `expected ${platforms.join(', ')}`);
    if (seenPlatforms.has(run.platform)) issue(errors, `runs[${index}].platform`, 'duplicate platform run');
    seenPlatforms.add(run.platform);
    requireValue(errors, `${run.platform}.osVersion`, run.osVersion, isString, 'OS version is required');
    requireValue(errors, `${run.platform}.appVersion`, run.appVersion, (v) => isString(v) && v === manifest.releaseVersion, 'run must match the candidate release version');
    requireValue(errors, `${run.platform}.appBinarySha256`, run.appBinarySha256, (v) => shaPattern.test(v ?? ''), 'app executable SHA-256 is required');
    requireValue(errors, `${run.platform}.appPackageSha256`, run.appPackageSha256, (v) => shaPattern.test(v ?? ''), 'signed installer or app package SHA-256 is required');
    requireValue(errors, `${run.platform}.operator`, run.operator, isString, 'named test operator is required');
    requireValue(errors, `${run.platform}.execution`, run.execution, (v) => v === 'desktop_ui', 'only actual desktop UI conversions count');
    requireValue(errors, `${run.platform}.reviewedAt`, run.reviewedAt, isDate, 'run date is required');
    if (!Array.isArray(run.results)) { issue(errors, `${run.platform}.results`, 'results array is required'); continue; }
    const seen = new Set();
    for (const [resultIndex, result] of run.results.entries()) {
      if (!isObject(result)) { issue(errors, `${run.platform}.results[${resultIndex}]`, 'expected an object'); continue; }
      const document = documents.get(result.documentId);
      if (!document) { issue(errors, `${run.platform}.results[${resultIndex}]`, 'unknown document ID'); continue; }
      if (!expectedOperations(document).includes(result.kind)) { issue(errors, `${run.platform}/${result.documentId}`, 'unexpected conversion kind'); continue; }
      const key = `${result.documentId}/${result.kind}`;
      if (seen.has(key)) { issue(errors, `${run.platform}/${key}`, 'duplicate conversion record'); continue; }
      seen.add(key);
      const group = `${run.platform}/${result.kind}`;
      if (!summary.has(group)) summary.set(group, { attempted: 0, succeeded: 0, expectedFailure: 0, unexpected: 0, durations: [], memory: [] });
      const totals = summary.get(group);
      totals.attempted++;
      const expectedCodes = document.expectedFailures?.[result.kind];
      if (result.state === 'succeeded' && !expectedCodes) totals.succeeded++;
      else if (result.state === 'failed' && Array.isArray(expectedCodes) && expectedCodes.includes(result.errorCode)) totals.expectedFailure++;
      else totals.unexpected++;
      if (Number.isFinite(result.elapsedMs)) totals.durations.push(result.elapsedMs);
      if (Number.isFinite(result.peakRssMiB)) totals.memory.push(result.peakRssMiB);
      const uiImage = result.uiEvidence;
      const uiFormat = extname(uiImage?.file ?? '').slice(1).toLowerCase();
      if (!['png', 'jpg'].includes(uiFormat)) issue(errors, `${run.platform}/${key}.uiEvidence`, 'desktop task state screenshot must be PNG or JPG');
      else await checkArtifact(errors, `${run.platform}/${key}.uiEvidence`, artifacts, uiImage, uiFormat);
      const value = await checkResult(result, document, run, artifacts, corpus, errors);
      if (value) metrics.push({ platform: run.platform, documentId: document.id, operation: result.kind, ...value });
    }
    for (const document of documents.values()) {
      for (const operation of expectedOperations(document)) {
        if (!seen.has(`${document.id}/${operation}`)) issue(errors, `${run.platform}/${document.id}/${operation}`, 'missing conversion result');
      }
    }
  }
  if (strictCoverage) {
    for (const platform of platforms) if (!seenPlatforms.has(platform)) issue(errors, 'coverage', `missing real ${platform} desktop run`);
  }
  return { errors, metrics, summary: [...summary], sampleCount: documents.size, platforms: [...seenPlatforms] };
}

function p95(values) {
  if (!values.length) return 'n/a';
  const sorted = [...values].sort((a, b) => a - b);
  return String(sorted[Math.ceil(sorted.length * 0.95) - 1]);
}

async function main() {
  const args = process.argv.slice(2);
  if (args.length !== 4 || args[0] !== '--manifest' || args[2] !== '--records') {
    throw new Error('Usage: node scripts/real-quality-check.mjs --manifest /absolute/private-corpus/manifest.json --records /absolute/private-corpus/results.json');
  }
  const [manifestFile, recordsFile] = [args[1], args[3]];
  if (![manifestFile, recordsFile].every(isAbsolute)) throw new Error('Manifest and records paths must be absolute');
  const [manifest, records] = await Promise.all([readFile(manifestFile, 'utf8'), readFile(recordsFile, 'utf8')]);
  const result = await audit(JSON.parse(manifest), JSON.parse(records), dirname(manifestFile), dirname(recordsFile));
  for (const line of result.errors) console.error(`FAIL ${line}`);
  for (const row of result.metrics) console.log(`TEXT ${row.platform} ${row.documentId}/${row.operation}: ${(row.retention * 100).toFixed(2)}% (${row.matched}/${row.expected} non-whitespace Unicode characters)`);
  for (const [group, totals] of result.summary) {
    console.log(`RESULT ${group}: ${totals.succeeded}/${totals.attempted} succeeded, ${totals.expectedFailure} expected rejections, ${totals.unexpected} unexpected; p95 ${p95(totals.durations)} ms, ${p95(totals.memory)} MiB RSS`);
  }
  console.log(`${result.sampleCount ?? 0} real document entries, ${result.platforms?.length ?? 0}/3 platform runs; ${result.errors.length} gate failures`);
  if (result.errors.length) process.exitCode = 1;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await main();
}
