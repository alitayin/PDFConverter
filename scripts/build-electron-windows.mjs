import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, unlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { inspectPdfiumRuntime, sha256 } from './check-windows-pdfium.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const strict = process.env.RELEASE_STRICT === '1';
const sourceRuntime = join(root, 'rust-engine', 'target', 'pdfium-windows-x64');
const stage = join(root, 'electron', 'bin', 'windows');
const stageRuntime = join(stage, 'pdfium-runtime');
const stageOffice = join(stage, 'office');
const engineName = 'minimal-pdf-converter.exe';
const electronName = 'MinimalPdfConverter.exe';

function run(command, args, options = {}) {
  return execFileSync(command, args, { cwd: root, stdio: 'inherit', timeout: 600_000, ...options });
}

function realFile(path) {
  if (!existsSync(path) || !lstatSync(path).isFile() || lstatSync(path).isSymbolicLink() || lstatSync(path).size === 0) {
    throw new Error(`Windows Electron build requires a real, nonempty file: ${path}`);
  }
}

export function assertStagedLayout(dir, expectedEngineHash, phase = 'source', inspect = inspectPdfiumRuntime) {
  if (!existsSync(dir) || !lstatSync(dir).isDirectory() || lstatSync(dir).isSymbolicLink()) {
    throw new Error('Windows Electron engine staging directory must be a real directory');
  }
  if (readdirSync(dir).sort().join(',') !== `${engineName},office,pdfium-runtime`) {
    throw new Error('Windows Electron engine staging has missing or unexpected entries');
  }
  const engine = join(dir, engineName);
  realFile(engine);
  if (sha256(engine) !== expectedEngineHash) throw new Error('staged Windows engine differs from the just-built binary');
  if (!existsSync(join(dir, 'pdfium-runtime')) || !lstatSync(join(dir, 'pdfium-runtime')).isDirectory() ||
      lstatSync(join(dir, 'pdfium-runtime')).isSymbolicLink()) {
    throw new Error('Windows PDFium runtime must be a real sibling directory of the Rust engine');
  }
  inspect(join(dir, 'pdfium-runtime'), phase);
}

export function inspectBundledOffice(dir, expectedHash) {
  let current = dir;
  for (const part of [null, 'LibreOffice', 'program']) {
    if (part) current = join(current, part);
    if (!existsSync(current) || !lstatSync(current).isDirectory() || lstatSync(current).isSymbolicLink()) {
      throw new Error(`Windows bundled Office directory is missing or symlinked: ${current}`);
    }
  }
  for (const file of ['LICENSE', 'NOTICE']) realFile(join(dir, 'LibreOffice', file));
  const executable = join(current, 'soffice.exe');
  realFile(executable);
  const hash = sha256(executable);
  if (expectedHash && hash !== expectedHash) throw new Error('installed Windows Office executable differs from the staged runtime');
  return { executable, hash };
}

export function assertOfficeReleaseReady(releaseStrict) {
  if (releaseStrict) {
    throw new Error('strict Windows Electron release requires verified official LibreOffice MSI provenance, nested signatures, and component notices');
  }
}

function signingInputs() {
  const thumbprint = (process.env.WINDOWS_CERTIFICATE_THUMBPRINT || '').replace(/\s/g, '').toUpperCase();
  const timestamp = process.env.WINDOWS_TIMESTAMP_URL?.trim();
  if (!/^[0-9A-F]{40}$/.test(thumbprint)) throw new Error('strict Electron Windows build requires WINDOWS_CERTIFICATE_THUMBPRINT');
  if (!timestamp || !URL.canParse(timestamp) || new URL(timestamp).protocol !== 'https:') {
    throw new Error('strict Electron Windows build requires an HTTPS WINDOWS_TIMESTAMP_URL');
  }
  return { thumbprint, timestamp };
}

function findSignTool() {
  const script = [
    String.raw`$tools = @(Get-ChildItem -LiteralPath 'C:\Program Files (x86)\Windows Kits\10\bin' -Filter signtool.exe -Recurse -File -ErrorAction Stop | Where-Object { (Split-Path $_.DirectoryName -Leaf) -eq 'x64' } | Sort-Object FullName -Descending)`,
    'if ($tools.Count -eq 0) { throw "Windows SDK x64 signtool.exe is required" }',
    '$tools[0].FullName',
  ].join('; ');
  const path = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
    cwd: root, encoding: 'utf8', timeout: 30_000,
  }).trim();
  realFile(path);
  return path;
}

function checkExistingInstall() {
  const script = [
    "$keys = @('HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall', 'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall', 'HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall')",
    '$matches = @($keys | ForEach-Object { Get-ChildItem -LiteralPath $_ -ErrorAction SilentlyContinue | Get-ItemProperty -ErrorAction SilentlyContinue } | Where-Object { $_.DisplayName -eq $env:MPC_SMOKE_PRODUCT_NAME })',
    'if ($matches.Count -ne 0) { throw "an existing product installation blocks the isolated NSIS smoke" }',
  ].join('; ');
  run('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
    timeout: 30_000, env: { ...process.env, MPC_SMOKE_PRODUCT_NAME: 'Ayst Arc PDF' },
  });
}

function stopInstalledProcesses(installDir) {
  const script = [
    '$root = [IO.Path]::GetFullPath($env:MPC_SMOKE_INSTALL_DIR).TrimEnd([IO.Path]::DirectorySeparatorChar)',
    '$processes = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object { ($_.ExecutablePath -and $_.ExecutablePath.StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase)) -or ($_.CommandLine -and $_.CommandLine.IndexOf($root, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) })',
    '$processes | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }',
  ].join('; ');
  run('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
    timeout: 30_000,
    env: { ...process.env, MPC_SMOKE_INSTALL_DIR: installDir },
  });
}

function waitForRemoval(path, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  const pause = new Int32Array(new SharedArrayBuffer(4));
  while (existsSync(path) && Date.now() < deadline) Atomics.wait(pause, 0, 0, 250);
}

function removeSmokeRoot(smokeRoot, installDir) {
  const deadline = Date.now() + 30_000;
  while (existsSync(smokeRoot)) {
    try {
      rmSync(smokeRoot, { recursive: true, force: true });
      return;
    } catch (error) {
      if (!['EBUSY', 'EPERM', 'ENOTEMPTY'].includes(error?.code) || Date.now() >= deadline) throw error;
      stopInstalledProcesses(installDir);
      const pause = new Int32Array(new SharedArrayBuffer(4));
      Atomics.wait(pause, 0, 0, 250);
    }
  }
}

function smokeInstalledNsis(installer, expectedEngineHash, expectedOfficeHash) {
  checkExistingInstall();
  const smokeRoot = mkdtempSync(join(tmpdir(), 'minimal-pdf-electron-nsis-'));
  const installDir = join(smokeRoot, 'installed');
  const installedEngine = join(installDir, 'resources', 'bin', engineName);
  const installedOffice = join(installDir, 'resources', 'office');
  let installed = false;
  let uninstalled = false;
  try {
    // NSIS needs /D= as the last, unquoted argument even when the path has spaces.
    run(installer, ['/S', `/D=${installDir}`], { windowsVerbatimArguments: true, timeout: 300_000 });
    installed = true;
    realFile(join(installDir, electronName));
    realFile(installedEngine);
    if (sha256(installedEngine) !== expectedEngineHash) throw new Error('installed Rust engine differs from the staged signed binary');
    const office = inspectBundledOffice(installedOffice, expectedOfficeHash);
    run(office.executable, ['--version'], { timeout: 30_000 });
    if (strict) {
      run('node', ['scripts/verify-windows-artifact.mjs', join(installDir, electronName)]);
      run('node', ['scripts/verify-windows-artifact.mjs', installedEngine]);
      run('node', ['scripts/check-windows-pdfium.mjs', 'signed'], {
        env: { ...process.env, PDFIUM_RUNTIME_DIR: join(dirname(installedEngine), 'pdfium-runtime') },
      });
    }
    run('node', ['scripts/windows-pdfium-smoke.mjs', installedEngine, stageRuntime], { timeout: 300_000 });
    run('node', ['--test', 'electron/format-conversions.integration.test.mjs'], {
      timeout: 300_000,
      env: {
        ...process.env,
        MINIMALPDF_RUST_ENGINE: installedEngine,
        MINIMALPDF_OFFICE_MODE: 'bundled',
        MINIMALPDF_OFFICE_EXECUTABLE: office.executable,
        MINIMALPDF_REQUIRE_OFFICE: '1',
        PYTHONDONTWRITEBYTECODE: '1',
      },
    });
  } finally {
    if (installed) {
      // Office and the Rust bridge are normally closed by the integration
      // tests, but Windows can retain an executable handle briefly. Stop only
      // processes launched from this isolated install before invoking NSIS.
      stopInstalledProcesses(installDir);
      const uninstallers = existsSync(installDir) ? readdirSync(installDir)
        .filter((name) => /^Uninstall.*\.exe$/i.test(name)) : [];
      if (uninstallers.length !== 1) throw new Error(`cannot identify isolated NSIS uninstaller; inspect ${smokeRoot}`);
      run(join(installDir, uninstallers[0]), ['/S'], { timeout: 180_000 });
      stopInstalledProcesses(installDir);
      waitForRemoval(installedEngine);
      if (existsSync(installedEngine)) throw new Error(`NSIS uninstaller left engine behind; inspect ${smokeRoot}`);
      uninstalled = true;
    }
    if (uninstalled) removeSmokeRoot(smokeRoot, installDir);
  }
}

function main() {
  if (process.platform !== 'win32' || process.arch !== 'x64') throw new Error('Windows Electron x64 NSIS requires a Windows x64 build host');
  assertOfficeReleaseReady(strict);
  if (!existsSync(join(root, 'out', 'index.html'))) throw new Error('Next.js static export is missing; run pnpm build');
  realFile(join(root, 'node_modules', 'electron-builder', 'cli.js'));
  const targets = execFileSync('rustup', ['target', 'list', '--installed'], { cwd: root, encoding: 'utf8' });
  if (!targets.split(/\r?\n/).includes('x86_64-pc-windows-msvc')) throw new Error('Rust x86_64-pc-windows-msvc target is missing');
  const signing = strict ? signingInputs() : null;
  run('node', ['scripts/check-version.mjs']);

  inspectPdfiumRuntime(sourceRuntime, 'source');
  const officeHash = inspectBundledOffice(stageOffice).hash;
  const oldSignedMarker = join(stageRuntime, 'RUNTIME_DLL_SHA256_SIGNED');
  if (existsSync(stage)) {
    if (!lstatSync(stage).isDirectory() || lstatSync(stage).isSymbolicLink() ||
        readdirSync(stage).some((name) => name !== engineName && name !== 'pdfium-runtime' && name !== 'office')) {
      throw new Error('unexpected files or symlink in generated Windows Electron staging');
    }
    if (existsSync(join(stage, engineName))) realFile(join(stage, engineName));
    if (existsSync(stageRuntime) && (!lstatSync(stageRuntime).isDirectory() || lstatSync(stageRuntime).isSymbolicLink())) {
      throw new Error('Windows PDFium staging path is not a real directory');
    }
    if (existsSync(stageRuntime)) {
      inspectPdfiumRuntime(stageRuntime, existsSync(oldSignedMarker) ? 'signed' : 'source');
    }
  }
  mkdirSync(stage, { recursive: true });
  if (existsSync(oldSignedMarker)) {
    realFile(oldSignedMarker);
    unlinkSync(oldSignedMarker);
  }
  cpSync(sourceRuntime, stageRuntime, { recursive: true, force: true });
  const stagedPdfium = inspectPdfiumRuntime(stageRuntime, 'source');
  let signedHash;
  if (signing) {
    run(findSignTool(), ['sign', '/fd', 'SHA256', '/tr', signing.timestamp, '/td', 'SHA256', '/sha1', signing.thumbprint, stagedPdfium.dll]);
    signedHash = sha256(stagedPdfium.dll);
    writeFileSync(oldSignedMarker, signedHash, { encoding: 'ascii', flag: 'wx' });
    run('node', ['scripts/check-windows-pdfium.mjs', 'signed'], {
      env: { ...process.env, PDFIUM_RUNTIME_DIR: stageRuntime },
    });
  }

  const env = { ...process.env };
  if (signedHash) env.MPC_PDFIUM_SIGNED_SHA256 = signedHash;
  else delete env.MPC_PDFIUM_SIGNED_SHA256;
  run('cargo', ['build', '--locked', '--release', '--target', 'x86_64-pc-windows-msvc', '--manifest-path', 'rust-engine/Cargo.toml', '--bin', 'minimal-pdf-converter'], {
    env, timeout: 1_800_000,
  });
  const compiled = join(root, 'rust-engine', 'target', 'x86_64-pc-windows-msvc', 'release', engineName);
  realFile(compiled);
  cpSync(compiled, join(stage, engineName));
  if (signing) {
    run(findSignTool(), ['sign', '/fd', 'SHA256', '/tr', signing.timestamp, '/td', 'SHA256', '/sha1', signing.thumbprint, join(stage, engineName)]);
    run('node', ['scripts/verify-windows-artifact.mjs', join(stage, engineName)]);
  }
  const engineHash = sha256(join(stage, engineName));
  assertStagedLayout(stage, engineHash, signing ? 'signed' : 'source');
  inspectBundledOffice(stageOffice, officeHash);

  const version = readFileSync(join(root, 'VERSION'), 'utf8').trim();
  const installer = join(root, 'dist-electron', `MinimalPdfConverter-${version}-x64-Setup.exe`);
  const before = existsSync(installer) ? { hash: sha256(installer), modified: statSync(installer).mtimeMs } : null;
  const buildArgs = ['node_modules/electron-builder/cli.js', '--win', 'nsis', '--x64', '--config', 'electron-builder.yml', '--publish', 'never'];
  if (signing) buildArgs.push(
    '--config.win.forceCodeSigning=true',
    `--config.win.signtoolOptions.certificateSha1=${signing.thumbprint}`,
    `--config.win.signtoolOptions.rfc3161TimeStampServer=${signing.timestamp}`,
  );
  run('node', buildArgs, { env: { ...process.env, CSC_IDENTITY_AUTO_DISCOVERY: signing ? 'true' : 'false' }, timeout: 1_800_000 });
  realFile(installer);
  if (before && before.hash === sha256(installer) && before.modified === statSync(installer).mtimeMs) {
    throw new Error('Electron builder did not produce a new NSIS installer');
  }
  if (signing) run('node', ['scripts/verify-windows-artifact.mjs', installer]);
  smokeInstalledNsis(installer, engineHash, officeHash);
  console.log(`Windows Electron NSIS installed-engine smoke passed: ${installer}`);
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  try {
    main();
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 2;
  }
}
