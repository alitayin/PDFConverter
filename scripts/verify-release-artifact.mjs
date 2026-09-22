import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { arch, platform } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const root = resolve(import.meta.dirname, '..');
const version = readFileSync(join(root, 'VERSION'), 'utf8').trim();
const artifact = process.argv[2] ? resolve(process.argv[2]) : undefined;
const strict = process.env.RELEASE_STRICT === '1';

if (!artifact || !existsSync(artifact)) {
  console.error('release artifact missing; pass an app, dmg, exe, or installer path');
  process.exit(2);
}

function command(name, args) {
  const result = spawnSync(name, args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
  return { ok: result.status === 0, output: `${result.stdout || ''}${result.stderr || ''}${result.error || ''}` };
}

function totalSize(path) {
  const info = statSync(path);
  if (info.isFile()) return info.size;
  return readdirSync(path).reduce((total, entry) => total + totalSize(join(path, entry)), 0);
}

const isApp = artifact.endsWith('.app');
const isMac = platform() === 'darwin';
if (isApp) {
  const executable = join(artifact, 'Contents', 'MacOS', 'minimal-pdf-converter');
  const worker = join(artifact, 'Contents', 'MacOS', 'pdf_to_txt_worker');
  const plist = join(artifact, 'Contents', 'Info.plist');
  if (!existsSync(executable) || !existsSync(worker) || !existsSync(plist)) throw new Error('invalid macOS app bundle');
  const plistText = readFileSync(plist, 'utf8');
  if (!plistText.includes(`<string>${version}</string>`)) throw new Error(`app version does not contain ${version}`);
  if (strict && isMac) {
    const bundleVersion = command('plutil', ['-extract', 'CFBundleShortVersionString', 'raw', plist]);
    if (!bundleVersion.ok || bundleVersion.output.trim() !== version) {
      throw new Error(`macOS CFBundleShortVersionString must equal ${version}`);
    }
  }
  const fileInfo = command('file', [executable]);
  const signature = command('codesign', ['-dv', '--verbose=4', artifact]);
  const signatureCheck = command('codesign', ['--verify', '--deep', '--strict', '--verbose=2', artifact]);
  console.log(fileInfo.output.trim());
  console.log(signature.output.split('\n').filter((line) => /Signature=|Authority=|TeamIdentifier=|Identifier=/.test(line)).join('\n'));
  if (!signatureCheck.ok) {
    if (strict) throw new Error(`macOS code signature verification failed: ${signatureCheck.output.trim()}`);
    console.warn(`warning: macOS app is not deeply signed (${signatureCheck.output.trim()})`);
  }
  if (strict && !fileInfo.ok) throw new Error('cannot inspect macOS executable');
  if (strict && isMac) {
    for (const binary of [executable, worker]) {
      const architectures = command('lipo', ['-archs', binary]);
      if (!architectures.ok || !['arm64', 'x86_64'].every((name) => architectures.output.split(/\s+/).includes(name))) {
        throw new Error(`strict macOS release requires arm64 and x86_64 in ${basename(binary)}`);
      }
    }
    const appTeam = signature.output.match(/^TeamIdentifier=([A-Z0-9]{10})$/m)?.[1];
    if (!signature.ok || !signature.output.includes('Authority=Developer ID Application:')) {
      throw new Error('strict macOS release requires Developer ID Application signing');
    }
    if (!appTeam) {
      throw new Error('strict macOS release requires a team identifier');
    }
    const workerSignature = command('codesign', ['-dv', '--verbose=4', worker]);
    const workerSignatureCheck = command('codesign', ['--verify', '--strict', '--verbose=2', worker]);
    if (!workerSignature.ok || !workerSignatureCheck.ok
        || !workerSignature.output.includes('Authority=Developer ID Application:')
        || !workerSignature.output.includes(`TeamIdentifier=${appTeam}`)) {
      throw new Error('strict macOS release requires worker signing by the app Developer ID team');
    }
  }
} else if (strict && !['win32'].includes(platform())) {
  console.warn(`artifact ${basename(artifact)} cannot be deeply inspected on ${platform()}`);
}

const size = totalSize(artifact);
console.log(`artifact verified: ${artifact}`);
console.log(`version: ${version}; host: ${platform()} ${arch()}; size: ${size} bytes`);
