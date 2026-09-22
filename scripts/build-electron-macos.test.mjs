import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmdirSync, rmSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const script = join(root, 'scripts', 'build-electron-macos.sh');

test('macOS builder keeps preview output separate from existing app and DMG', () => {
  const source = readFileSync(script, 'utf8');
  assert.match(source, /output_dir="\$preview_root\/\$preview_name"/);
  assert.match(source, /preview_root="\$repo_root\/dist-electron\/previews"/);
  assert.match(source, /"--config\.directories\.output=\$output_dir"/);
  assert.match(source, /mkdir "\$output_dir"/);
  assert.match(source, /output_created=yes/);
  assert.doesNotMatch(source, /app_path="\$repo_root\/dist-electron\/mac-arm64/);
  assert.doesNotMatch(source, /dmg_path="\$repo_root\/dist-electron\/MinimalPdfConverter-/);
});

test('macOS builder verifies the original Office signature before preparing a signed background app', () => {
  const source = readFileSync(script, 'utf8');
  const verified = source.indexOf('verify_official_office "$stage_tmp/office/LibreOffice.app"');
  const background = source.indexOf('plutil -insert LSUIElement -bool YES');
  const resigned = source.indexOf('codesign --force --deep --sign');
  assert.ok(verified >= 0 && verified < background && background < resigned);
  assert.match(source, /verify_background_office "\$office_stage"/);
  assert.match(source, /verify_background_office "\$app_path\/Contents\/Resources\/office\/LibreOffice\.app"/);
});

test('macOS development preview cannot stage an arbitrary engine executable', () => {
  const source = readFileSync(script, 'utf8');
  assert.match(source, /MINIMALPDF_PREVIEW_ENGINE" != "\$engine_binary"/);
  assert.match(source, /-L "\$engine_binary"/);
  assert.match(source, /Packaging the prebuilt debug engine for a development preview/);
  assert.match(source, /MINIMALPDF_PREVIEW_OFFICE_APP" != \/Applications\/LibreOffice\.app/);
  assert.match(source, /verify_official_office "\$source_app"/);
});

test('macOS builder rejects escape and existing preview before any download or build', {
  skip: process.platform !== 'darwin' || process.arch !== 'arm64',
}, (t) => {
  const previewRoot = join(root, 'dist-electron', 'previews');
  const createdParent = !existsSync(previewRoot);
  if (createdParent) mkdirSync(previewRoot);
  const existing = join(previewRoot, `collision-${randomUUID()}`);
  mkdirSync(existing);
  writeFileSync(join(existing, 'sentinel'), 'existing preview');
  t.after(() => {
    rmSync(existing, { recursive: true });
    if (createdParent) rmdirSync(previewRoot);
  });
  const check = (previewName) => spawnSync('/bin/zsh', [script], {
    cwd: root,
    encoding: 'utf8',
    timeout: 15_000,
    env: { ...process.env, RELEASE_STRICT: '0', MINIMALPDF_PREVIEW_NAME: previewName },
  });

  const escaped = check('../mac-arm64');
  assert.equal(escaped.status, 2, escaped.stderr);
  assert.match(escaped.stderr, /Invalid package version or preview directory name/);
  assert.equal(readFileSync(join(existing, 'sentinel'), 'utf8'), 'existing preview');

  const collision = check(basename(existing));
  assert.equal(collision.status, 2, collision.stderr);
  assert.match(collision.stderr, /Refusing to replace existing packaging artifact/);
  assert.equal(readFileSync(join(existing, 'sentinel'), 'utf8'), 'existing preview');
});
