import { spawn, execFileSync } from 'node:child_process';
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const version = readFileSync(join(repoRoot, 'VERSION'), 'utf8').trim();
const defaultDmg = join(repoRoot, 'src-tauri', 'target', 'universal-apple-darwin', 'release',
  'bundle', 'dmg', `Ayst Arc PDF_${version}_universal.dmg`);
const args = process.argv.slice(2);
const noLaunch = args.includes('--no-launch');
const paths = args.filter((arg) => arg !== '--no-launch');
if (process.platform !== 'darwin' || paths.length > 1 || paths.some((arg) => arg.startsWith('--'))) {
  throw new Error('Usage (macOS): node scripts/macos-dmg-smoke.mjs [absolute/path/to/app.dmg] [--no-launch]');
}
const dmg = resolve(paths[0] ?? defaultDmg);
if (!existsSync(dmg) || !lstatSync(dmg).isFile() || !dmg.endsWith('.dmg')) {
  throw new Error(`DMG not found: ${dmg}`);
}
const strict = process.env.RELEASE_STRICT === '1';

function check(command, commandArgs, options = {}) {
  return execFileSync(command, commandArgs, {
    cwd: repoRoot, stdio: 'inherit', timeout: 120_000, ...options,
  });
}

function verifyApp(app) {
  check('node', [join(repoRoot, 'scripts', 'verify-release-artifact.mjs'), app]);
  check('codesign', ['--verify', '--deep', '--strict', '--verbose=2', app]);
  for (const name of ['minimal-pdf-converter', 'pdf_to_txt_worker']) {
    const binary = join(app, 'Contents', 'MacOS', name);
    const architectures = execFileSync('lipo', ['-archs', binary], { encoding: 'utf8', timeout: 30_000 })
      .trim().split(/\s+/);
    if (!['arm64', 'x86_64'].every((architecture) => architectures.includes(architecture))) {
      throw new Error(`copied ${name} is not a macOS Universal binary: ${architectures.join(', ')}`);
    }
  }
  if (strict) {
    check('spctl', ['--assess', '--type', 'execute', '--verbose=4', app]);
    check('xcrun', ['stapler', 'validate', '-v', app]);
  }
}

async function launchCopy(app) {
  const executable = join(app, 'Contents', 'MacOS', 'minimal-pdf-converter');
  const child = spawn(executable, [], { cwd: dirname(executable), stdio: 'ignore' });
  let launchError;
  child.once('error', (error) => { launchError = error; });
  try {
    await new Promise((done) => setTimeout(done, 3000));
    if (launchError || child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`copied app failed to stay running: ${launchError?.message ?? `${child.exitCode}/${child.signalCode}`}`);
    }
    console.log(`copied app launched: pid ${child.pid}`);
  } finally {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    await Promise.race([
      new Promise((done) => child.once('close', done)),
      new Promise((done) => setTimeout(done, 5000)),
    ]);
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
  }
}

const root = mkdtempSync(join(tmpdir(), 'minimal-pdf-dmg-smoke-'));
const volume = join(root, 'volume');
const installDir = join(root, 'installed');
mkdirSync(volume);
mkdirSync(installDir);
let mounted = false;
try {
  const attach = execFileSync('hdiutil', ['attach', '-readonly', '-nobrowse', '-mountpoint', volume, '-plist', dmg], {
    cwd: repoRoot, stdio: ['ignore', 'pipe', 'inherit'], timeout: 120_000,
  });
  mounted = true;
  const mountedPlist = execFileSync('plutil', ['-convert', 'json', '-o', '-', '-'], {
    input: attach, timeout: 30_000,
  });
  const entities = JSON.parse(mountedPlist.toString('utf8'))['system-entities'];
  if (!entities?.some((entry) => entry['mount-point'] && realpathSync(entry['mount-point']) === realpathSync(volume))) {
    throw new Error('DMG did not mount at the isolated mount point');
  }
  const apps = readdirSync(volume).filter((name) => name.endsWith('.app') && lstatSync(join(volume, name)).isDirectory());
  if (apps.length !== 1) throw new Error(`DMG must contain exactly one app, found ${apps.length}`);
  const source = join(volume, apps[0]);
  if (!realpathSync(source).startsWith(`${realpathSync(volume)}${sep}`)) {
    throw new Error('DMG app resolves outside its mounted volume');
  }
  const installed = join(installDir, basename(source));
  check('ditto', ['--rsrc', '--extattr', source, installed]);
  verifyApp(installed);
  if (strict) check('xcrun', ['stapler', 'validate', '-v', dmg]);
  check('node', [join(repoRoot, 'scripts', 'quality-smoke.mjs'), '--worker',
    join(installed, 'Contents', 'MacOS', 'pdf_to_txt_worker')]);
  if (!noLaunch) await launchCopy(installed);
  console.log(strict
    ? 'signed DMG copy smoke passed; clean-machine and real-document checks remain separate'
    : 'local DMG copy smoke passed (development signature; not a notarized/clean-machine release)');
} finally {
  if (mounted) check('hdiutil', ['detach', volume]);
  rmSync(root, { recursive: true, force: true });
}
