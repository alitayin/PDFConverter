'use strict';

const { app, BrowserWindow, session } = require('electron');
const { constants } = require('node:fs');
const { open, link, lstat, mkdtemp, rm, stat } = require('node:fs/promises');
const { dirname, extname, isAbsolute, join } = require('node:path');

const MAX_HTML_BYTES = 4 * 1024 * 1024;
const MAX_PDF_BYTES = 64 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS = 30_000;
const CSP = "default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; script-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; media-src 'none'; form-action 'none'; base-uri 'none'";
let active = false;

function assertActive(signal) {
  if (signal.aborted) throw new Error(signal.reason);
}

function secureHtml(html) {
  const head = `<meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="${CSP}"><title>Local HTML</title>`;
  const doctype = /^\uFEFF?\s*<!doctype\s+html\s*>/i.exec(html);
  return doctype ? `${doctype[0]}${head}${html.slice(doctype[0].length)}` : `<!doctype html>${head}${html}`;
}

function allowedInlineAsset(details) {
  if (details.resourceType === 'image') {
    return /^data:image\/(?:png|jpeg|gif|webp|bmp);base64,/i.test(details.url);
  }
  if (details.resourceType === 'font') {
    return /^data:(?:font\/(?:woff2?|ttf|otf)|application\/(?:font-woff|x-font-ttf));base64,/i.test(details.url);
  }
  return false;
}

async function readInput(path, signal) {
  const original = await lstat(path);
  if (!original.isFile() || original.isSymbolicLink() || original.size < 1 || original.size > MAX_HTML_BYTES) {
    throw new Error('HTML_INPUT_TOO_LARGE_OR_INVALID');
  }
  const handle = await open(path, constants.O_RDONLY | (constants.O_NOFOLLOW || 0));
  try {
    const metadata = await handle.stat();
    if (!metadata.isFile() || metadata.dev !== original.dev || metadata.ino !== original.ino
      || metadata.size < 1 || metadata.size > MAX_HTML_BYTES) throw new Error('HTML_INPUT_TOO_LARGE_OR_INVALID');
    const bytes = await handle.readFile({ signal });
    if (bytes.length < 1 || bytes.length > MAX_HTML_BYTES) throw new Error('HTML_INPUT_TOO_LARGE_OR_INVALID');
    const html = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    if (html.includes('\0')) throw new Error('HTML_INVALID_ENCODING');
    return html;
  } finally {
    await handle.close();
  }
}

async function installPdf(outputPath, bytes, signal) {
  const staging = await mkdtemp(join(dirname(outputPath), '.minimalpdf-html-'));
  try {
    const temporary = join(staging, 'document.pdf');
    const handle = await open(temporary, 'wx', 0o600);
    try {
      await handle.writeFile(bytes);
    } finally {
      await handle.close();
    }
    assertActive(signal);
    // Linking a file in the same directory is atomic and fails if the destination exists.
    await link(temporary, outputPath);
    if (signal.aborted) {
      const source = await stat(temporary);
      const destination = await lstat(outputPath);
      if (source.dev === destination.dev && source.ino === destination.ino) await rm(outputPath);
      assertActive(signal);
    }
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}

async function renderLocalHtmlToPdf({ inputPath, outputPath, signal, timeoutMs = DEFAULT_TIMEOUT_MS }) {
  if (!app.isReady()) throw new Error('HTML_PRINT_APP_NOT_READY');
  if (active) throw new Error('HTML_PRINT_BUSY');
  if (typeof inputPath !== 'string' || !isAbsolute(inputPath) || !['.html', '.htm'].includes(extname(inputPath).toLowerCase())
    || typeof outputPath !== 'string' || !isAbsolute(outputPath) || extname(outputPath).toLowerCase() !== '.pdf'
    || !Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 90_000) {
    throw new Error('HTML_PRINT_INVALID_REQUEST');
  }
  active = true;
  const controller = new AbortController();
  let window;
  const cancel = () => controller.abort('HTML_PRINT_CANCELLED');
  signal?.addEventListener('abort', cancel, { once: true });
  if (signal?.aborted) cancel();
  const timeout = setTimeout(() => controller.abort('HTML_PRINT_TIMEOUT'), timeoutMs);
  try {
    assertActive(controller.signal);
    const html = secureHtml(await readInput(inputPath, controller.signal));
    assertActive(controller.signal);
    const sourceUrl = `data:text/html;charset=utf-8,${encodeURIComponent(html)}`;
    const isolated = session.fromPartition('minimalpdf-offline-print', { cache: false });
    isolated.setPermissionRequestHandler((_contents, _permission, respond) => respond(false));
    isolated.webRequest.onBeforeRequest({ urls: ['<all_urls>'] }, (details, respond) => {
      const topLevel = details.resourceType === 'mainFrame' && details.url === sourceUrl;
      respond({ cancel: !topLevel && !allowedInlineAsset(details) });
    });
    window = new BrowserWindow({
      show: false,
      webPreferences: {
        session: isolated,
        javascript: false,
        sandbox: true,
        contextIsolation: true,
        nodeIntegration: false,
        webSecurity: true,
        webviewTag: false,
        plugins: false,
      },
    });
    const contents = window.webContents;
    contents.setWindowOpenHandler(() => ({ action: 'deny' }));
    contents.on('will-navigate', (event) => event.preventDefault());
    contents.on('will-frame-navigate', (event) => event.preventDefault());
    contents.on('will-redirect', (event) => event.preventDefault());
    contents.on('will-attach-webview', (event) => event.preventDefault());
    const stopped = new Promise((_, reject) => {
      controller.signal.addEventListener('abort', () => {
        if (window && !window.isDestroyed()) window.destroy();
        reject(new Error(controller.signal.reason));
      }, { once: true });
      contents.on('render-process-gone', () => reject(new Error('HTML_PRINT_RENDERER_FAILED')));
    });
    void stopped.catch(() => {});
    await Promise.race([contents.loadURL(sourceUrl), stopped]);
    assertActive(controller.signal);
    const pdf = await Promise.race([
      contents.printToPDF({ printBackground: true, preferCSSPageSize: false, pageSize: 'A4' }),
      stopped,
    ]);
    assertActive(controller.signal);
    if (pdf.length < 8 || pdf.length > MAX_PDF_BYTES || !pdf.subarray(0, 5).equals(Buffer.from('%PDF-'))
      || !pdf.subarray(-1024).includes(Buffer.from('%%EOF'))) {
      throw new Error('HTML_PRINT_INVALID_PDF');
    }
    await installPdf(outputPath, pdf, controller.signal);
    return outputPath;
  } catch (error) {
    if (controller.signal.aborted && error?.name === 'AbortError') throw new Error(controller.signal.reason);
    throw error;
  } finally {
    clearTimeout(timeout);
    signal?.removeEventListener('abort', cancel);
    if (window && !window.isDestroyed()) window.destroy();
    active = false;
  }
}

module.exports = { renderLocalHtmlToPdf };
