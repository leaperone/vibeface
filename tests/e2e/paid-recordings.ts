import assert from 'node:assert/strict';
import { spawn, execFile, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { promisify } from 'node:util';
import { randomUUID, createHash } from 'node:crypto';
import { createWriteStream, constants } from 'node:fs';
import { copyFile, lstat, mkdir, readFile, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { expect } from 'e2e';
import { artifacts, record, string } from './paid-delivery-control';

type Stage = 'web-confirm' | 'android-accept-offline' | 'android-resume-save';
type Exit = { exitCode: number | null; signal: NodeJS.Signals | null };
export type Video = Exit & { file: string; sha256: string; bytes: number; durationSeconds: number;
  stop: 'q' | 'SIGINT'; source: 'ffmpeg-x11grab' | 'adb-screenrecord'; stage: Stage; startedAfterPairing: true };
export type Recording = { stop(): Promise<Video>; cancel(): Promise<void> };
const execute = promisify(execFile);
const directory = resolve(artifacts, 'video-private');
const publicDirectory = resolve(artifacts, 'public');

function bounded<T>(value: Promise<T>, timeout: number, message: string): Promise<T> {
  return new Promise<T>((done, fail) => {
    const timer = setTimeout(() => fail(new Error(message)), timeout);
    value.then(result => { clearTimeout(timer); done(result); }, error => { clearTimeout(timer); fail(error); });
  });
}
export function recordingProcessState(child: ChildProcessWithoutNullStreams) {
  let stopping = false, ended = false, early = false;
  const exit = new Promise<Exit>((done, fail) => {
    child.once('error', () => { ended = true; early = !stopping; fail(new Error('The task recorder could not start')); });
    child.once('exit', (exitCode, signal) => { ended = true; early = !stopping; done({ exitCode, signal }); });
  });
  void exit.catch(() => undefined);
  return { exit, alive: () => !ended, beginStop() {
    assert(!ended && !early, 'The recorder must remain alive until the actual UI segment finishes');
    stopping = true;
  } };
}
async function exportVideo(path: string, stage: Stage, status: Exit, stop: Video['stop'], source: Video['source']): Promise<Video> {
  const metadata = await lstat(path);
  assert(metadata.isFile() && !metadata.isSymbolicLink() && metadata.size > 0 && metadata.size <= 64 * 1024 * 1024);
  const result = await execute('ffprobe', ['-v', 'error', '-select_streams', 'v:0', '-show_entries', 'stream=codec_type:format=duration,format_name', '-of', 'json', path],
    { encoding: 'utf8', timeout: 15000, maxBuffer: 65536 });
  const probe = record(JSON.parse(result.stdout)), format = record(probe.format);
  const durationSeconds = Number(format.duration);
  assert(Number.isFinite(durationSeconds) && durationSeconds > 0 && durationSeconds <= 180 && string(format.format_name).split(',').includes('mp4'));
  assert(Array.isArray(probe.streams) && probe.streams.some((stream: unknown) => record(stream).codec_type === 'video'), 'The bounded MP4 contains an actual video stream');
  const bytes = await readFile(path), file = `${stage}.mp4`;
  await mkdir(publicDirectory, { recursive: true, mode: 0o700 });
  await copyFile(path, resolve(publicDirectory, file), constants.COPYFILE_EXCL);
  return { file, sha256: createHash('sha256').update(bytes).digest('hex'), bytes: bytes.length,
    durationSeconds, ...status, stop, source, stage, startedAfterPairing: true };
}
export async function webRecording(): Promise<Recording> {
  assert(process.platform === 'linux' && /^:[0-9]+(?:\.0)?$/.test(process.env.DISPLAY ?? ''), 'Record only the dedicated task Xvfb display');
  await mkdir(directory, { recursive: true, mode: 0o700 });
  const token = randomUUID(), destination = resolve(directory, `${token}-web-confirm.mp4`);
  const log = createWriteStream(resolve(directory, `${token}-web-confirm.log`), { flags: 'wx', mode: 0o600 });
  const child = spawn('ffmpeg', ['-hide_banner', '-y', '-f', 'x11grab', '-framerate', '10', '-video_size', '1440x1000',
    '-i', process.env.DISPLAY ?? '', '-t', '180', '-c:v', 'libx264', '-preset', 'ultrafast', '-pix_fmt', 'yuv420p',
    '-progress', 'pipe:1', '-stats_period', '0.2', '-nostats', destination], { stdio: ['pipe', 'pipe', 'pipe'] });
  child.stderr.pipe(log);
  const state = recordingProcessState(child);
  child.stdin.on('error', () => undefined);
  const ready = new Promise<void>((done, fail) => {
    createInterface({ input: child.stdout }).on('line', line => { if (/^frame=\s*[1-9][0-9]*\s*$/.test(line)) done(); });
    void state.exit.then(() => fail(new Error('Web recording ended before its first actual frame')), fail);
  });
  const cancel = async () => {
    if (!state.alive()) return;
    state.beginStop();
    child.stdin.write('q\n');
    try { await bounded(state.exit, 15000, 'Web recording did not stop'); }
    catch { if (state.alive()) child.kill('SIGTERM'); }
    log.end();
  };
  try { await bounded(ready, 15000, 'Web recording did not capture its first post-pairing frame'); }
  catch (error) { await cancel(); throw error; }
  const processFact = async () => {
    assert(child.pid);
    const fields = (await readFile(`/proc/${child.pid}/stat`, 'utf8')).trim().replace(/^.*\) /, '').split(/\s+/);
    assert(fields[0] !== 'Z' && fields[0] !== 'X' && fields[19], 'The actual task ffmpeg process is still running');
    return fields[19];
  };
  const started = await processFact();
  return { cancel, async stop() {
    assert(state.alive() && await processFact() === started, 'The original Web recorder covers the whole actual UI segment');
    state.beginStop();
    child.stdin.write('q\n');
    const status = await bounded(state.exit, 15000, 'Web recording did not finalize after the actual UI segment');
    log.end();
    assert(status.exitCode === 0 && status.signal === null, 'The actual Web recorder exits cleanly after its explicit stop');
    return exportVideo(destination, 'web-confirm', status, 'q', 'ffmpeg-x11grab');
  } };
}
export async function androidRecording(stage: Exclude<Stage, 'web-confirm'>, serial: string): Promise<Recording> {
  assert(/^emulator-[0-9]+$/.test(serial));
  await mkdir(directory, { recursive: true, mode: 0o700 });
  const token = randomUUID(), remote = `/sdcard/vdp-paid-${token}.mp4`, destination = resolve(directory, `${token}-${stage}.mp4`);
  const log = createWriteStream(resolve(directory, `${token}-${stage}.log`), { flags: 'wx', mode: 0o600 });
  const script = `screenrecord --bit-rate 2000000 --time-limit 180 ${remote} </dev/null &\nvdp_record_pid=$!\nprintf 'VDP_RECORD_PID:%s\\n' "$vdp_record_pid"\nwait "$vdp_record_pid"\nexit $?`;
  const child = spawn('adb', ['-s', serial, 'shell', script], { stdio: ['pipe', 'pipe', 'pipe'] });
  child.stderr.pipe(log);
  const state = recordingProcessState(child);
  const pid = await bounded(new Promise<string>((done, fail) => {
    createInterface({ input: child.stdout }).on('line', line => { const found = /^VDP_RECORD_PID:([1-9][0-9]{0,9})$/.exec(line.trim()); if (found) done(found[1]); });
    void state.exit.then(() => fail(new Error('Android recording ended before its task PID was observed')), fail);
  }), 15000, 'Android recorder did not identify its own process');
  const adb = async (args: string[]) => (await execute('adb', ['-s', serial, ...args], { encoding: 'utf8', timeout: 15000, maxBuffer: 65536 })).stdout;
  const fact = async () => {
    const fields = (await adb(['shell', 'cat', `/proc/${pid}/stat`])).trim().replace(/^.*\) /, '').split(/\s+/);
    const command = (await adb(['exec-out', 'cat', `/proc/${pid}/cmdline`])).split('\0');
    assert(command[0].endsWith('screenrecord') && command.includes(remote), 'Stop only this exact task screenrecord process');
    assert(fields[0] !== 'Z' && fields[0] !== 'X' && fields[19] && /^[0-9]+$/.test(fields[19]));
    return fields[19];
  };
  let started = '';
  await expect.poll(async () => { started = await fact(); return started.length > 0; }, {
    timeout: 15000, message: 'The exact task screenrecord process starts with its own output path',
  }).toBe(true);
  const interrupt = async () => {
    assert(state.alive() && await fact() === started, 'The original Android recorder remains alive until the actual UI segment ends');
    state.beginStop();
    await adb(['shell', 'kill', '-INT', pid]);
    return bounded(state.exit, 15000, 'Android recording did not finalize after its own SIGINT');
  };
  const cancel = async () => {
    if (state.alive()) await interrupt().catch(() => undefined);
    log.end();
  };
  try {
    await expect.poll(async () => Number((await adb(['shell', 'stat', '-c', '%s', remote])).trim()), {
      timeout: 15000, message: 'The post-pairing Android recorder writes actual video bytes',
    }).toBeGreaterThan(64);
  } catch (error) { await cancel(); throw error; }
  return { cancel, async stop() {
    const status = await interrupt();
    log.end();
    assert((status.signal === null && (status.exitCode === 0 || status.exitCode === 130))
      || (status.exitCode === null && status.signal === 'SIGINT'), 'Preserve the actual Android recorder exit and its explicit interrupt');
    await adb(['pull', remote, destination]);
    return exportVideo(destination, stage, status, 'SIGINT', 'adb-screenrecord');
  } };
}
export async function videoEvidence(role: 'web' | 'android', orderId: string, intentId: string, videos: Video[]): Promise<void> {
  assert(/^[a-f0-9]{64}$/.test(orderId));
  assert(/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(intentId));
  assert(videos.length === (role === 'web' ? 1 : 2));
  await mkdir(publicDirectory, { recursive: true, mode: 0o700 });
  await writeFile(resolve(publicDirectory, `${role}-video.json`), `${JSON.stringify({ schemaVersion: 1, kind: 'vdp-paid-ui-videos',
    status: 'PASS', role, orderId, intentId, videos }, null, 2)}\n`, { mode: 0o600, flag: 'wx' });
}
