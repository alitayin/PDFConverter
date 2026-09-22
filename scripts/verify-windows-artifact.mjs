import { existsSync, statSync } from 'node:fs';
import { resolve } from 'node:path';
import { execFileSync } from 'node:child_process';
import { platform } from 'node:os';

const artifact = process.argv[2] ? resolve(process.argv[2]) : '';
const strict = process.env.RELEASE_STRICT === '1';
const expectedThumbprint = (process.env.WINDOWS_CERTIFICATE_THUMBPRINT || '').replace(/\s/g, '').toUpperCase();

if (!artifact.toLowerCase().endsWith('.exe') || !existsSync(artifact) || !statSync(artifact).isFile()) {
  throw new Error('Windows installer artifact is missing');
}
if (strict && !/^[0-9A-F]{40}$/.test(expectedThumbprint)) {
  throw new Error('strict Windows release requires a SHA-1 WINDOWS_CERTIFICATE_THUMBPRINT');
}
if (platform() !== 'win32') {
  const message = 'Authenticode verification requires a Windows host';
  if (strict) throw new Error(message);
  console.warn(`warning: ${message}`);
  process.exit(0);
}

const script = [
  '$signature = Get-AuthenticodeSignature -LiteralPath $env:MPC_INSTALLER_PATH -ErrorAction Stop',
  '[pscustomobject]@{ Status = [string]$signature.Status; Thumbprint = [string]$signature.SignerCertificate.Thumbprint; Timestamped = [bool]$signature.TimeStamperCertificate } | ConvertTo-Json -Compress',
].join('; ');
const output = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
  encoding: 'utf8',
  env: { ...process.env, MPC_INSTALLER_PATH: artifact },
});
const signature = JSON.parse(output.trim());
if (signature.Status !== 'Valid') throw new Error(`Authenticode status is ${signature.Status}`);
if (strict && signature.Thumbprint?.toUpperCase() !== expectedThumbprint) {
  throw new Error('Authenticode signer thumbprint does not match WINDOWS_CERTIFICATE_THUMBPRINT');
}
if (strict && !signature.Timestamped) throw new Error('strict Windows release requires a trusted timestamp');
console.log(`Authenticode: ${signature.Status}; thumbprint: ${signature.Thumbprint}; timestamped: ${signature.Timestamped}`);
