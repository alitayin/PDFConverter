import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { inspectPdfiumRuntime } from './check-windows-pdfium.mjs';

function stream(dictionary, body) {
  return Buffer.concat([
    Buffer.from(`<< ${dictionary} /Length ${body.length} >>\nstream\n`),
    body,
    Buffer.from('\nendstream'),
  ]);
}

export function buildWindowsRenderFixture() {
  const pixels = Buffer.from([0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0]);
  const firstPage = Buffer.from([
    'q 1 0 0 rg 0 40 120 40 re f 0 0 1 rg 0 0 120 40 re f Q',
    'q 18 0 0 18 2 2 cm /Im1 Do Q',
    'q /G1 gs 1 1 0 rg 10 55 20 20 re f Q',
  ].join('\n') + '\n');
  const objects = [
    Buffer.from('<< /Type /Catalog /Pages 2 0 R >>'),
    Buffer.from('<< /Type /Pages /Kids [3 0 R 7 0 R 9 0 R] /Count 3 >>'),
    Buffer.from('<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 80] /Resources << /XObject << /Im1 5 0 R >> /ExtGState << /G1 6 0 R >> >> /Contents 4 0 R >>'),
    stream('', firstPage),
    stream('/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8', pixels),
    Buffer.from('<< /Type /ExtGState /ca 0.5 /CA 0.5 >>'),
    Buffer.from('<< /Type /Page /Parent 2 0 R /MediaBox [0 0 80 120] /Rotate 90 /Contents 8 0 R >>'),
    stream('', Buffer.from('q 1 0 0 rg 0 0 80 120 re f Q\n')),
    Buffer.from('<< /Type /Page /Parent 2 0 R /MediaBox [0 0 160 100] /CropBox [20 10 120 90] /Contents 10 0 R >>'),
    stream('', Buffer.from('q 0 1 0 rg 0 0 160 100 re f Q\nq 1 0 0 rg 20 10 m 120 90 l 120 10 l h f Q\n')),
  ];
  const pieces = [Buffer.from('%PDF-1.4\n')];
  const offsets = [0];
  let length = pieces[0].length;
  for (const [index, body] of objects.entries()) {
    offsets.push(length);
    const value = Buffer.concat([Buffer.from(`${index + 1} 0 obj\n`), body, Buffer.from('\nendobj\n')]);
    pieces.push(value);
    length += value.length;
  }
  const xref = Buffer.from([
    `xref\n0 ${offsets.length}\n0000000000 65535 f \n`,
    ...offsets.slice(1).map((offset) => `${String(offset).padStart(10, '0')} 00000 n \n`),
    `trailer\n<< /Size ${offsets.length} /Root 1 0 R >>\nstartxref\n${length}\n%%EOF\n`,
  ].join(''));
  pieces.push(xref);
  return Buffer.concat(pieces);
}

function pixelReport(image) {
  const script = [
    // Windows PowerShell does not reliably auto-load the drawing assembly;
    // explicitly load it before resolving Bitmap (especially on hosted CI).
    "Add-Type -AssemblyName System.Drawing -ErrorAction Stop",
    '$bitmap = [System.Drawing.Bitmap]::new($env:MPC_PDFIUM_IMAGE)',
    'try {',
    '  $w = $bitmap.Width; $h = $bitmap.Height',
    '  $top = $bitmap.GetPixel([int]($w / 2), [int]($h / 8))',
    '  $bottom = $bitmap.GetPixel([int]($w / 2), [int]($h * 7 / 8))',
    '  $scan = $bitmap.GetPixel([int]($w / 12), [int]($h * 7 / 8))',
    '  $overlay = $bitmap.GetPixel([int]($w / 6), [int]($h / 5))',
    '  $softEdges = 0',
    '  for ($y = [int]($h / 2) - 3; $y -le [int]($h / 2) + 3; $y++) {',
    '    for ($x = 0; $x -lt $w; $x++) {',
    '      $pixel = $bitmap.GetPixel($x, $y)',
    '      if ($pixel.R -gt 12 -and $pixel.R -lt 244 -and $pixel.G -gt 12 -and $pixel.G -lt 244) { $softEdges++ }',
    '    }',
    '  }',
    '  [pscustomobject]@{width=$w;height=$h;top=@($top.R,$top.G,$top.B);bottom=@($bottom.R,$bottom.G,$bottom.B);scan=@($scan.R,$scan.G,$scan.B);overlay=@($overlay.R,$overlay.G,$overlay.B);softEdges=$softEdges} | ConvertTo-Json -Compress',
    '} finally { $bitmap.Dispose() }',
  ].join('\n');
  return JSON.parse(execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
    encoding: 'utf8', env: { ...process.env, MPC_PDFIUM_IMAGE: image },
  }).trim());
}

export function buildWorkerRequest(input, outputDir, { pages, dpi, format }) {
  return {
    input, output_dir: outputDir, stem: `fixture-${dpi}-${format}`,
    pages, dpi, format, jpg_quality: 85,
    max_selected_pages: 10_000,
    max_output_bytes: Number.MAX_SAFE_INTEGER,
  };
}

function runWorker(app, input, outputDir, options) {
  mkdirSync(outputDir);
  const request = buildWorkerRequest(input, outputDir, options);
  execFileSync(app, ['--pdfium-render-worker'], {
    input: JSON.stringify(request), windowsHide: true, timeout: 120_000,
    stdio: ['pipe', 'ignore', 'pipe'],
  });
  return JSON.parse(readFileSync(join(outputDir, 'result.json'), 'utf8'));
}

export function smoke(appPath, runtimePath) {
  if (process.platform !== 'win32') throw new Error('full-page PDFium smoke needs a Windows host');
  const app = resolve(appPath);
  const runtime = resolve(runtimePath);
  if (!existsSync(app) || !isAbsolute(appPath)) throw new Error('pass an absolute Windows app .exe path');
  verifyInstalledPdfium(app, runtime);

  const root = mkdtempSync(join(tmpdir(), 'minimal-pdf-windows-render-smoke-'));
  try {
    const input = join(root, '样本 vector and scan.pdf');
    writeFileSync(input, buildWindowsRenderFixture());
    const pngDir = join(root, 'png');
    assert.deepEqual(runWorker(app, input, pngDir, { pages: [1], dpi: 150, format: 'png' }), {
      state: 'succeeded', value: 1,
    });
    const png = join(pngDir, 'fixture-150-png-001.png');
    assert.ok(readFileSync(png).subarray(0, 8).equals(Buffer.from('89504e470d0a1a0a', 'hex')));
    const page = pixelReport(png);
    assert.deepEqual([page.width, page.height], [250, 167]);
    assert.ok(page.top[0] > 220 && page.top[1] < 45 && page.top[2] < 45, `vector top: ${page.top}`);
    assert.ok(page.bottom[0] < 45 && page.bottom[1] < 45 && page.bottom[2] > 220, `vector bottom: ${page.bottom}`);
    assert.ok(page.scan[0] < 45 && page.scan[1] > 210 && page.scan[2] < 45, `embedded image: ${page.scan}`);
    assert.ok(page.overlay[0] > 210 && page.overlay[1] > 70 && page.overlay[1] < 190, `transparency: ${page.overlay}`);

    const jpgDir = join(root, 'jpg');
    assert.deepEqual(runWorker(app, input, jpgDir, { pages: [2], dpi: 300, format: 'jpg' }), {
      state: 'succeeded', value: 1,
    });
    const jpg = join(jpgDir, 'fixture-300-jpg-001.jpg');
    assert.equal(readFileSync(jpg).subarray(0, 2).toString('hex'), 'ffd8');
    const rotated = pixelReport(jpg);
    assert.deepEqual([rotated.width, rotated.height], [500, 334]);
    assert.ok(rotated.top[0] > 195 && rotated.top[1] < 65 && rotated.top[2] < 65, `rotated page: ${rotated.top}`);

    const cropDir = join(root, 'crop');
    assert.deepEqual(runWorker(app, input, cropDir, { pages: [3], dpi: 200, format: 'png' }), {
      state: 'succeeded', value: 1,
    });
    const cropped = pixelReport(join(cropDir, 'fixture-200-png-001.png'));
    assert.deepEqual([cropped.width, cropped.height], [278, 223], 'CropBox must override MediaBox');
    assert.ok(cropped.top[0] < 45 && cropped.top[1] > 210, `cropped upper half: ${cropped.top}`);
    assert.ok(cropped.bottom[0] > 210 && cropped.bottom[1] < 45, `cropped lower half: ${cropped.bottom}`);
    assert.ok(cropped.softEdges > 0, 'diagonal edge must retain anti-aliased color');

    const bad = join(root, 'bad.pdf');
    writeFileSync(bad, '%PDF-1.4\nbad document\n');
    const failed = runWorker(app, bad, join(root, 'failed'), { pages: [1], dpi: 200, format: 'png' });
    assert.deepEqual(failed, { state: 'failed', value: 'corrupted_pdf' });
    console.log('Windows PDFium smoke: vector, embedded scan image, transparency, rotation, CropBox, antialiasing, page selection, PNG/JPG, DPI, Unicode path and damaged PDF passed');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

export function verifyInstalledPdfium(appPath, runtimePath, phase = process.env.RELEASE_STRICT === '1' ? 'signed' : 'source') {
  const app = resolve(appPath);
  const runtime = resolve(runtimePath);
  if (!isAbsolute(appPath) || !existsSync(app)) throw new Error('installed Windows app .exe is missing');
  const sibling = join(dirname(app), 'pdfium-runtime');
  if (!existsSync(sibling)) throw new Error('installed PDFium runtime is missing; smoke must not stage it');
  const approved = inspectPdfiumRuntime(runtime, phase);
  const installed = inspectPdfiumRuntime(sibling, phase);
  if (installed.sha256 !== approved.sha256) throw new Error('installed PDFium DLL differs from the checked source');
  return sibling;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  const [, , app, runtime] = process.argv;
  if (!app || !runtime) throw new Error('usage: node scripts/windows-pdfium-smoke.mjs <absolute-app-exe> <absolute-staged-pdfium-runtime>');
  smoke(app, runtime);
}
