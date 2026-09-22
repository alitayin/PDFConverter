import { existsSync, lstatSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { execFileSync } from 'node:child_process';
import { inspectPdfiumRuntime, sha256 } from './check-windows-pdfium.mjs';

const strict = process.env.RELEASE_STRICT === '1';
const runtime = process.env.WEBVIEW2_FIXED_RUNTIME_DIR;
const bundledRuntimePath = 'target/webview2-fixed';
const runtimeSource = resolve('src-tauri', bundledRuntimePath);
const bundledPdfiumPath = 'target/pdfium-windows-x64';
const pdfiumSource = resolve('src-tauri', bundledPdfiumPath);
const thumbprint = (process.env.WINDOWS_CERTIFICATE_THUMBPRINT || '').replace(/\s/g, '').toUpperCase();
const timestampUrl = process.env.WINDOWS_TIMESTAMP_URL?.trim();
if (process.platform !== 'win32') throw new Error('Windows installer and install smoke require a Windows host');
if (!runtime || resolve(runtime).toLowerCase() !== runtimeSource.toLowerCase() || !existsSync(runtimeSource)) {
  console.error(`set WEBVIEW2_FIXED_RUNTIME_DIR to the staged runtime at ${runtimeSource}`);
  process.exit(2);
}
if (!process.env.PDFIUM_RUNTIME_DIR || resolve(process.env.PDFIUM_RUNTIME_DIR).toLowerCase() !== pdfiumSource.toLowerCase()) {
  console.error(`set PDFIUM_RUNTIME_DIR to the staged runtime at ${pdfiumSource}`);
  process.exit(2);
}
if (strict && !/^[0-9A-F]{40}$/.test(thumbprint)) {
  console.error('strict Windows release requires a SHA-1 WINDOWS_CERTIFICATE_THUMBPRINT');
  process.exit(2);
}
if (strict && (!timestampUrl || !URL.canParse(timestampUrl) || new URL(timestampUrl).protocol !== 'https:')) {
  console.error('strict Windows release requires an HTTPS WINDOWS_TIMESTAMP_URL');
  process.exit(2);
}

execFileSync('node', ['scripts/check-version.mjs'], { stdio: 'inherit', env: process.env });
const pdfium = inspectPdfiumRuntime(pdfiumSource, 'source');
let signedPdfiumHash;
if (strict) {
  const sdkDirectory = String.raw`C:\Program Files (x86)\Windows Kits\10\bin`;
  const findSignTool = [
    `$tools = @(Get-ChildItem -LiteralPath '${sdkDirectory}' -Filter signtool.exe -Recurse -File -ErrorAction Stop | Where-Object { (Split-Path $_.DirectoryName -Leaf) -eq 'x64' } | Sort-Object FullName -Descending)`,
    'if ($tools.Count -eq 0) { throw "Windows SDK x64 signtool.exe is required" }',
    '$tools[0].FullName',
  ].join('; ');
  const signtool = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', findSignTool], { encoding: 'utf8' }).trim();
  execFileSync(signtool, ['sign', '/fd', 'SHA256', '/tr', timestampUrl, '/td', 'SHA256', '/sha1', thumbprint, pdfium.dll], { stdio: 'inherit' });
  signedPdfiumHash = sha256(pdfium.dll);
  writeFileSync(join(pdfiumSource, 'RUNTIME_DLL_SHA256_SIGNED'), signedPdfiumHash, { encoding: 'ascii', flag: 'wx' });
  execFileSync('node', ['scripts/check-windows-pdfium.mjs', 'signed'], { stdio: 'inherit', env: process.env });
} else {
  execFileSync('node', ['scripts/check-windows-pdfium.mjs', 'source'], { stdio: 'inherit', env: process.env });
}
const buildEnv = { ...process.env };
if (signedPdfiumHash) buildEnv.MPC_PDFIUM_SIGNED_SHA256 = signedPdfiumHash;
else delete buildEnv.MPC_PDFIUM_SIGNED_SHA256;

const tauriConfig = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8'));
const productName = tauriConfig.productName;
const manufacturer = tauriConfig.bundle?.publisher || tauriConfig.identifier.split('.')[1] || tauriConfig.identifier;
const registrationEnv = (installDir) => ({
  ...process.env,
  MPC_SMOKE_PRODUCT_NAME: productName,
  MPC_SMOKE_MANUFACTURER: manufacturer,
  MPC_SMOKE_INSTALL_DIR: installDir,
});

function checkNoExistingInstall() {
  const script = [
    '$product = $env:MPC_SMOKE_PRODUCT_NAME',
    '$maker = $env:MPC_SMOKE_MANUFACTURER',
    "$uninstall = Join-Path 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall' $product",
    "$registration = Join-Path (Join-Path 'HKCU:\\Software' $maker) $product",
    'if ((Test-Path -LiteralPath $uninstall) -or (Test-Path -LiteralPath $registration)) { throw "existing installation or settings: aborting isolated smoke" }',
    "$wix = Get-ChildItem -LiteralPath 'HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall' -ErrorAction SilentlyContinue | Get-ItemProperty -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -eq $product -and $_.Publisher -eq $maker }",
    'if ($wix) { throw "existing machine-wide installation: aborting isolated smoke" }',
  ].join('; ');
  execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
    stdio: 'inherit', timeout: 30_000, env: registrationEnv(''),
  });
}

function removeSmokeRegistration(installDir) {
  const script = [
    '$product = $env:MPC_SMOKE_PRODUCT_NAME',
    '$maker = $env:MPC_SMOKE_MANUFACTURER',
    "$uninstall = Join-Path 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall' $product",
    "$registration = Join-Path (Join-Path 'HKCU:\\Software' $maker) $product",
    '$productKey = Get-Item -LiteralPath $registration -ErrorAction SilentlyContinue',
    '$uninstallKey = Get-Item -LiteralPath $uninstall -ErrorAction SilentlyContinue',
    'if ($productKey -and [string]$productKey.GetValue(\'\') -ne $env:MPC_SMOKE_INSTALL_DIR) { throw "installation registration points elsewhere: refusing cleanup" }',
    'if ($uninstallKey -and ([string]$uninstallKey.GetValue(\'InstallLocation\')).Trim(\'"\') -ne $env:MPC_SMOKE_INSTALL_DIR) { throw "uninstall registration points elsewhere: refusing cleanup" }',
    'if ($uninstallKey) { Remove-Item -LiteralPath $uninstall -Force -ErrorAction Stop }',
    'if ($productKey) { Remove-Item -LiteralPath $registration -Force -ErrorAction Stop }',
  ].join('; ');
  execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
    stdio: 'inherit', timeout: 30_000, env: registrationEnv(installDir),
  });
}

function smokeInstalledNsis(installer) {
  checkNoExistingInstall();
  const root = mkdtempSync(join(tmpdir(), 'minimal-pdf-nsis-smoke-'));
  const installDir = join(root, 'installed');
  const app = join(installDir, 'minimal-pdf-converter.exe');
  let installed = false;
  try {
    // NSIS requires /D= to be last and unquoted, even when the path contains spaces.
    execFileSync(installer, ['/S', '/NS', `/D=${installDir}`], {
      windowsVerbatimArguments: true, stdio: 'inherit', timeout: 300_000,
    });
    installed = true;
    if (!existsSync(app)) throw new Error('NSIS did not install the expected app executable');
    if (strict) {
      execFileSync('node', ['scripts/check-windows-pdfium.mjs', 'signed'], {
        stdio: 'inherit', env: { ...process.env, PDFIUM_RUNTIME_DIR: join(installDir, 'pdfium-runtime') }, timeout: 60_000,
      });
    }
    execFileSync('node', ['scripts/windows-pdfium-smoke.mjs', app, pdfiumSource], {
      stdio: 'inherit', env: process.env, timeout: 300_000,
    });
  } finally {
    if (!lstatSync(root).isDirectory() || lstatSync(root).isSymbolicLink()) {
      throw new Error('isolated NSIS smoke directory changed; refusing cleanup');
    }
    let cleanupError;
    try {
      const uninstaller = join(installDir, 'uninstall.exe');
      if (installed && !existsSync(uninstaller)) throw new Error('installed NSIS uninstaller is missing');
      if (existsSync(uninstaller)) {
        execFileSync(uninstaller, ['/S', `_?=${installDir}`], {
          windowsVerbatimArguments: true, stdio: 'inherit', timeout: 180_000,
        });
        if (existsSync(app)) throw new Error('NSIS uninstall left the app executable in place');
      }
    } catch (error) {
      cleanupError = error;
    }
    try {
      removeSmokeRegistration(installDir);
    } catch (error) {
      cleanupError ||= error;
    }
    try {
      rmSync(root, { recursive: true, force: true });
    } catch (error) {
      cleanupError ||= error;
    }
    if (cleanupError) throw cleanupError;
  }
}

const bundleDir = resolve('src-tauri/target/release/bundle/nsis');
const version = readFileSync('VERSION', 'utf8').trim();
const installedBefore = new Map(existsSync(bundleDir)
  ? readdirSync(bundleDir).filter((name) => name.toLowerCase().endsWith('.exe')).map((name) => {
    const path = join(bundleDir, name);
    return [name, { digest: sha256(path), modified: statSync(path).mtimeMs }];
  })
  : []);

const config = {
  bundle: {
    targets: ['nsis'],
    windows: {
      webviewInstallMode: { type: 'fixedRuntime', path: bundledRuntimePath },
      nsis: { installMode: 'currentUser' },
      ...(thumbprint ? { certificateThumbprint: thumbprint } : {}),
      ...(timestampUrl ? { timestampUrl, tsp: true } : {}),
    },
    resources: { [bundledPdfiumPath]: 'pdfium-runtime/' },
  },
};
execFileSync('node', ['scripts/check-windows-runtime.mjs'], { stdio: 'inherit', env: process.env });
execFileSync('cargo', ['tauri', 'build', '--bundles', 'nsis', '--config', JSON.stringify(config)], { stdio: 'inherit', env: buildEnv });

const installers = existsSync(bundleDir) ? readdirSync(bundleDir)
  .filter((name) => name.toLowerCase().endsWith('.exe') && name.includes(version))
  .filter((name) => {
    const path = join(bundleDir, name);
    const previous = installedBefore.get(name);
    return !previous || previous.digest !== sha256(path) || previous.modified < statSync(path).mtimeMs;
  })
  .map((name) => join(bundleDir, name)) : [];
if (installers.length !== 1) {
  throw new Error(`expected exactly one newly built ${version} NSIS installer in ${bundleDir}, found ${installers.length}`);
}
execFileSync('node', ['scripts/verify-windows-artifact.mjs', installers[0]], { stdio: 'inherit', env: process.env });
if (strict) execFileSync('node', ['scripts/check-windows-pdfium.mjs', 'signed'], { stdio: 'inherit', env: process.env });
smokeInstalledNsis(installers[0]);
execFileSync('node', ['scripts/generate-release-manifest.mjs', '--artifact', installers[0]], { stdio: 'inherit', env: process.env });
console.log(`Windows installer verified: ${installers[0]}`);
