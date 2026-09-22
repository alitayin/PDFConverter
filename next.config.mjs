import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const version = readFileSync(resolve(import.meta.dirname, 'VERSION'), 'utf8').trim();

/** @type {import('next').NextConfig} */
const nextConfig = {
  output: 'export',
  outputFileTracingRoot: new URL('.', import.meta.url).pathname,
  trailingSlash: true,
  images: { unoptimized: true },
  env: { NEXT_PUBLIC_APP_VERSION: version }
};

export default nextConfig;
