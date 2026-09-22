'use strict';

const { readFile } = require('node:fs/promises');
const { extname, isAbsolute, relative, resolve, sep } = require('node:path');

const MIME = {
  '.css': 'text/css',
  '.html': 'text/html',
  '.ico': 'image/x-icon',
  '.jpg': 'image/jpeg',
  '.js': 'text/javascript',
  '.json': 'application/json',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.woff2': 'font/woff2',
};
const CSP = "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'none'; object-src 'none'; base-uri 'none'; frame-src 'none'";

function assetPath(outDir, rawUrl) {
  const url = new URL(rawUrl);
  if (url.protocol !== 'app:' || url.hostname !== 'local') return null;
  let pathname;
  try {
    pathname = decodeURIComponent(url.pathname);
  } catch {
    return null;
  }
  if (pathname.includes('\\') || pathname.includes('\0')) return null;
  const relativePath = pathname.endsWith('/') ? `${pathname}index.html` : pathname;
  const target = resolve(outDir, `.${relativePath}`);
  const fromRoot = relative(resolve(outDir), target);
  if (fromRoot === '..' || fromRoot.startsWith(`..${sep}`) || isAbsolute(fromRoot)) return null;
  return target;
}

async function serveStatic(outDir, rawUrl) {
  const file = assetPath(outDir, rawUrl);
  if (!file) return new Response('Not found', { status: 404 });
  try {
    const data = await readFile(file);
    return new Response(data, {
      headers: {
        'content-type': MIME[extname(file)] ?? 'application/octet-stream',
        'content-security-policy': CSP,
        'cache-control': 'no-store',
      },
    });
  } catch {
    return new Response('Not found', { status: 404 });
  }
}

module.exports = { assetPath, serveStatic };
