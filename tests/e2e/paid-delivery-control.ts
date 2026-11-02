import assert from 'node:assert/strict';
import { copyFile, lstat, readFile, mkdir, writeFile } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import { createHash } from 'node:crypto';
import { expect } from 'e2e';

export const root = resolve(import.meta.dirname, '../..');
export const artifacts = resolve(root, '.artifacts/vdp/live-ui');
export const roles = ['web', 'android', 'provider'] as const;
export type UiRole = 'web' | 'android';
export const inspectionKinds = ['source-partial', 'source-unknown', 'receiver-key-before', 'receiver-key-after'] as const;
export type InspectionKind = typeof inspectionKinds[number];
export type Inspection = {
  id: string; kind: InspectionKind; role: UiRole; nodePeer: string; orderId: string | null; intentId: string | null;
  keyId: string | null; keySerial: number | null; expectedPhase: string | null; expectedPaymentState: string | null;
  earned: string | null; withdrawn: string | null; refunded: string | null; evidence: string | null;
  windows: { state: string; decryptionProven: boolean }[]; screenshot: string; timeoutMs: number;
};
export type Pairing = { peer: string; code: string; approved: boolean };
export type PaidOrder = {
  order_id: string; intent_id: string; name: string | null; bytes: number; sha256: string | null;
  mode: string; acceptance: string; phase: string; evidence: string;
  payment: { total: string; earned: string; withdrawn: string; refunded: string; gas_cap: string;
    chain_id: string | null; asset: string | null; provider_wallet: string | null };
};
export type PaidFacts = {
  nodes: Record<typeof roles[number], { url: string; peer: string }>;
  sourceFile: string; name: string; bytes: number; sha256: string; serviceCap: string; gasCap: string;
  source: PaidOrder | null; receiver: PaidOrder | null; provider: PaidOrder | null;
  settled: boolean; paired: Record<UiRole, boolean>; events: string[]; failure: string | null;
  stage: 'bootstrap' | 'inspections' | 'ordinary' | 'complete'; inspection: Inspection | null;
};

export function record(value: unknown): Record<string, unknown> {
  const valid = (input: unknown): input is Record<string, unknown> => input !== null && typeof input === 'object' && !Array.isArray(input);
  assert(valid(value), 'Expected one bounded UI controller record');
  return value;
}
export function string(value: unknown): string { assert(typeof value === 'string' && value.length > 0); return value; }
function nullable(value: unknown): string | null { return value === null ? null : string(value); }
function integer(value: unknown): number { assert(typeof value === 'number' && Number.isSafeInteger(value) && value >= 0); return value; }
function boolean(value: unknown): boolean { assert(typeof value === 'boolean'); return value; }
function hex(value: unknown): string { const text = string(value); assert(/^[a-f0-9]{64}$/.test(text)); return text; }
function order(value: unknown): PaidOrder | null {
  if (value === null) return null;
  const row = record(value), payment = record(row.payment);
  return { order_id: hex(row.order_id), intent_id: string(row.intent_id), name: nullable(row.name), bytes: integer(row.bytes), sha256: nullable(row.sha256),
    mode: string(row.mode), acceptance: string(row.acceptance), phase: string(row.phase), evidence: string(row.evidence),
    payment: { total: string(payment.total), earned: string(payment.earned), withdrawn: string(payment.withdrawn), refunded: string(payment.refunded),
      gas_cap: string(payment.gas_cap), chain_id: nullable(payment.chain_id), asset: nullable(payment.asset), provider_wallet: nullable(payment.provider_wallet) } };
}
function loopback(value: unknown): string {
  const text = string(value), url = new URL(text);
  assert(url.protocol === 'http:' && url.hostname === '127.0.0.1' && url.port && url.pathname === '/' && !url.username && !url.password && !url.search && !url.hash,
    'UI acceptance uses a literal loopback HTTP origin');
  return url.origin;
}
export function insideArtifacts(value: string): string {
  const path = resolve(value), suffix = relative(resolve(root, '.artifacts'), path);
  assert(suffix && !suffix.startsWith('..') && !suffix.startsWith('/'), 'UI acceptance files remain in this task worktree artifacts');
  return path;
}
export function deadline(): number {
  const value = Number(process.env.VIBEFACE_DELIVERY_UI_TOTAL_TIMEOUT_MS);
  assert(Number.isSafeInteger(value) && value >= 30000 && value <= 86400000, 'UI acceptance requires an explicit bounded runtime deadline');
  return value;
}
export function ordinaryDeadline(): number {
  const value = Number(process.env.VIBEFACE_DELIVERY_UI_TIMEOUT_MS);
  assert(Number.isSafeInteger(value) && value >= 30000 && value <= 3600000, 'Ordinary UI retains its existing independent deadline');
  return value;
}
function member<const T extends string>(value: unknown, values: readonly T[]): T {
  const result = values.find(item => item === value);
  assert(result !== undefined);
  return result;
}
function inspection(value: unknown): Inspection | null {
  if (value === null) return null;
  const row = record(value), kind = member(row.kind, inspectionKinds), role = member(row.role, ['web', 'android']);
  const files = { 'source-partial': 'web-partial-refund.png', 'source-unknown': 'web-unknown-transaction.png',
    'receiver-key-before': 'android-key-before.png', 'receiver-key-after': 'android-key-rotated.png' };
  assert(row.screenshot === files[kind] && row.timeoutMs === 120000 && role === (kind.startsWith('source-') ? 'web' : 'android'));
  assert(Array.isArray(row.windows) && row.windows.length <= 2);
  const orderId = row.orderId === null ? null : hex(row.orderId), intentId = nullable(row.intentId);
  assert((kind === 'receiver-key-before') === (orderId === null && intentId === null));
  return { id: string(row.id), kind, role, nodePeer: hex(row.nodePeer), orderId, intentId,
    keyId: row.keyId === null ? null : hex(row.keyId), keySerial: row.keySerial === null ? null : integer(row.keySerial),
    expectedPhase: nullable(row.expectedPhase), expectedPaymentState: nullable(row.expectedPaymentState),
    earned: nullable(row.earned), withdrawn: nullable(row.withdrawn), refunded: nullable(row.refunded), evidence: nullable(row.evidence),
    windows: row.windows.map(value => { const window = record(value); return { state: string(window.state), decryptionProven: boolean(window.decryptionProven) }; }),
    screenshot: string(row.screenshot), timeoutMs: integer(row.timeoutMs) };
}
export async function request(path: string, body?: Record<string, unknown>): Promise<unknown> {
  const filename = process.env.VIBEFACE_DELIVERY_UI_CONTROL;
  assert(filename, 'An active bounded delivery runner must supply the private UI controller');
  const location = insideArtifacts(filename), metadata = await lstat(location);
  assert(metadata.isFile() && !metadata.isSymbolicLink() && metadata.size <= 2048 && (metadata.mode & 0o077) === 0,
    'Read only this task private regular controller descriptor');
  assert(metadata.uid === process.getuid?.(), 'The controller explicitly assigns its private descriptor to this host user');
  const controller = record(JSON.parse(await readFile(location, 'utf8')));
  assert(Object.keys(controller).sort().join(',') === 'schemaVersion,token,url' && controller.schemaVersion === 1);
  const token = hex(controller.token), origin = loopback(controller.url);
  assert(['/facts', '/invitation/web', '/invitation/android', '/pairing', '/approve', '/event', '/abort'].includes(path));
  const response = await fetch(`${origin}${path}`, {
    method: body === undefined ? 'GET' : 'POST', redirect: 'error', cache: 'no-store', signal: AbortSignal.timeout(10000),
    headers: { Authorization: `Bearer ${token}`, ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  assert(response.ok && Number(response.headers.get('content-length')) <= 65536, 'The actual UI controller accepts this bounded operation');
  const value = record(await response.json());
  assert(value.ok === true, 'The actual UI controller completes this operation');
  return value.result;
}
export async function facts(): Promise<PaidFacts> {
  const value = record(await request('/facts'));
  assert(value.schemaVersion === 1);
  const nodes = record(value.nodes), paired = record(value.paired);
  const node = (role: typeof roles[number]) => { const item = record(nodes[role]); return { url: loopback(item.url), peer: hex(item.peer) }; };
  assert(Array.isArray(value.events) && value.events.every((event: unknown) => typeof event === 'string'));
  const failure = nullable(value.failure);
  assert(failure === null, 'Both real UI tests must remain healthy');
  assert(value.name === 'ordinary-paid-ui.bin' && value.bytes === 4096
    && resolve(root, string(value.sourceFile)) === resolve(artifacts, 'ordinary-paid-ui.bin'),
  'The Web UI selects only the bounded synthetic original in the dedicated controller directory');
  return { nodes: { web: node('web'), android: node('android'), provider: node('provider') },
    sourceFile: string(value.sourceFile), name: string(value.name), bytes: integer(value.bytes), sha256: hex(value.sha256),
    serviceCap: string(value.serviceCap), gasCap: string(value.gasCap), source: order(value.source), receiver: order(value.receiver), provider: order(value.provider),
    settled: boolean(value.settled), paired: { web: boolean(paired.web), android: boolean(paired.android) }, events: value.events, failure,
    stage: member(value.stage, ['bootstrap', 'inspections', 'ordinary', 'complete']), inspection: inspection(value.inspection) };
}
export async function nextInspection(kind: InspectionKind): Promise<Inspection> {
  await expect.poll(async () => {
    const current = await facts();
    assert(current.stage === 'bootstrap' || current.stage === 'inspections', 'The required original-order inspection precedes ordinary payment');
    return current.inspection?.kind === kind;
  }, { timeout: deadline(), interval: 1000, message: `The original real Node reaches its fixed ${kind} inspection` }).toBe(true);
  const value = (await facts()).inspection;
  assert(value && value.kind === kind);
  return value;
}
export async function ordinaryReady(): Promise<void> {
  await expect.poll(async () => (await facts()).stage === 'ordinary', {
    timeout: deadline(), interval: 1000, message: 'All original-order inspections and key lifecycle finish before actual ordinary UI actions',
  }).toBe(true);
}
export async function acknowledgeInspection(value: Inspection, screenshot: string): Promise<void> {
  await preserveScreenshot(value.screenshot, screenshot);
  const image = await readFile(resolve(artifacts, 'public', value.screenshot));
  await event('inspection-observed', { role: value.role, id: value.id, screenshot: value.screenshot,
    sha256: createHash('sha256').update(image).digest('hex'), bytes: image.length });
}
export async function pairing(role: UiRole): Promise<Pairing | null> {
  const value = await request('/pairing', { role });
  if (value === null) return null;
  const row = record(value), code = string(row.code);
  assert(/^[0-9]{6}$/.test(code));
  return { peer: hex(row.peer), code, approved: boolean(row.approved) };
}
export async function invitation(role: UiRole): Promise<string> { return string(record(await request(`/invitation/${role}`)).invitation); }
export async function event(name: string, extra: Record<string, unknown> = {}): Promise<void> { await request('/event', { name, ...extra }); }
export async function runUi(role: UiRole, execute: () => Promise<void>): Promise<void> {
  try { await execute(); }
  catch (error) { await request('/abort', { role }).catch(() => undefined); throw error; }
}
export async function saveEvidence(name: string, value: Record<string, unknown>): Promise<void> {
  assert(/^(web|android)\.json$/.test(name));
  await mkdir(resolve(artifacts, 'public'), { recursive: true, mode: 0o700 });
  await writeFile(resolve(artifacts, 'public', name), `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
}
export async function preserveScreenshot(name: string, source: string): Promise<string> {
  assert(['web.png', 'android-offline.png', 'android.png', 'web-settled.png', 'web-partial-refund.png',
    'web-unknown-transaction.png', 'android-key-before.png', 'android-key-rotated.png'].includes(name));
  const location = insideArtifacts(source), suffix = relative(resolve(artifacts, 'e2e'), location);
  assert(suffix && !suffix.startsWith('..') && !suffix.startsWith('/'), 'Export only this run explicit post-join e2e screenshot');
  const metadata = await lstat(location);
  assert(metadata.isFile() && !metadata.isSymbolicLink() && metadata.size <= 10 * 1024 * 1024);
  const image = await readFile(location);
  assert(image.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10])));
  await mkdir(resolve(artifacts, 'public'), { recursive: true, mode: 0o700 });
  await copyFile(location, resolve(artifacts, 'public', name));
  return name;
}
