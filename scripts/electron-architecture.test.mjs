import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { test } from 'node:test';

test('the desktop release is Electron plus a standalone Rust engine', () => {
  const root = resolve(import.meta.dirname, '..');
  assert.equal(existsSync(resolve(root, 'src-tauri')), false);
  assert.equal(existsSync(resolve(root, 'rust-engine/Cargo.toml')), true);
  const cargo = readFileSync(resolve(root, 'rust-engine/Cargo.toml'), 'utf8');
  const workflow = readFileSync(resolve(root, '.github/workflows/release.yml'), 'utf8');
  assert.doesNotMatch(cargo, /tauri/i);
  assert.doesNotMatch(workflow, /cargo\s+tauri|tauri-cli|tauri\.conf/i);
  assert.match(workflow, /softprops\/action-gh-release/);
});
