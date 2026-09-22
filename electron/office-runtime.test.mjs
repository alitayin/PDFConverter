import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, renameSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { test } from 'node:test';

const require = createRequire(import.meta.url);
const { bundledOfficeExecutable, officeBridgeEnv } = require('./office-runtime.cjs');
const officeVar = 'MINIMALPDF_OFFICE_EXECUTABLE';
const officeMode = 'MINIMALPDF_OFFICE_MODE';

function fixture(t) {
  const resourcesPath = mkdtempSync(join(tmpdir(), 'minimal-pdf-office-resources-'));
  t.after(() => rmSync(resourcesPath, { recursive: true, force: true }));
  return resourcesPath;
}

function stageExecutable(resourcesPath, platform) {
  const relative = platform === 'darwin'
    ? ['office', 'LibreOffice.app', 'Contents', 'MacOS', 'soffice']
    : ['office', 'LibreOffice', 'program', 'soffice.exe'];
  const executable = join(resourcesPath, ...relative);
  mkdirSync(dirname(executable), { recursive: true });
  writeFileSync(executable, 'test executable');
  return executable;
}

for (const platform of ['darwin', 'win32']) {
  test(`packaged ${platform} bridge passes only the fixed nonempty office runtime`, (t) => {
    const resourcesPath = fixture(t);
    const baseEnv = { ...process.env, [officeVar]: '/tmp/untrusted-soffice', [officeMode]: 'host', PYTHONDONTWRITEBYTECODE: '0', TEST_PRESERVED: 'yes' };
    const options = { isPackaged: true, resourcesPath, platform };
    assert.equal(bundledOfficeExecutable(options), null);
    assert.equal(officeBridgeEnv({ ...options, baseEnv })[officeVar], undefined);
    assert.equal(officeBridgeEnv({ ...options, baseEnv })[officeMode], 'bundled');
    const executable = stageExecutable(resourcesPath, platform);
    assert.equal(bundledOfficeExecutable(options), executable);
    const env = officeBridgeEnv({ ...options, baseEnv });
    assert.equal(env[officeVar], executable);
    assert.equal(env[officeMode], 'bundled');
    assert.equal(env.PYTHONDONTWRITEBYTECODE, '1');
    assert.equal(env.TEST_PRESERVED, 'yes');
    assert.equal(baseEnv[officeVar], '/tmp/untrusted-soffice');
    assert.equal(baseEnv[officeMode], 'host');
    assert.equal(officeBridgeEnv({ ...options, isPackaged: false, baseEnv })[officeVar], undefined);
    assert.equal(officeBridgeEnv({ ...options, isPackaged: false, baseEnv })[officeMode], undefined);
    assert.equal(officeBridgeEnv({ ...options, isPackaged: false, baseEnv }).PYTHONDONTWRITEBYTECODE, '0');
    assert.equal(officeBridgeEnv({ ...options, resourcesPath: 'relative', baseEnv })[officeVar], undefined);
    assert.equal(officeBridgeEnv({ ...options, resourcesPath: 'relative', baseEnv })[officeMode], 'bundled');
  });

  test(`packaged ${platform} bridge ignores incomplete and symlinked office bundles`, (t) => {
    const resourcesPath = fixture(t);
    const executable = stageExecutable(resourcesPath, platform);
    const options = { isPackaged: true, resourcesPath, platform };
    writeFileSync(executable, '');
    assert.equal(bundledOfficeExecutable(options), null);
    writeFileSync(executable, 'test executable');
    const original = join(resourcesPath, 'actual-executable');
    writeFileSync(original, 'test executable');
    rmSync(executable);
    try {
      symlinkSync(original, executable, 'file');
    } catch (error) {
      if (process.platform !== 'win32' || !['EPERM', 'EACCES'].includes(error.code)) throw error;
      t.diagnostic('Windows requires symlink privileges for the symlink rejection assertions');
      return;
    }
    assert.equal(bundledOfficeExecutable(options), null);
    rmSync(executable);
    writeFileSync(executable, 'test executable');
    const officeDir = join(resourcesPath, 'office');
    const movedOffice = join(resourcesPath, 'actual-office');
    renameSync(officeDir, movedOffice);
    symlinkSync(movedOffice, officeDir, process.platform === 'win32' ? 'junction' : 'dir');
    assert.equal(bundledOfficeExecutable(options), null);
    assert.equal(officeBridgeEnv({ ...options, baseEnv: { [officeVar]: original } })[officeVar], undefined);
    assert.equal(officeBridgeEnv({ ...options, baseEnv: { [officeVar]: original } })[officeMode], 'bundled');
  });
}

test('unsupported platforms do not inherit an office executable override', (t) => {
  const resourcesPath = fixture(t);
  for (const platform of ['linux', 'constructor']) {
    assert.deepEqual(officeBridgeEnv({ isPackaged: true, resourcesPath, platform, baseEnv: { [officeVar]: '/tmp/soffice', [officeMode]: 'host' } }), { [officeMode]: 'bundled', PYTHONDONTWRITEBYTECODE: '1' });
  }
});

test('symlinked resources root cannot supply an office runtime', (t) => {
  const resourcesPath = fixture(t);
  const executable = stageExecutable(resourcesPath, 'darwin');
  const linkedRoot = join(resourcesPath, 'linked-resources');
  try {
    symlinkSync(resourcesPath, linkedRoot, process.platform === 'win32' ? 'junction' : 'dir');
  } catch (error) {
    if (process.platform !== 'win32' || !['EPERM', 'EACCES'].includes(error.code)) throw error;
    t.diagnostic('Windows requires symlink privileges for the symlink rejection assertion');
    return;
  }
  assert.equal(bundledOfficeExecutable({ isPackaged: true, resourcesPath: linkedRoot, platform: 'darwin' }), null);
  assert.equal(officeBridgeEnv({ isPackaged: true, resourcesPath: linkedRoot, platform: 'darwin', baseEnv: { [officeVar]: executable } })[officeMode], 'bundled');
});
