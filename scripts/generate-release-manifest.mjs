import { createHash, randomUUID } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { arch, homedir, platform } from 'node:os';
import { basename, join, relative, resolve } from 'node:path';
import { inspectPdfiumRuntime, LICENSE_FILES } from './check-windows-pdfium.mjs';

const root = resolve(import.meta.dirname, '..');
const version = readFileSync(join(root, 'VERSION'), 'utf8').trim();
const args = process.argv.slice(2);
const sourceOnly = args.includes('--source-only');
const artifactIndex = args.indexOf('--artifact');
const artifact = artifactIndex >= 0 ? args[artifactIndex + 1] : undefined;
const outputRoot = resolve(root, process.env.RELEASE_OUTPUT_DIR || join('release', version, `${platform()}-${arch()}`));
mkdirSync(outputRoot, { recursive: true });

function readJson(path) {
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return undefined;
  }
}

function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

function walkFiles(path, prefix = '') {
  if (!existsSync(path)) return [];
  const info = statSync(path);
  if (info.isFile()) return [{ path, relativePath: prefix || basename(path), size: info.size, sha256: sha256(path) }];
  return readdirSync(path, { withFileTypes: true }).flatMap((entry) =>
    walkFiles(join(path, entry.name), join(prefix, entry.name)),
  );
}

function packageKeyParts(key) {
  const clean = key.replace(/^['"]|['"]$/g, '').replace(/^npm:/, '');
  const at = clean.lastIndexOf('@');
  if (at <= 0 || at === clean.length - 1) return undefined;
  return { name: clean.slice(0, at), version: clean.slice(at + 1).split('(')[0] };
}

function parsePnpmPackages() {
  const lockPath = join(root, 'pnpm-lock.yaml');
  if (!existsSync(lockPath)) return [];
  const lines = readFileSync(lockPath, 'utf8').split('\n');
  const packages = new Map();
  let section = '';
  for (const line of lines) {
    if (/^(packages|snapshots|importers):$/.test(line.trim())) {
      section = line.trim().slice(0, -1);
      continue;
    }
    if (section !== 'packages') continue;
    const match = line.match(/^  ([^\s][^:]*):$/);
    if (!match) continue;
    const parsed = packageKeyParts(match[1]);
    if (parsed && !packages.has(`${parsed.name}@${parsed.version}`)) packages.set(`${parsed.name}@${parsed.version}`, parsed);
  }
  return [...packages.values()];
}

function nodeManifest(name, wantedVersion) {
  const encoded = name.replaceAll('/', '+');
  const pnpmRoots = [
    join(root, 'node_modules', '.pnpm'),
    join(homedir(), 'node_modules', '.pnpm'),
  ];
  for (const pnpmRoot of pnpmRoots) {
    if (!existsSync(pnpmRoot)) continue;
    for (const entry of readdirSync(pnpmRoot)) {
      if (!entry.startsWith(`${encoded}@`) && !(name.startsWith('@') && entry.startsWith(`${name.replace('/', '+')}@`))) continue;
      const candidate = join(pnpmRoot, entry, 'node_modules', name, 'package.json');
      const json = readJson(candidate);
      if (json && (!wantedVersion || json.version === wantedVersion)) return { json, path: candidate };
    }
  }
  for (const nodeRoot of [join(root, 'node_modules'), join(homedir(), 'node_modules')]) {
    const candidate = join(nodeRoot, name, 'package.json');
    const direct = readJson(candidate);
    if (direct && (!wantedVersion || direct.version === wantedVersion)) return { json: direct, path: candidate };
  }
  return undefined;
}

// These packages publish platform-specific optional binaries. They are not
// part of a host build when their package directory is absent, but their
// license still needs to remain visible in a source inventory.
const knownOptionalLicenses = new Map([
  ['@emnapi/runtime', 'MIT'],
  ['sharp-platform', 'Apache-2.0'],
]);

function packageLicense(name, json) {
  if (typeof json?.license === 'string') return json.license;
  if (name.startsWith('@img/')) return 'Apache-2.0';
  if (name.startsWith('@next/swc-')) return 'MIT';
  return knownOptionalLicenses.get(name) || 'UNKNOWN';
}

function nodeComponents() {
  return parsePnpmPackages().map(({ name, version: wantedVersion }) => {
    const manifest = nodeManifest(name, wantedVersion);
    const json = manifest?.json;
    return {
      type: 'library',
      name,
      version: wantedVersion,
      license: packageLicense(name, json),
      purl: `pkg:npm/${name}@${wantedVersion}`,
      source: 'pnpm-lock.yaml',
    };
  });
}

function parseCargoPackages() {
  const lockPath = join(root, 'rust-engine', 'Cargo.lock');
  if (!existsSync(lockPath)) return [];
  const text = readFileSync(lockPath, 'utf8');
  return [...text.matchAll(/\[\[package\]\]\s+name = "([^"]+)"\s+version = "([^"]+)"/g)].map((match) => ({ name: match[1], version: match[2] }));
}

function cargoManifest(name, version) {
  const roots = [join(homedir(), '.cargo', 'registry', 'src'), '/usr/local/cargo/registry/src', '/root/.cargo/registry/src'];
  for (const registryRoot of roots) {
    if (!existsSync(registryRoot)) continue;
    for (const index of readdirSync(registryRoot)) {
      const candidate = join(registryRoot, index, `${name}-${version}`, 'Cargo.toml');
      if (existsSync(candidate)) return candidate;
    }
  }
  return undefined;
}

function cargoLicense(path) {
  if (!path) return 'UNKNOWN';
  const text = readFileSync(path, 'utf8');
  return text.match(/^license\s*=\s*"([^"]+)"/m)?.[1] || (text.match(/^license-file\s*=\s*"([^"]+)"/m) ? 'SEE-LICENSE-FILE' : 'UNKNOWN');
}

function cargoComponents() {
  return parseCargoPackages().map(({ name, version }) => {
    const manifest = cargoManifest(name, version);
    return {
      type: 'library',
      name,
      version,
      license: name === 'minimal-pdf-converter' ? 'SEE-ROOT-LICENSE' : name === 'windows-sys' && version === '0.52.0' ? 'MIT' : cargoLicense(manifest),
      purl: `pkg:cargo/${name}@${version}`,
      source: 'rust-engine/Cargo.lock',
    };
  });
}

function uniqueComponents(components) {
  const seen = new Set();
  return components.filter((component) => {
    const key = `${component.purl}|${component.version}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  }).sort((a, b) => a.purl.localeCompare(b.purl));
}

const platformComponents = platform() === 'win32' ? [{
  type: 'library',
  name: 'PDFium Windows x64 app-local runtime (bblanchon build)',
  version: '155.0.8057.0',
  license: 'SEE-PDFIUM-BUNDLED-LICENSES',
  purl: 'pkg:generic/pdfium@155.0.8057.0?arch=x64&builder=bblanchon',
  source: 'scripts/fetch-pdfium-runtime.ps1',
}] : [];
const components = uniqueComponents([...nodeComponents(), ...cargoComponents(), ...platformComponents]);
const unknown = components.filter((component) => component.license === 'UNKNOWN');
const bomComponents = components.map(({ license, source, ...component }) => ({
  ...component,
  licenses: license === 'UNKNOWN' ? undefined : [{
    ...(license.startsWith('SEE-') ? { license: { name: license } } : { expression: license }),
  }],
  properties: [{ name: 'minimal-pdf-converter:lockfile', value: source }],
}));
const appComponent = {
  type: 'application',
  name: 'minimal-pdf-converter',
  version,
  publisher: 'Minimal PDF Converter',
  purl: `pkg:generic/minimal-pdf-converter@${version}`,
};
const bom = {
  bomFormat: 'CycloneDX',
  specVersion: '1.5',
  serialNumber: `urn:uuid:${randomUUID()}`,
  version: 1,
  metadata: {
    timestamp: new Date().toISOString(),
    component: appComponent,
    properties: [{ name: 'minimal-pdf-converter:inventory', value: 'Cargo.lock, pnpm-lock.yaml and platform runtime inventory' }],
  },
  components: bomComponents,
};
const artifactFiles = artifact && existsSync(artifact) ? walkFiles(resolve(artifact)).map((file) => ({
  path: relative(root, file.path),
  size: file.size,
  sha256: file.sha256,
})) : [];
let pdfiumFiles = [];
if (platform() === 'win32' && artifact) {
  const runtime = process.env.PDFIUM_RUNTIME_DIR ? resolve(process.env.PDFIUM_RUNTIME_DIR) : undefined;
  if (!runtime) throw new Error('Windows artifact inventory requires PDFIUM_RUNTIME_DIR');
  inspectPdfiumRuntime(runtime, process.env.RELEASE_STRICT === '1' ? 'signed' : 'source');
  pdfiumFiles = walkFiles(runtime, 'pdfium-runtime').map(({ relativePath: path, size, sha256 }) => ({ path, size, sha256 }));
  const noticeDir = join(outputRoot, 'pdfium', 'licenses');
  mkdirSync(noticeDir, { recursive: true });
  copyFileSync(join(runtime, 'LICENSE'), join(outputRoot, 'pdfium', 'LICENSE'));
  for (const file of LICENSE_FILES) copyFileSync(join(runtime, 'licenses', file), join(noticeDir, file));
}
const componentManifest = {
  product: appComponent,
  platform: platform(),
  architecture: arch(),
  sourceOnly,
  artifact: artifact ? relative(root, resolve(artifact)) : null,
  generatedAt: new Date().toISOString(),
  files: artifactFiles,
  appLocalRuntimeFiles: pdfiumFiles,
  dependencyCount: components.length,
  unknownLicenseCount: unknown.length,
};

const licenseLines = [
  '# Third-party licenses',
  '',
  `Generated for ${appComponent.name} ${version}.`,
  '',
  '| Component | Version | License | Source |',
  '| --- | --- | --- | --- |',
  ...components.map((component) => `| ${component.name} | ${component.version} | ${component.license} | ${component.source} |`),
  '',
  unknown.length ? `Unknown licenses: ${unknown.length}. Strict release mode must not ship this inventory.` : 'All discovered components expose a license expression or license file.',
  ...(platform() === 'win32' ? ['PDFium full license texts and third-party notices: `pdfium/LICENSE` and `pdfium/licenses/` (also bundled inside the installer).'] : []),
  '',
];

writeFileSync(join(outputRoot, 'sbom.cdx.json'), `${JSON.stringify(bom, null, 2)}\n`);
writeFileSync(join(outputRoot, 'components.json'), `${JSON.stringify(componentManifest, null, 2)}\n`);
writeFileSync(join(outputRoot, 'THIRD_PARTY_LICENSES.md'), licenseLines.join('\n'));
writeFileSync(join(outputRoot, 'LICENSE'), readFileSync(join(root, 'LICENSE')));
writeFileSync(join(outputRoot, 'NOTICE'), `${readFileSync(join(root, 'NOTICE.md'))}\n\n${licenseLines.slice(5).join('\n')}`);

console.log(`release metadata: ${outputRoot}`);
console.log(`dependencies: ${components.length}; unknown licenses: ${unknown.length}`);
if (process.env.RELEASE_STRICT === '1' && unknown.length) process.exitCode = 2;
