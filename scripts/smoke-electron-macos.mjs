import { execFileSync, spawn } from 'node:child_process';
import {
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const version = readFileSync(join(root, 'VERSION'), 'utf8').trim();
const paths = process.argv.slice(2);
if (process.platform !== 'darwin' || process.arch !== 'arm64' || paths.length > 1) {
  throw new Error('Usage (Apple Silicon macOS): node scripts/smoke-electron-macos.mjs [path/to/arm64.dmg]');
}
const dmg = resolve(paths[0] ?? join(root, 'dist-electron', `MinimalPdfConverter-${version}-arm64.dmg`));

function run(command, args, options = {}) {
  return execFileSync(command, args, {
    cwd: root,
    stdio: 'inherit',
    timeout: 600_000,
    ...options,
  });
}

function requireRealFile(path, label) {
  if (!existsSync(path)) throw new Error(`${label} is missing: ${path}`);
  const metadata = lstatSync(path);
  if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size === 0) {
    throw new Error(`${label} must be a real nonempty file: ${path}`);
  }
}

function plistValue(plist, key) {
  return execFileSync('/usr/libexec/PlistBuddy', ['-c', `Print :${key}`, plist], {
    encoding: 'utf8',
    timeout: 30_000,
  }).trim();
}

function verifyOfficeSignature(officeApp) {
  run('codesign', ['--verify', '--deep', '--strict', officeApp]);
  if (plistValue(join(officeApp, 'Contents', 'Info.plist'), 'LSUIElement') !== 'true') {
    throw new Error('packaged LibreOffice can appear in the macOS Dock');
  }
}

async function launchCopiedApp(app, scratch) {
  const plist = join(app, 'Contents', 'Info.plist');
  const executable = join(app, 'Contents', 'MacOS', plistValue(plist, 'CFBundleExecutable'));
  requireRealFile(executable, 'Electron executable');
  const userData = join(scratch, 'electron-user-data');
  mkdirSync(userData);
  const child = spawn(executable, [`--user-data-dir=${userData}`], {
    cwd: dirname(executable),
    env: { ...process.env, ELECTRON_ENABLE_LOGGING: '1' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const closed = new Promise((done) => child.once('close', done));
  let output = '';
  try {
    await new Promise((resolveReady, rejectReady) => {
      let settled = false;
      const timer = setTimeout(() => finish(new Error(`copied Electron app did not become ready\n${output}`)), 20_000);
      const finish = (error) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        if (error) rejectReady(error);
        else resolveReady();
      };
      const capture = (chunk) => {
        if (output.length < 16_384) output += chunk.toString('utf8');
        if (output.includes('[desktop] ready')) finish();
      };
      child.stdout.on('data', capture);
      child.stderr.on('data', capture);
      child.once('error', (error) => finish(new Error(`copied Electron app failed to launch: ${error.message}\n${output}`)));
      child.once('exit', (code, signal) => finish(new Error(`copied Electron app exited before ready: ${code}/${signal}\n${output}`)));
    });
  } finally {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    let didClose = await Promise.race([
      closed.then(() => true),
      new Promise((done) => setTimeout(() => done(false), 10_000)),
    ]);
    if (!didClose && child.exitCode === null && child.signalCode === null) {
      child.kill('SIGKILL');
      didClose = await Promise.race([
        closed.then(() => true),
        new Promise((done) => setTimeout(() => done(false), 5_000)),
      ]);
    }
    if (!didClose) throw new Error(`copied Electron app could not be stopped\n${output}`);
  }
}

requireRealFile(dmg, 'Electron DMG');
const scratch = realpathSync(mkdtempSync(join(tmpdir(), 'minimalpdf-electron-dmg-')));
const volume = join(scratch, 'volume');
const installedRoot = join(scratch, 'installed');
mkdirSync(volume);
mkdirSync(installedRoot);
let mounted = false;
try {
  const attach = execFileSync(
    'hdiutil',
    ['attach', '-readonly', '-nobrowse', '-noautoopen', '-mountpoint', volume, '-plist', dmg],
    { cwd: root, stdio: ['ignore', 'pipe', 'inherit'], timeout: 120_000 },
  );
  mounted = true;
  const mountedJson = execFileSync('plutil', ['-convert', 'json', '-o', '-', '-'], {
    input: attach,
    timeout: 30_000,
  });
  const entities = JSON.parse(mountedJson.toString('utf8'))['system-entities'];
  if (!entities?.some((entry) => entry['mount-point'] && realpathSync(entry['mount-point']) === realpathSync(volume))) {
    throw new Error('DMG did not mount at the isolated mount point');
  }

  const apps = readdirSync(volume).filter((name) => {
    const path = join(volume, name);
    return name.endsWith('.app') && lstatSync(path).isDirectory() && !lstatSync(path).isSymbolicLink();
  });
  if (apps.length !== 1) throw new Error(`DMG must contain exactly one real app, found ${apps.length}`);
  const source = join(volume, apps[0]);
  if (!realpathSync(source).startsWith(`${realpathSync(volume)}${sep}`)) {
    throw new Error('DMG app resolves outside the mounted image');
  }
  const installedTarget = join(installedRoot, basename(source));
  run('ditto', ['--rsrc', '--extattr', source, installedTarget]);
  const installed = realpathSync(installedTarget);

  const resources = join(installed, 'Contents', 'Resources');
  const engine = join(resources, 'bin', 'minimal-pdf-converter');
  const officeApp = join(resources, 'office', 'LibreOffice.app');
  const office = join(officeApp, 'Contents', 'MacOS', 'soffice');
  const asar = join(resources, 'app.asar');
  const plist = join(installed, 'Contents', 'Info.plist');
  requireRealFile(engine, 'packaged Rust engine');
  requireRealFile(office, 'packaged LibreOffice executable');
  requireRealFile(asar, 'packaged Electron asar');
  requireRealFile(plist, 'packaged Info.plist');
  if (plistValue(plist, 'CFBundleShortVersionString') !== version) {
    throw new Error('packaged app version does not match VERSION');
  }
  if (execFileSync('lipo', ['-archs', engine], { encoding: 'utf8', timeout: 30_000 }).trim() !== 'arm64') {
    throw new Error('packaged Rust engine is not arm64-only');
  }
  if (plistValue(join(officeApp, 'Contents', 'Info.plist'), 'CFBundleShortVersionString') !== '26.2.6.3') {
    throw new Error('packaged LibreOffice version is not 26.2.6.3');
  }
  verifyOfficeSignature(officeApp);

  run('node', ['--test', 'electron/format-conversions.integration.test.mjs'], {
    timeout: 300_000,
    env: {
      ...process.env,
      MINIMALPDF_RUST_ENGINE: engine,
      MINIMALPDF_OFFICE_EXECUTABLE: office,
      MINIMALPDF_OFFICE_MODE: 'bundled',
      MINIMALPDF_REQUIRE_OFFICE: '1',
      PYTHONDONTWRITEBYTECODE: '1',
    },
  });
  verifyOfficeSignature(officeApp);
  await launchCopiedApp(installed, scratch);
  console.log(`Electron arm64 DMG copy, bundled-engine integration and launch smoke passed: ${dmg}`);
} finally {
  if (mounted) {
    try {
      run('hdiutil', ['detach', volume], { timeout: 120_000 });
      mounted = false;
    } catch {
      console.error(`could not detach smoke-test volume: ${volume}`);
      process.exitCode = 2;
    }
  }
  if (!mounted) rmSync(scratch, { recursive: true, force: true });
}
