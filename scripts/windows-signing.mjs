// Authenticode boundaries shared by the native release, Electron signing hook and verifier.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ensurePrivateDirectory } from './private-control.mjs';

export function windowsSigningConfigured(environment = process.env) {
  return Boolean(environment.WIN_CSC_LINK || environment.CSC_LINK);
}

export function inspectAuthenticode(filename) {
  assert.equal(process.platform, 'win32', 'Authenticode is verified on Windows');
  const script = `$ErrorActionPreference = 'Stop'\n$s = Get-AuthenticodeSignature -LiteralPath $env:VIBEFACE_SIGNATURE_FILE\n@{ status = $s.Status.ToString(); subject = $s.SignerCertificate.Subject; thumbprint = $s.SignerCertificate.Thumbprint; timestamped = ($null -ne $s.TimeStamperCertificate) } | ConvertTo-Json -Compress`;
  const output = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-EncodedCommand', Buffer.from(script, 'utf16le').toString('base64')], {
    encoding: 'utf8', windowsHide: true, env: { ...process.env, VIBEFACE_SIGNATURE_FILE: path.resolve(filename) },
  });
  return JSON.parse(output);
}

export function requireAuthenticode(filename, options = {}) {
  const signature = inspectAuthenticode(filename);
  assert.equal(signature.status, 'Valid', `Authenticode rejected ${path.basename(filename)}: ${signature.status}`);
  assert(signature.timestamped, `${path.basename(filename)} has no trusted timestamp`);
  const subject = options.subject ?? process.env.VIBEFACE_WINDOWS_EXPECTED_SUBJECT;
  const thumbprint = options.thumbprint ?? process.env.VIBEFACE_WINDOWS_EXPECTED_THUMBPRINT;
  if (subject) assert.equal(signature.subject, subject, 'the Authenticode publisher differs from the configured subject');
  if (thumbprint) assert.equal(signature.thumbprint?.toUpperCase(), thumbprint.replace(/\s/g, '').toUpperCase(), 'the Authenticode certificate differs from the configured thumbprint');
  return signature;
}

function signTool() {
  const candidates = [];
  if (process.env.WindowsSdkVerBinPath) candidates.push(path.join(process.env.WindowsSdkVerBinPath, 'x64/signtool.exe'));
  if (process.env.WindowsSdkDir) {
    const bins = path.join(process.env.WindowsSdkDir, 'bin');
    if (existsSync(bins)) for (const version of readdirSync(bins).sort().reverse()) candidates.push(path.join(bins, version, 'x64/signtool.exe'));
  }
  const located = candidates.find(existsSync);
  if (located) return located;
  throw new Error('The existing Windows SDK must provide x64 signtool.exe; initialize its MSVC environment');
}

export async function signNative(files) {
  assert.equal(process.platform, 'win32', 'Windows programs are signed on Windows');
  assert(windowsSigningConfigured(), 'Configure the Windows signing certificate before requesting a signed distribution');
  assert(files.length > 0 && files.every(file => path.isAbsolute(file) && existsSync(file)), 'Name existing absolute program paths');
  const configured = process.env.WIN_CSC_LINK || process.env.CSC_LINK;
  const password = process.env.WIN_CSC_KEY_PASSWORD ?? process.env.CSC_KEY_PASSWORD ?? '';
  const directory = mkdtempSync(path.join(tmpdir(), 'vibeface-signing-'));
  try {
    await ensurePrivateDirectory(directory);
    const certificate = path.join(directory, 'certificate.pfx');
    const source = configured.startsWith('file://') ? fileURLToPath(configured) : configured;
    const bytes = existsSync(source) ? readFileSync(source) : Buffer.from(source, 'base64');
    assert(bytes.length > 0, 'The configured Windows certificate is empty');
    writeFileSync(certificate, bytes, { flag: 'wx', mode: 0o600 });
    for (const file of files) {
      try {
        execFileSync(signTool(), ['sign', '/fd', 'SHA256', '/tr', 'http://timestamp.digicert.com', '/td', 'SHA256', '/f', certificate, '/p', password, file], { encoding: 'utf8', windowsHide: true, stdio: 'pipe' });
      } catch {
        throw new Error(`Authenticode signing failed for ${path.basename(file)}; check the configured certificate and timestamp service`);
      }
      requireAuthenticode(file);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const [command, ...files] = process.argv.slice(2);
    assert(files.length > 0, 'usage: node scripts/windows-signing.mjs sign|verify ABSOLUTE_EXE...');
    if (command === 'sign') await signNative(files.map(file => path.resolve(file)));
    else if (command === 'verify') for (const file of files) requireAuthenticode(file);
    else throw new Error('Expected sign or verify');
    console.log(`Authenticode ${command} passed for ${files.length} programs`);
  } catch (error) {
    console.error(error instanceof Error ? error.message : 'Authenticode operation failed');
    process.exitCode = 1;
  }
}
