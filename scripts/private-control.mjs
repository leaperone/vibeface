#!/usr/bin/env node
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { chmod, lstat, mkdir, mkdtemp, readFile, rename, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const run = promisify(execFile);
let cachedWindowsSid;

const WINDOWS_SET_PRIVATE_ACL = `
param([string]$Path, [string]$Kind)
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
if ($Kind -eq 'directory') {
  if (-not (Test-Path -LiteralPath $Path -PathType Container)) { throw 'missing-directory' }
  $acl = New-Object System.Security.AccessControl.DirectorySecurity
  $acl.SetAccessRuleProtection($true, $false)
  $acl.SetOwner($sid)
  $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
  $acl.SetAccessRule($rule)
  Set-Acl -LiteralPath $Path -AclObject $acl
} elseif ($Kind -eq 'file') {
  if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw 'missing-file' }
  $acl = New-Object System.Security.AccessControl.FileSecurity
  $acl.SetAccessRuleProtection($true, $false)
  $acl.SetOwner($sid)
  $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', 'Allow')
  $acl.SetAccessRule($rule)
  Set-Acl -LiteralPath $Path -AclObject $acl
} else {
  throw 'invalid-kind'
}
@{ sid = $sid.Value } | ConvertTo-Json -Compress
`;

const WINDOWS_VERIFY_PRIVATE_ACL = `
param([string]$Path, [string]$CurrentSid)
$item = Get-Item -LiteralPath $Path -Force
if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'reparse-point' }
$acl = Get-Acl -LiteralPath $Path
function Convert-Sid([object]$Identity) {
  if ($Identity -is [System.Security.Principal.SecurityIdentifier]) { return $Identity.Value }
  if ($Identity -is [string]) {
    try { return (New-Object System.Security.Principal.SecurityIdentifier($Identity)).Value } catch {}
    return (New-Object System.Security.Principal.NTAccount($Identity)).Translate([System.Security.Principal.SecurityIdentifier]).Value
  }
  return $Identity.Translate([System.Security.Principal.SecurityIdentifier]).Value
}
$ownerSid = Convert-Sid $acl.Owner
$trusted = @($CurrentSid, 'S-1-5-18', 'S-1-5-32-544')
$allowSids = @()
$badAllowSids = @()
$inherited = $false
foreach ($rule in $acl.Access) {
  if ($rule.IsInherited) { $inherited = $true }
  if ($rule.AccessControlType -ne [System.Security.AccessControl.AccessControlType]::Allow) { continue }
  $sid = Convert-Sid $rule.IdentityReference
  $allowSids += $sid
  if ($trusted -notcontains $sid) { $badAllowSids += $sid }
}
@{ ownerSid = $ownerSid; allowSids = $allowSids; badAllowSids = $badAllowSids; inherited = $inherited; protected = $acl.AreAccessRulesProtected } | ConvertTo-Json -Compress
`;

const WINDOWS_REPARSE_FLAG = `
param([string]$Path)
$item = Get-Item -LiteralPath $Path -Force
@{ reparse = [bool](($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) } | ConvertTo-Json -Compress
`;

function inside(base, candidate) {
  const suffix = relative(base, candidate);
  return suffix === '' || (!suffix.startsWith('..') && !suffix.startsWith(`..${sep}`) && !isAbsolute(suffix));
}

export function resolveBoundedPath(root, candidate) {
  const base = resolve(root);
  const value = resolve(base, candidate);
  assert(inside(base, value), 'Control path escapes its bounded directory');
  return value;
}

async function powershellJson(script, ...args) {
  const invocation = '$ErrorActionPreference = "Stop"\n$values = @(ConvertFrom-Json -InputObject $env:VIBEFACE_PRIVATE_CONTROL_ARGS)\n& {\n' + script + '\n} @values';
  const result = await run('powershell.exe', ['-NoProfile', '-NonInteractive', '-EncodedCommand', Buffer.from(invocation, 'utf16le').toString('base64')], {
    windowsHide: true,
    maxBuffer: 8 * 1024 * 1024,
    env: { ...process.env, VIBEFACE_PRIVATE_CONTROL_ARGS: JSON.stringify(args) },
  });
  const text = result.stdout.trim();
  assert(text, 'PowerShell returned no JSON');
  return JSON.parse(text);
}

async function ensureNoReparsePoint(pathname) {
  if (process.platform !== 'win32') return;
  const data = await powershellJson(WINDOWS_REPARSE_FLAG, pathname);
  assert(data && data.reparse === false, 'Control path must not use reparse points');
}

async function requireExistingPath(pathname, kind) {
  const metadata = await lstat(pathname);
  assert(!metadata.isSymbolicLink(), 'Control path must not use symbolic links');
  assert(kind === 'file' ? metadata.isFile() : metadata.isDirectory(), 'Unexpected control path kind');
  await ensureNoReparsePoint(pathname);
  return metadata;
}

async function currentWindowsSid() {
  if (process.platform !== 'win32') return null;
  if (cachedWindowsSid) return cachedWindowsSid;
  const data = await powershellJson('[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value | ConvertTo-Json -Compress');
  assert(typeof data === 'string' && data.startsWith('S-1-5-'), 'Missing current Windows SID');
  cachedWindowsSid = data;
  return cachedWindowsSid;
}

async function setPrivateAcl(pathname, kind) {
  if (process.platform !== 'win32') return;
  const data = await powershellJson(WINDOWS_SET_PRIVATE_ACL, pathname, kind);
  const sid = await currentWindowsSid();
  assert(data && data.sid === sid, 'Control ACL was not assigned to the current Windows user');
}

async function verifyWindowsAcl(pathname) {
  const sid = await currentWindowsSid();
  const data = await powershellJson(WINDOWS_VERIFY_PRIVATE_ACL, pathname, sid);
  assert(data && data.ownerSid === sid, 'Control file owner must be the current Windows user SID');
  assert(data.inherited === false && data.protected === true, 'Control ACL inheritance is disabled');
  assert(Array.isArray(data.allowSids) && data.allowSids.includes(sid), 'Control ACL must allow the current SID');
  assert(Array.isArray(data.badAllowSids) && data.badAllowSids.length === 0, 'Control ACL has unexpected allow entries');
}

export async function ensurePrivateDirectory(directory, options = {}) {
  const base = options.root ? resolve(options.root) : undefined;
  const target = base ? resolveBoundedPath(base, directory) : resolve(directory);
  await mkdir(target, { recursive: true, mode: 0o700 });
  await requireExistingPath(target, 'directory');
  if (process.platform === 'win32') {
    await setPrivateAcl(target, 'directory');
    await verifyWindowsAcl(target);
  } else {
    await chmod(target, 0o700);
    const metadata = await requireExistingPath(target, 'directory');
    assert((metadata.mode & 0o077) === 0, 'Control directory must be private');
  }
  return target;
}

export async function verifyPrivateControlFile(filename, options = {}) {
  const base = options.root ? resolve(options.root) : undefined;
  const target = base ? resolveBoundedPath(base, filename) : resolve(filename);
  const metadata = await requireExistingPath(target, 'file');
  const maxBytes = options.maxBytes ?? 4096;
  assert(Number.isSafeInteger(maxBytes) && maxBytes > 0, 'maxBytes must be a positive integer');
  assert(metadata.size > 0 && metadata.size <= maxBytes, 'Control file size is out of bounds');
  if (process.platform === 'win32') {
    await verifyWindowsAcl(target);
  } else {
    assert((metadata.mode & 0o077) === 0, 'Control file must stay private');
  }
  return target;
}

export async function readPrivateControlJson(filename, options = {}) {
  const target = await verifyPrivateControlFile(filename, options);
  return JSON.parse(await readFile(target, 'utf8'));
}

export async function writePrivateControlFile(filename, content, options = {}) {
  assert(typeof content === 'string' && content.length > 0, 'Control file content must be a non-empty string');
  const base = options.root ? resolve(options.root) : undefined;
  const target = base ? resolveBoundedPath(base, filename) : resolve(filename);
  const parent = await ensurePrivateDirectory(dirname(target), base ? { root: base } : {});
  const temporary = resolveBoundedPath(parent, `${process.pid}.${Date.now()}.${Math.random().toString(16).slice(2)}.tmp`);
  await writeFile(temporary, content, { mode: 0o600, flag: 'wx' });
  try {
    await requireExistingPath(temporary, 'file');
    if (process.platform === 'win32') {
      await setPrivateAcl(temporary, 'file');
      await verifyWindowsAcl(temporary);
    }
    await rename(temporary, target);
  } finally {
    await rm(temporary, { force: true });
  }
  await verifyPrivateControlFile(target, base ? { ...options, root: base } : options);
  return target;
}

async function selfTest() {
  const scratch = await mkdtemp(join(tmpdir(), 'vibeface-private-control-'));
  try {
    const root = join(scratch, 'root');
    await ensurePrivateDirectory(root);
    const controlRoot = await ensurePrivateDirectory('control', { root });
    const file = resolveBoundedPath(controlRoot, '9001.json');
    const fixture = { url: 'http://127.0.0.1:9001', token: 'a'.repeat(64) };
    await writePrivateControlFile(file, JSON.stringify(fixture), { root: controlRoot, maxBytes: 2048 });
    const value = await readPrivateControlJson(file, { root: controlRoot, maxBytes: 2048 });
    assert.deepEqual(value, fixture, 'Control file round-trip keeps exact JSON');
    assert.throws(() => resolveBoundedPath(controlRoot, '../escape.json'), /escapes its bounded directory/);
    console.log('private control helper self-test passed');
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
}

const entry = fileURLToPath(import.meta.url);
if (process.argv[1] && resolve(process.argv[1]) === entry) {
  if (process.argv.length === 3 && process.argv[2] === '--self-test') {
    await selfTest();
  } else {
    console.error('usage: node scripts/private-control.mjs --self-test');
    process.exitCode = 1;
  }
}
