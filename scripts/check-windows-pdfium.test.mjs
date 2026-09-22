import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import {
  inspectPdfiumRuntime, LICENSE_FILES, PINNED_ARCHIVE_SHA256, PINNED_DLL_SHA256, PINNED_VERSION,
} from './check-windows-pdfium.mjs';

async function fixture(t, sourceArchive) {
  const root = await mkdtemp(join(tmpdir(), 'minimal-pdf-windows-runtime-test-'));
  t.after(async () => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, 'licenses'));
  const load = (name) => sourceArchive
    ? execFileSync('tar', ['-xOf', sourceArchive, name], { maxBuffer: 16 * 1024 * 1024 })
    : Buffer.from(`unit fixture for ${name}`);
  await writeFile(join(root, 'LICENSE'), load('LICENSE'));
  for (const name of LICENSE_FILES) await writeFile(join(root, 'licenses', name), load(`licenses/${name}`));
  await writeFile(join(root, 'pdfium.dll'), load('bin/pdfium.dll'));
  await writeFile(join(root, 'RUNTIME_VERSION'), PINNED_VERSION);
  await writeFile(join(root, 'RUNTIME_ARCHIVE_SHA256'), PINNED_ARCHIVE_SHA256);
  await writeFile(join(root, 'RUNTIME_DLL_SHA256_SOURCE'), PINNED_DLL_SHA256);
  return root;
}

test('missing and tampered PDFium binaries are rejected', async (t) => {
  assert.throws(() => inspectPdfiumRuntime(join(tmpdir(), 'nonexistent-pdfium-runtime')), /missing/);
  const root = await fixture(t);
  assert.throws(() => inspectPdfiumRuntime(root), /unsigned PDFium DLL hash/);
});

test('unexpected files or license gaps block staging', async (t) => {
  const root = await fixture(t);
  await writeFile(join(root, 'unapproved.dll'), 'foreign binary');
  assert.throws(() => inspectPdfiumRuntime(root), /unexpected entries/);
  await rm(join(root, 'unapproved.dll'));
  await rm(join(root, 'licenses', LICENSE_FILES[0]));
  assert.throws(() => inspectPdfiumRuntime(root), /license set/);
});

test('metadata cannot substitute a different runtime release', async (t) => {
  const root = await fixture(t);
  await writeFile(join(root, 'RUNTIME_ARCHIVE_SHA256'), '0'.repeat(64));
  assert.throws(() => inspectPdfiumRuntime(root), /provenance/);
  assert.throws(() => inspectPdfiumRuntime(root, 'other'), /verification phase/);
});

test('the pinned community archive passes the source hash and all 15 license checks', {
  skip: !process.env.MPC_PDFIUM_ARCHIVE_FIXTURE,
}, async (t) => {
  const root = await fixture(t, process.env.MPC_PDFIUM_ARCHIVE_FIXTURE);
  const info = inspectPdfiumRuntime(root);
  assert.equal(info.sha256, PINNED_DLL_SHA256);
  assert.equal(info.licenseCount, 15);
  await writeFile(join(root, 'RUNTIME_DLL_SHA256_SIGNED'), PINNED_DLL_SHA256);
  assert.throws(() => inspectPdfiumRuntime(root, 'signed'), /signed PDFium DLL hash/);
});
