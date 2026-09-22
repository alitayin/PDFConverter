import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const root = resolve(import.meta.dirname, '..');
const version = readFileSync(resolve(root, 'VERSION'), 'utf8').trim();
const packageJson = JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8'));
const cargo = readFileSync(resolve(root, 'rust-engine/Cargo.toml'), 'utf8');
const page = readFileSync(resolve(root, 'src/app/page.tsx'), 'utf8');
const nextConfig = readFileSync(resolve(root, 'next.config.mjs'), 'utf8');
const cargoVersion = cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
const values = { VERSION: version, package: packageJson.version, cargo: cargoVersion };
const mismatches = Object.entries(values).filter(([, value]) => value !== version);
if (mismatches.length) {
  console.error('Version mismatch:', values);
  process.exit(1);
}
if (process.env.GITHUB_REF_TYPE === 'tag' && process.env.GITHUB_REF_NAME !== `v${version}`) {
  console.error(`Git tag ${process.env.GITHUB_REF_NAME || '(missing)'} does not match v${version}`);
  process.exit(1);
}
if (/v\d+\.\d+\.\d+(?:-[\w.]+)?/.test(page)) {
  console.error('Frontend contains a hard-coded semantic version; use NEXT_PUBLIC_APP_VERSION');
  process.exit(1);
}
if (!nextConfig.includes("NEXT_PUBLIC_APP_VERSION: version")) {
  console.error('Next.js build does not inject VERSION into the frontend');
  process.exit(1);
}
console.log(`version ${version}`);
