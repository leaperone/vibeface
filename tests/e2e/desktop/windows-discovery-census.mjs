/** Bounded read-only Windows discovery census for desktop E2E. Never returns raw argv, env, SID, or paths. */
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { basename } from 'node:path';
import { promisify } from 'node:util';

const run = promisify(execFile);
const SCHEMA = 1;
const WARNINGS = new Set(['candidate_limit_reached', 'current_user_home_unavailable',
  'user_service_metadata_unavailable', 'running_process_metadata_unavailable',
  'candidate_not_current_user_owned', 'candidate_ownership_unavailable',
  'candidate_unavailable_or_identity_mismatch', 'candidate_inspection_incomplete']);
const TIMEOUT_MS = 15000;
const OUTPUT_LIMIT = 8192;

const PROCESS_SCRIPT = `
$ErrorActionPreference = 'Stop'
$censusProcessId = [int]$env:VIBEFACE_CENSUS_PID
$expected = $env:VIBEFACE_CENSUS_EXE
$p = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId=$censusProcessId"
if (-not $p) {
  @{ exe_matches = $false; owner_query_success = $false; readable_commandline = $false; has_data_dir = $false; path_candidate_count = 0 } | ConvertTo-Json -Compress
  exit 0
}
$exeMatches = $false
if ($p.Name -and $expected) {
  $exeMatches = [string]::Equals($p.Name, $expected, [StringComparison]::OrdinalIgnoreCase)
}
$ownerOk = $false
$ownerMatches = $false
try {
  $owner = Invoke-CimMethod -InputObject $p -MethodName GetOwnerSid
  if ($owner.ReturnValue -eq 0 -and $owner.Sid) {
    $ownerOk = $true
    $tokenSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $ownerMatches = [string]::Equals($owner.Sid, $tokenSid, [StringComparison]::Ordinal)
  }
} catch {}
$cmd = [string]$p.CommandLine
$readable = ($cmd.Length -gt 0 -and $cmd.Length -le 32768)
$dataDir = $readable -and ($cmd -match '(?:^|\\s)--data-dir(?:\\s|=)')
$candidates = 0
if ($readable) {
  $matches = [regex]::Matches($cmd, '(?:^|\\s)--data-dir(?:\\s|=)([^\\s"]+|"(?:[^"\\\\]|\\\\.)*")')
  $candidates = @($matches).Count
}
@{ exe_matches = [bool]$exeMatches; owner_query_success = [bool]$ownerOk; owner_matches_current_user = [bool]$ownerMatches;
   readable_commandline = [bool]$readable; has_data_dir = [bool]$dataDir; path_candidate_count = [int]$candidates } | ConvertTo-Json -Compress
`.trim();

function encodeCommand(script) {
  return Buffer.from(script, 'utf16le').toString('base64');
}

function parseJson(stdout) {
  const text = stdout.trim();
  assert(text.length > 0 && text.length <= OUTPUT_LIMIT, 'census output out of bounds');
  return JSON.parse(text);
}

function publicProcessRow(row) {
  const fields = typeof row === 'object' && row ? row : {};
  const count = fields.path_candidate_count;
  return {
    exe_matches: fields.exe_matches === true,
    owner_query_success: fields.owner_query_success === true,
    owner_matches_current_user: fields.owner_matches_current_user === true,
    readable_commandline: fields.readable_commandline === true,
    has_data_dir: fields.has_data_dir === true,
    path_candidate_count: Number.isInteger(count) && count >= 0 && count <= 8 ? count : 0,
  };
}

async function probeProcess(pid, expectedExe) {
  const encoded = encodeCommand(PROCESS_SCRIPT);
  const env = { ...process.env, VIBEFACE_CENSUS_PID: String(pid), VIBEFACE_CENSUS_EXE: basename(expectedExe) };
  const { stdout } = await run('powershell.exe', ['-NoProfile', '-NonInteractive', '-EncodedCommand', encoded], {
    timeout: TIMEOUT_MS, maxBuffer: OUTPUT_LIMIT, windowsHide: true, env,
  });
  return parseJson(stdout);
}

export function sanitizeDiscover(raw) {
  assert(raw && typeof raw === 'object' && raw.ok, 'discover control failed');
  const result = raw.result;
  assert(result && typeof result === 'object', 'discover result missing');
  const nodes = Array.isArray(result.nodes) ? result.nodes : [];
  const warnings = Array.isArray(result.warnings) ? result.warnings : [];
  const codes = [...new Set(warnings.filter(code => WARNINGS.has(code)))].sort();
  const running = nodes.filter(node => node?.status === 'running').length;
  const stopped = nodes.filter(node => node?.status === 'stopped').length;
  const pathCandidates = nodes.filter(node => Array.isArray(node?.sources) && node.sources.includes('running_process')).length;
  assert(nodes.length <= 64 && codes.length <= 128);
  return {
    ok: true,
    total_count: nodes.length,
    running_count: running,
    stopped_count: stopped,
    path_candidate_count: pathCandidates,
    warning_codes: codes,
  };
}

export async function censusCoreProcess(pid, expectedCorePath, inMain) {
  assert(Number.isInteger(pid) && pid > 0);
  const driver = await probeProcess(pid, expectedCorePath);
  const main = await inMain(`(async () => {
    const { execFile } = await import('node:child_process');
    const { promisify } = await import('node:util');
    const run = promisify(execFile);
    const encoded = ${JSON.stringify(encodeCommand(PROCESS_SCRIPT))};
    const env = { ...process.env, VIBEFACE_CENSUS_PID: ${JSON.stringify(String(pid))},
      VIBEFACE_CENSUS_EXE: ${JSON.stringify(basename(expectedCorePath))} };
    const { stdout } = await run('powershell.exe', ['-NoProfile', '-NonInteractive', '-EncodedCommand', encoded],
      { timeout: ${TIMEOUT_MS}, maxBuffer: ${OUTPUT_LIMIT}, windowsHide: true, env });
    return stdout.trim();
  })()`);
  assert.equal(typeof main, 'string');
  const mainRow = parseJson(main);
  return {
    driver: publicProcessRow(driver),
    main: publicProcessRow(mainRow),
  };
}

export function publicCensusProjection(value) {
  assert(value?.schema === SCHEMA, 'unsupported census');
  if (value.platform !== 'win32') {
    return { schema: SCHEMA, platform: 'other', skipped: true };
  }
  const discover = value.discover;
  assert(discover && typeof discover === 'object');
  const cores = Array.isArray(value.cores) ? value.cores : [];
  assert(cores.length <= 64);
  const discoveryPair = pair => ({ driver: publicDiscoverStats(pair?.driver), main: publicDiscoverStats(pair?.main) });
  return {
    schema: SCHEMA,
    platform: 'win32',
    core_count: cores.length,
    cores: cores.map(entry => ({
      driver: publicProcessRow(entry?.driver),
      main: publicProcessRow(entry?.main),
    })),
    automatic: discoveryPair(value.automatic),
    remembered: discoveryPair(discover),
  };
}

function publicDiscoverStats(value) {
  if (value?.ok !== true) return { ok: false };
  const keys = ['total_count', 'running_count', 'stopped_count', 'path_candidate_count'];
  if (!keys.every(key => Number.isInteger(value[key]) && value[key] >= 0 && value[key] <= 64)) return { ok: false };
  return { ok: true, total_count: value.total_count, running_count: value.running_count,
    stopped_count: value.stopped_count, path_candidate_count: value.path_candidate_count,
    warning_codes: Array.isArray(value.warning_codes) ? [...new Set(value.warning_codes.filter(code => WARNINGS.has(code)))].sort() : [] };
}

export async function discoverFromDriver(core, profileDir, remembered) {
  const execution = run(core, ['--control'], {
    timeout: TIMEOUT_MS, maxBuffer: 1048576, killSignal: 'SIGKILL', windowsHide: true,
  });
  execution.child.stdin.on('error', () => {});
  execution.child.stdin.end(JSON.stringify({ command: 'discover_local_nodes',
    app_profile_dir: profileDir, remembered_data_dirs: remembered }) + String.fromCharCode(10));
  try {
    const { stdout } = await execution;
    return sanitizeDiscover(JSON.parse(stdout.split(String.fromCharCode(10), 1)[0]));
  } catch { return { ok: false }; }
}

export async function discoverFromMain(profileDir, remembered, inMain) {
  const request = { command: 'discover_local_nodes', app_profile_dir: profileDir, remembered_data_dirs: remembered };
  return inMain(`(async () => {
    const { execFile } = await import('node:child_process');
    const { join } = await import('node:path');
    const assert = (await import('node:assert/strict')).default;
    const WARNINGS = new Set(${JSON.stringify([...WARNINGS])});
    const sanitizeDiscover = ${sanitizeDiscover.toString()};
    const core = join(process.resourcesPath, 'runtime', 'vibeface-core.exe');
    const raw = await new Promise(resolve => {
      const child = execFile(core, ['--control'], {
        timeout: ${TIMEOUT_MS}, maxBuffer: 1048576, killSignal: 'SIGKILL', windowsHide: true,
      }, (error, stdout) => {
        if (error) return resolve({ ok: false });
        try { resolve(JSON.parse(stdout.split(String.fromCharCode(10), 1)[0])); }
        catch { resolve({ ok: false }); }
      });
      child.stdin.on('error', () => {});
      child.stdin.end(JSON.stringify(${JSON.stringify(request)}) + String.fromCharCode(10));
    });
    if (raw?.ok !== true) return { ok: false };
    return sanitizeDiscover(raw);
  })()`);
}
