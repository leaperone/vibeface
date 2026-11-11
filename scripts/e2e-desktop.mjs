#!/usr/bin/env node
// Run the packaged native executable in an isolated, controlled-fixture profile.
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import * as fs from 'node:fs';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
const repo = fileURLToPath(new URL('../', import.meta.url));

function freshProfilePath(directory) {
  // The real application creates the profile with its protected current-account ACL.
  // Precreating it in Node inherits a Windows DACL that prepare_profile must reject.
  const profile = path.join(directory, `controlled-profile-${randomUUID()}`);
  assert(!fs.existsSync(profile), 'the controlled application profile must be fresh');
  return profile;
}

if (process.argv.length === 3 && process.argv[2] === '--self-test') {
  const artifacts = path.join(repo, '.artifacts');
  fs.mkdirSync(artifacts, { recursive: true });
  const directory = fs.mkdtempSync(path.join(artifacts, 'profile-allocation-'));
  try {
    const first = freshProfilePath(directory);
    const second = freshProfilePath(directory);
    assert.notEqual(first, second);
    assert.equal(path.dirname(first), directory);
    assert.match(path.basename(first), /^controlled-profile-[a-f0-9-]{36}$/);
    assert(!fs.existsSync(first) && !fs.existsSync(second));
    assert.deepEqual(fs.readdirSync(directory), [], 'profile allocation must not create files or directories');
    console.log('native desktop driver self-test passed: unique absent profile paths leave private directory creation to the app');
  } finally { fs.rmSync(directory, { recursive: true, force: true }); }
  process.exit(0);
}

const options = new Map();
const args = process.argv.slice(2);
while (args.length) {
  const flag = args.shift();
  assert(['--app', '--runtime', '--output'].includes(flag) && args.length && !options.has(flag), `invalid argument ${flag}`);
  options.set(flag, path.resolve(args.shift()));
}
for (const flag of ['--app', '--runtime', '--output']) assert(options.has(flag), `${flag} is required`);
const output = options.get('--output');
fs.mkdirSync(output, { recursive: true });
const profile = freshProfilePath(output);
const report = path.join(profile, 'window.json');
const requested = options.get('--app');
const executable = requested.endsWith('.app') ? path.join(requested, 'Contents/MacOS/Vibeface') : requested;
const runtime = options.get('--runtime');
const before = JSON.parse(fs.readFileSync(path.join(runtime, 'BUILD-INFO.json'), 'utf8'));
const child = spawn(executable, ['--controlled-fixture', '--profile-dir', profile, '--runtime-dir', runtime, '--acceptance-report', report],
  { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, RUST_BACKTRACE: '1' } });
const logs = fs.createWriteStream(path.join(output, 'native-window.log'), { flags: 'wx' });
child.stdout.pipe(logs, { end: false });
child.stderr.pipe(logs, { end: false });
let spawnError;
child.on('error', error => { spawnError = error; });
const exit = new Promise(resolve => child.on('close', (code, signal) => resolve({ code, signal })));
for (const signal of ['SIGTERM', 'SIGINT']) process.once(signal, () => {
  if (child.exitCode === null && child.signalCode === null) child.kill();
  process.exitCode = 1;
});
function processRows() {
  if (process.platform === 'win32') {
    const script = 'Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name | ConvertTo-Json -Compress';
    return JSON.parse(execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8' }))
      .map(row => ({ pid: row.ProcessId, parent: row.ParentProcessId, name: row.Name }));
  }
  return execFileSync('ps', ['-axo', 'pid=,ppid=,comm='], { encoding: 'utf8' }).trim().split('\n').map(line => {
    const [, pid, parent, name] = /^\s*(\d+)\s+(\d+)\s+(.+)$/.exec(line);
    return { pid: Number(pid), parent: Number(parent), name };
  });
}
function driveWindow(pid) {
  const screenshot = path.join(output, 'native-window.png');
  let evidence;
  if (process.platform === 'darwin') {
    evidence = JSON.parse(execFileSync('xcrun', ['swift', path.join(repo, 'scripts/native-desktop/window.swift'), String(pid)], { encoding: 'utf8' }));
    execFileSync('osascript', ['-e', `tell application "System Events" to tell first process whose unix id is ${pid}`, '-e', 'set frontmost to true', '-e', 'keystroke "native-gpui-fixture"', '-e', 'key code 36', '-e', 'end tell']);
    execFileSync('screencapture', ['-x', '-l', String(evidence.window_id), screenshot]);
    evidence.keyboard = 'System Events';
  } else if (process.platform === 'win32') {
    evidence = JSON.parse(execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-File', path.join(repo, 'scripts/native-desktop/window.ps1'), '-OwnedPid', String(pid), '-Screenshot', screenshot], { encoding: 'utf8' }));
  } else {
    const window = execFileSync('xdotool', ['search', '--onlyvisible', '--pid', String(pid)], { encoding: 'utf8' }).trim().split('\n')[0];
    assert(/^\d+$/.test(window), 'native PID owns a visible X11 window');
    execFileSync('xdotool', ['windowfocus', '--sync', window]);
    execFileSync('xdotool', ['type', '--clearmodifiers', '--window', window, 'native-gpui-fixture']);
    execFileSync('xdotool', ['key', '--clearmodifiers', '--window', window, 'Return']);
    execFileSync('import', ['-window', window, screenshot]);
    evidence = { pid, window_id: window, keyboard: 'X11 native events' };
  }
  const png = fs.readFileSync(screenshot);
  assert(png.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10])), 'OS produced an actual PNG window capture');
  assert(png.readUInt32BE(16) >= 300 && png.readUInt32BE(20) >= 200, 'captured native window has usable dimensions');
  return { ...evidence, screenshot: path.basename(screenshot), width: png.readUInt32BE(16), height: png.readUInt32BE(20) };
}

const observed = new Map();
try {
  const deadline = Date.now() + 60000;
  while (!fs.existsSync(report) && Date.now() < deadline) {
    assert(!spawnError, `native application failed to spawn: ${spawnError?.message}`);
    assert(child.exitCode === null && child.signalCode === null, 'native application exited before opening its window');
    const rows = processRows();
    const owned = new Set([child.pid]);
    let previous;
    do {
      previous = owned.size;
      for (const row of rows) if (owned.has(row.parent)) owned.add(row.pid);
    } while (owned.size !== previous);
    for (const row of rows.filter(row => owned.has(row.pid))) observed.set(row.pid, row);
    await sleep(100);
  }
  assert(fs.existsSync(report), 'native application did not report a real window');
  const opened = JSON.parse(fs.readFileSync(report, 'utf8'));
  assert.equal(opened.phase, 'opened');
  assert.equal(opened.pid, child.pid);
  const windowEvidence = driveWindow(child.pid);
  fs.writeFileSync(`${report}.continue`, 'OS keyboard input and window capture complete\n', { flag: 'wx' });
  let window;
  const completionDeadline = Date.now() + 10000;
  while (Date.now() < completionDeadline) {
    window = JSON.parse(fs.readFileSync(report, 'utf8'));
    if (window.phase === 'completed') break;
    await sleep(50);
  }
  assert.equal(window.phase, 'completed', 'actual GPUI input state and action were acknowledged');
  assert.equal(window.input_text, 'native-gpui-fixture');
  assert.equal(window.focused, true);
  assert(window.action_count >= 1, 'the native Enter action ran');
  assert.equal(window.runtime_ui, 'gpui');
  assert.equal(window.window_opened, true);
  assert.equal(window.pid, child.pid, 'report belongs to the driver-owned native process');
  assert.equal(window.node_started, false, 'opening the control-only app must not start Node');
  assert(![...observed.values()].some(row => /electron|chromium|chrome|webview|vibeface-core/i.test(row.name)), 'control-only native application started an unexpected process');
  const ended = await Promise.race([exit, sleep(10000, undefined, { ref: false }).then(() => { throw new Error('native acceptance mode did not quit gracefully'); })]);
  assert.deepEqual(ended, { code: 0, signal: null });
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(runtime, 'BUILD-INFO.json'), 'utf8')), before);
  assert(!fs.existsSync(path.join(profile, 'node/config.json')), 'window acceptance created Node configuration');
  fs.writeFileSync(path.join(output, 'native-window.json'), `${JSON.stringify({ window, windowEvidence, processes: [...observed.values()], runtime: before, exit: ended }, null, 2)}\n`);
  console.log('Packaged native GPUI window received native keyboard input, ran Enter, was captured and exited; no browser runtime or automatic Node process');
} finally {
  if (child.exitCode === null && child.signalCode === null) child.kill();
  logs.end();
}
