import { existsSync, readFileSync, statSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join, resolve } from 'node:path';

const PINNED_VERSION = '153.0.4234.48';
const PINNED_PACKAGE_SHA256 = '11e8240cb0bc56dcd3e4498907203c251346f65107fe35a3a13e152c7d51c79e';
const PINNED_EXECUTABLE_SHA256 = '65afdc3965a6d1c4ccd5b47801fec8a16db15613d35c8e3a6d1fb6c0da970eea';

const runtime = process.env.WEBVIEW2_FIXED_RUNTIME_DIR;
const strict = process.env.RELEASE_STRICT === '1';

if (!runtime) {
  const message = 'WEBVIEW2_FIXED_RUNTIME_DIR is required for the self-contained Windows installer';
  if (strict) {
    console.error(message);
    process.exit(2);
  }
  console.warn(message);
  process.exit(0);
}

const root = resolve(runtime);
const executable = join(root, 'msedgewebview2.exe');
if (!existsSync(executable) || !statSync(executable).isFile() || statSync(executable).size === 0) {
  console.error(`WebView2 fixed runtime is incomplete: ${executable}`);
  process.exit(2);
}
const versionFile = join(root, 'RUNTIME_VERSION');
const digestFile = join(root, 'RUNTIME_PACKAGE_SHA256');
const executableDigestFile = join(root, 'RUNTIME_EXECUTABLE_SHA256');
if (strict && (!existsSync(versionFile) || !existsSync(digestFile) || !existsSync(executableDigestFile))) {
  console.error('strict Windows release requires pinned WebView2 runtime metadata');
  process.exit(2);
}
if (strict) {
  const version = readFileSync(versionFile, 'utf8').trim();
  const digest = readFileSync(digestFile, 'utf8').trim().toLowerCase();
  const executableDigest = readFileSync(executableDigestFile, 'utf8').trim().toLowerCase();
  if (version !== PINNED_VERSION || digest !== PINNED_PACKAGE_SHA256 || executableDigest !== PINNED_EXECUTABLE_SHA256) {
    console.error('WebView2 fixed runtime metadata does not match the pinned official Microsoft download');
    process.exit(2);
  }
  const installedDigest = createHash('sha256').update(readFileSync(executable)).digest('hex');
  if (installedDigest !== PINNED_EXECUTABLE_SHA256) {
    console.error('WebView2 fixed runtime executable SHA-256 does not match the pinned download');
    process.exit(2);
  }
}
console.log(`WebView2 fixed runtime: ${root}`);
