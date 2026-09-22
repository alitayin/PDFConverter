import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, lstatSync, readFileSync, readdirSync } from 'node:fs';
import { platform } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const PINNED_VERSION = '155.0.8057.0';
export const PINNED_ARCHIVE_SHA256 = 'e307d519e42f2e69b1b531f0c2a32dffcdf3891ec0eba60328ba51a57cec01ed';
export const PINNED_DLL_SHA256 = '55e7ebef29a1ec9523d1adb8b260a73e7dfb0f64d3f0285121d20ecd6148ef18';
export const LICENSE_FILES = Object.freeze([
  'pdfium.txt', 'libopenjpeg.txt', 'abseil.txt', 'lcms.txt', 'agg23.txt',
  'libjpeg_turbo.md', 'llvm-libc.txt', 'zlib.txt', 'freetype.txt',
  'libjpeg_turbo.ijg', 'icu.txt', 'simdutf.txt', 'fast_float.txt', 'libpng.txt',
]);

export function sha256(file) {
  return createHash('sha256').update(readFileSync(file)).digest('hex');
}

function verifiedFile(path) {
  if (!existsSync(path) || !lstatSync(path).isFile() || lstatSync(path).isSymbolicLink() || lstatSync(path).size === 0) {
    throw new Error(`PDFium file missing, empty or symlinked: ${path}`);
  }
  return path;
}

export function inspectPdfiumRuntime(root, phase = 'source') {
  if (phase !== 'source' && phase !== 'signed') throw new Error('PDFium verification phase must be source or signed');
  if (!existsSync(root) || !lstatSync(root).isDirectory() || lstatSync(root).isSymbolicLink()) {
    throw new Error('PDFium staged directory is missing or symlinked');
  }
  const files = ['LICENSE', 'RUNTIME_VERSION', 'RUNTIME_ARCHIVE_SHA256', 'RUNTIME_DLL_SHA256_SOURCE', 'pdfium.dll'];
  if (phase === 'signed') files.push('RUNTIME_DLL_SHA256_SIGNED');
  const allowedFiles = new Set(files);
  const rootNames = readdirSync(root);
  if (rootNames.length !== allowedFiles.size + 1 || !rootNames.includes('licenses') || rootNames.some((name) => name !== 'licenses' && !allowedFiles.has(name))) {
    throw new Error('PDFium staged directory has missing or unexpected entries');
  }
  for (const file of files) verifiedFile(join(root, file));
  const licenseRoot = join(root, 'licenses');
  if (!lstatSync(licenseRoot).isDirectory() || lstatSync(licenseRoot).isSymbolicLink()) {
    throw new Error('PDFium licenses directory is not a real directory');
  }
  const licenseNames = readdirSync(licenseRoot);
  if (licenseNames.length !== LICENSE_FILES.length || licenseNames.some((name) => !LICENSE_FILES.includes(name))) {
    throw new Error('PDFium third-party license set differs from approved archive');
  }
  for (const name of LICENSE_FILES) verifiedFile(join(licenseRoot, name));
  const marker = (name) => readFileSync(join(root, name), 'utf8').trim().toLowerCase();
  if (marker('RUNTIME_VERSION') !== PINNED_VERSION ||
      marker('RUNTIME_ARCHIVE_SHA256') !== PINNED_ARCHIVE_SHA256 ||
      marker('RUNTIME_DLL_SHA256_SOURCE') !== PINNED_DLL_SHA256) {
    throw new Error('PDFium staged provenance does not match approved release');
  }
  const actual = sha256(join(root, 'pdfium.dll'));
  if (phase === 'source' && actual !== PINNED_DLL_SHA256) throw new Error('unsigned PDFium DLL hash differs from approved release');
  if (phase === 'signed' && (actual === PINNED_DLL_SHA256 || actual !== marker('RUNTIME_DLL_SHA256_SIGNED'))) {
    throw new Error('signed PDFium DLL hash differs from the staged signed manifest');
  }
  return { dll: join(root, 'pdfium.dll'), sha256: actual, version: PINNED_VERSION, licenseCount: LICENSE_FILES.length + 1 };
}

function verifyAuthenticode(dll) {
  if (platform() !== 'win32') throw new Error('Authenticode verification requires a Windows host');
  const expected = (process.env.WINDOWS_CERTIFICATE_THUMBPRINT || '').replace(/\s/g, '').toUpperCase();
  if (!/^[0-9A-F]{40}$/.test(expected)) throw new Error('strict Windows release requires WINDOWS_CERTIFICATE_THUMBPRINT');
  const script = [
    '$s = Get-AuthenticodeSignature -LiteralPath $env:MPC_PDFIUM_DLL_PATH -ErrorAction Stop',
    '[pscustomobject]@{Status=[string]$s.Status;Thumbprint=[string]$s.SignerCertificate.Thumbprint;Timestamped=[bool]$s.TimeStamperCertificate} | ConvertTo-Json -Compress',
  ].join('; ');
  const signature = JSON.parse(execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
    encoding: 'utf8', env: { ...process.env, MPC_PDFIUM_DLL_PATH: dll },
  }).trim());
  if (signature.Status !== 'Valid' || signature.Thumbprint?.toUpperCase() !== expected || !signature.Timestamped) {
    throw new Error('PDFium DLL lacks the expected valid, timestamped Authenticode signature');
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  try {
    const root = process.env.PDFIUM_RUNTIME_DIR;
    if (!root) throw new Error('PDFIUM_RUNTIME_DIR must point to the staged app-local PDFium runtime');
    const phase = process.argv[2] || 'source';
    const info = inspectPdfiumRuntime(resolve(root), phase);
    if (process.env.RELEASE_STRICT === '1' && phase !== 'signed' && process.argv[3] !== '--before-sign') {
      throw new Error('strict Windows release requires signed PDFium verification');
    }
    if (phase === 'signed') verifyAuthenticode(info.dll);
    console.log(`PDFium ${info.version}: ${info.sha256}; license files: ${info.licenseCount}; phase: ${phase}`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 2;
  }
}
