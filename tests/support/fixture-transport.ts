import type { ChildProcess } from 'node:child_process';
import type { Readable, Writable } from 'node:stream';
import { TextDecoder } from 'node:util';
import { parseFixtureRequest, parseFixtureResult, type FixtureRequest, type ResultByOp } from './fixture-contract.ts';

type FixtureObject = Record<string, unknown>;
type FixtureChild = ChildProcess & { stdin: Writable; stdout: Readable };
type ChildClose = { code: number | null; signal: NodeJS.Signals | null };
type Waiter = {
  resolve(value: FixtureObject): void;
  reject(error: Error): void;
  timer: ReturnType<typeof setTimeout>;
  decode?: (value: FixtureObject) => FixtureObject;
};
type State =
  | { kind: 'starting'; waiter: Waiter }
  | { kind: 'idle'; nextId: number }
  | { kind: 'pending'; id: number; waiter: Waiter }
  | { kind: 'terminal'; error: Error };

const maxResponseBytes = 64 * 1024;

export class FixtureOperationError extends Error {}

function isFixtureObject(value: unknown): value is FixtureObject {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function hasKeys(value: FixtureObject, keys: string[]): boolean {
  return Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

function describeFailure(category: unknown, frames: unknown): string | undefined {
  if (typeof category !== 'string' || !Array.isArray(frames)) return undefined;
  const entries: unknown[] = frames;
  if (!entries.every(frame => typeof frame === 'string')) return undefined;
  const name = /^[A-Za-z_]{1,50}$/.test(category) ? category : 'Error';
  const validFrames = entries.filter(frame => /^verify-node\.py:[1-9][0-9]{0,4}:[A-Za-z_][A-Za-z0-9_]{0,70}$/.test(frame)).slice(-4);
  return `${name}${validFrames.length ? ` at ${validFrames.join(', ')}` : ''}`;
}

export function createFixtureTransport(child: FixtureChild, {
  requestTimeoutMs,
  shutdownTimeoutsMs: [eofMs, termMs, killMs],
}: {
  requestTimeoutMs: number;
  shutdownTimeoutsMs: [eof: number, term: number, kill: number];
}) {
  let state: State;
  let stopping: Promise<ChildClose> | undefined;
  let buffered: Buffer = Buffer.alloc(0);
  const decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

  function fail(reason: string): Error {
    const current = state;
    if (current.kind === 'terminal') return current.error;
    const error = new Error(current.kind === 'pending' ? `${reason}. Operation outcome is unknown` : reason);
    state = { kind: 'terminal', error };
    buffered = Buffer.alloc(0);
    if (current.kind === 'starting' || current.kind === 'pending') {
      clearTimeout(current.waiter.timer);
      current.waiter.reject(error);
    }
    return error;
  }

  const ready = new Promise<FixtureObject>((resolve, reject) => {
    const timer = setTimeout(() => fail('Fixture startup timed out'), requestTimeoutMs);
    state = { kind: 'starting', waiter: { resolve, reject, timer } };
  });
  // Cleanup can start before a caller awaits startup.
  void ready.catch(() => {});

  function accept(value: unknown) {
    const current = state;
    if (current.kind === 'starting' && isFixtureObject(value)) {
      if (hasKeys(value, ['ready']) && isFixtureObject(value.ready)) {
        state = { kind: 'idle', nextId: 1 };
        clearTimeout(current.waiter.timer);
        current.waiter.resolve(value.ready);
        return;
      }
      if (hasKeys(value, ['startup_error']) && isFixtureObject(value.startup_error) &&
          hasKeys(value.startup_error, ['error', 'frames'])) {
        const details = describeFailure(value.startup_error.error, value.startup_error.frames);
        if (details !== undefined) {
          fail(`Fixture startup failure ${details}`);
          return;
        }
      }
    } else if (current.kind === 'pending' && isFixtureObject(value)) {
      const { id, ok } = value;
      if (typeof id === 'number' && Number.isSafeInteger(id) && id > 0 && id !== current.id) {
        fail('Fixture response ID does not match the pending operation');
        return;
      }
      if (id === current.id) {
        if (ok === true && hasKeys(value, ['id', 'ok', 'result']) && isFixtureObject(value.result)) {
          try {
            const result = current.waiter.decode ? current.waiter.decode(value.result) : value.result;
            state = { kind: 'idle', nextId: current.id + 1 };
            clearTimeout(current.waiter.timer);
            current.waiter.resolve(result);
            return;
          } catch {
            fail('Fixture returned an invalid operation result');
            return;
          }
        }
        if (ok === false && hasKeys(value, ['id', 'ok', 'error', 'frames'])) {
          const details = describeFailure(value.error, value.frames);
          if (details !== undefined) {
            state = { kind: 'idle', nextId: current.id + 1 };
            clearTimeout(current.waiter.timer);
            current.waiter.reject(new FixtureOperationError(`Fixture ${details}`));
            return;
          }
        }
      }
    }
    fail('Fixture returned an unexpected or invalid response');
  }

  child.stdout.on('data', (chunk: Buffer) => {
    let offset = 0;
    while (offset < chunk.length && state.kind !== 'terminal') {
      const newline = chunk.indexOf(10, offset);
      const part = chunk.subarray(offset, newline === -1 ? chunk.length : newline);
      if (buffered.length + part.length > maxResponseBytes) {
        fail('Fixture response exceeds 64 KiB');
        return;
      }
      const line = buffered.length === 0 ? part : Buffer.concat([buffered, part]);
      if (newline === -1) {
        buffered = line;
        return;
      }
      buffered = Buffer.alloc(0);
      try {
        const value: unknown = JSON.parse(decoder.decode(line));
        accept(value);
      } catch {
        fail('Fixture returned an invalid JSON line');
      }
      offset = newline + 1;
    }
  });
  const stdoutClosed = () => fail(buffered.length ? 'Fixture stdout ended with an incomplete JSON line' : 'Fixture stdout closed');
  child.stdout.once('end', stdoutClosed);
  child.stdout.once('close', stdoutClosed);
  child.stdout.on('error', () => fail('Fixture stdout failed'));
  child.stdin.on('error', () => fail('Fixture stdin write failed'));
  child.on('error', () => fail('Fixture child process failed'));
  // The close event follows stdio drainage and also occurs after spawn errors.
  const closed = new Promise<ChildClose>(resolve => {
    child.once('close', (code, signal) => {
      fail('Fixture child process closed');
      resolve({ code, signal });
    });
  });

  async function call(request: FixtureObject, decode?: (value: FixtureObject) => FixtureObject): Promise<FixtureObject> {
    const current = state;
    if (current.kind === 'terminal') throw current.error;
    if (current.kind !== 'idle') throw new Error('Fixture is unavailable or busy');
    const id = current.nextId;
    if (!Number.isSafeInteger(id)) throw fail('Fixture request IDs are exhausted');
    const line = `${JSON.stringify({ id, request })}\n`;
    if (state !== current) {
      if (state.kind === 'terminal') throw state.error;
      throw new Error('Fixture is unavailable or busy');
    }
    return new Promise<FixtureObject>((resolve, reject) => {
      const timer = setTimeout(() => fail('Fixture operation timed out'), requestTimeoutMs);
      state = { kind: 'pending', id, waiter: { resolve, reject, timer, decode } };
      try {
        child.stdin.write(line, error => { if (error) fail('Fixture stdin write failed'); });
      } catch {
        fail('Fixture stdin write failed');
      }
    });
  }

  function waitForClose(timeoutMs: number): Promise<ChildClose | undefined> {
    return new Promise(resolve => {
      const timer = setTimeout(resolve, timeoutMs);
      void closed.then(status => { clearTimeout(timer); resolve(status); });
    });
  }

  async function close(): Promise<ChildClose> {
    fail('Fixture stopped');
    child.stdin.end();
    const graceful = await waitForClose(eofMs);
    if (graceful) return graceful;
    child.kill('SIGTERM');
    const terminated = await waitForClose(termMs);
    if (terminated) return terminated;
    child.kill('SIGKILL');
    const killed = await waitForClose(killMs);
    if (killed) return killed;
    child.stdin.destroy();
    child.stdout.destroy();
    throw new Error('Fixture cleanup timed out waiting for child close');
  }

  async function callTyped<O extends FixtureRequest['op']>(request: Extract<FixtureRequest, { op: O }>): Promise<ResultByOp[O]> {
    const parsed = parseFixtureRequest(request);
    return call(parsed, value => parseFixtureResult(parsed, value) as FixtureObject) as Promise<ResultByOp[O]>;
  }

  return { ready, call, callTyped, fail, closed, stop: () => stopping ??= close() };
}
