import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { stripTypeScriptTypes } from 'node:module';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const moduleUrl = pathToFileURL(resolve(process.argv[2]));
const crypto = await import(moduleUrl);
await crypto.default({ module_or_path: await readFile(new URL('./vibeface_secure_bg.wasm', moduleUrl)) });
const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });
const keys = new Map(['client', 'root', 'recovery'].map(name => [name, crypto.Identity.generate()]));
const connections = new Map();
const authorizations = new Map();
const gateways = new Map();
const gatewayPending = new Map();
const verifiedBindings = new Map();
const trusts = new Map();
const preparations = new Map();
const remoteAuthorizations = new Map();
let creation;
let trust;
let openedRecoveryMaterial;
let recoveryRequest;
let recoveryPreparation;
let nodeEnrollment;
let nodeEnrollmentCommit;
let browserEnrollment;

async function browserLibrary(name) {
  const sourceUrl = new URL(`../crates/node/web/src/lib/${name}.ts`, import.meta.url);
  let source = stripTypeScriptTypes(await readFile(sourceUrl, 'utf8'));
  for (const match of [...source.matchAll(/from ['"]([^'"]+)['"]/g)]) {
    const path = match[1];
    const target = path.endsWith('.js') ? new URL(path, sourceUrl).href : await browserLibrary(path.replace('./', ''));
    source = source.replace(match[0], `from ${JSON.stringify(target)}`);
  }
  return `data:text/javascript;base64,${Buffer.from(source).toString('base64')}`;
}
function sealedRecovery(material, password) {
  const plaintext = encoder.encode(material);
  try { return Buffer.from(crypto.recoverySeal(plaintext, password)).toString('hex'); }
  finally { plaintext.fill(0); }
}
const identity = name => {
  const key = keys.get(name || 'client');
  if (!key) throw new Error('unknown local identity');
  return key;
};
const exposed = key => ({ public_key: key.publicKeyHex(), node_id: key.nodeId(), device_id: crypto.deriveAuthorizationId('device', key.publicKeyHex()) });
const actions = {
  conversations: 'conversation_read', conversation: 'conversation_read', context: 'conversation_read',
  nodes: 'operation_status_read', session: 'operation_status_read', operation: 'operation_status_read',
  create_conversation: 'conversation_create', message: 'message_append', create_task: 'task_create',
  bind_execution: 'execution_open', seal_execution: 'execution_rebind', rebind_execution: 'execution_rebind', notifications: 'notification_read',
  ack: 'notification_ack', export: 'export', reconcile: 'operation_reconcile', resolve_unknown: 'operation_resolve',
  replica_inventory: 'replica_read', replica_event: 'replica_read', replica_blob: 'replica_read',
  execution_share_request: 'conversation_read', seal_remote_execution: 'execution_rebind',
  remote_execution_seal_status: 'operation_status_read',
};
const operationActions = {
  open: 'execution_open', read: 'operation_status_read', send: 'execution_send', steer: 'execution_steer',
  interrupt: 'execution_interrupt', respond_request: 'execution_respond_approval',
};
function expectedAction(body) {
  if (body.method === 'context' && typeof body.blob_hash === 'string') return 'attachment_read';
  if (body.method === 'operate') {
    const action = operationActions[body.request?.action?.type];
    if (!action) throw new Error('unknown execution action');
    return action;
  }
  if (body.method === 'update_task') {
    if (body.status === 'accepted') return 'task_accept';
    if (body.status === 'cancelled') return 'task_cancel';
    if (['open', 'working', 'blocked', 'needs_review'].includes(body.status)) return 'task_progress';
    throw new Error('unknown task progress');
  }
  if (!actions[body.method]) throw new Error('unknown business action');
  return actions[body.method];
}

function bindingKey(body) {
  const request = body.method === 'operate' ? body.request : body;
  return JSON.stringify([body.space_id, request.conversation_id, request.task_id, request.target_node, request.session_id]);
}

function checkedPreparation(input, rawContext, prepared) {
  const expected = structuredClone(input.body);
  const known = input.normalization || {};
  const context = structuredClone(rawContext.context);
  if (expected.method === 'update_task' && expected.detail === undefined) expected.detail = '';
  if (expected.method === 'message' && expected.scheduler_node === undefined && prepared.body.scheduler_node !== undefined) {
    assert.equal(typeof known.scheduler_node, 'string', 'scheduler normalization requires an independently selected Node');
    expected.scheduler_node = known.scheduler_node;
  }
  if (expected.method === 'operate') {
    const request = expected.request;
    const saved = verifiedBindings.get(bindingKey(expected));
    if (request.binding === undefined && prepared.body.request?.binding !== undefined) {
      assert(saved, 'Node cannot supply an execution binding that the client has not verified');
      request.binding = structuredClone(saved);
    }
    if (request.binding !== undefined) {
      assert(saved, 'execution requires a previously verified binding');
      assert.deepEqual(request.binding, saved, 'request differs from the independently verified binding');
      const binding = saved.binding;
      const targetKey = rawContext.execution_target_public_key;
      assert.equal(typeof targetKey, 'string', 'execution target must have an independently verified key');
      const boundContext = {
        conversation: binding.conversation_id, task: binding.task_ref,
        execution_session: { target: crypto.deriveAuthorizationId('device', targetKey), session_id: binding.session_id },
        executor_kind: binding.open_spec.executor, workspace_resource: expected.resource_id,
      };
      for (const [field, value] of Object.entries(boundContext)) {
        if (context[field] !== null) assert.deepEqual(context[field], value, 'normalization changed verified execution intent');
        context[field] = value;
      }
    }
    if (request.action.type === 'open' && prepared.body.request?.action?.cwd !== request.action.cwd) {
      const canonical = known.canonical_cwd || saved?.binding.open_spec.cwd;
      assert.equal(typeof canonical, 'string', 'cwd normalization requires an independently verified canonical path');
      assert.equal(canonical, saved?.binding.open_spec.cwd, 'canonical cwd differs from the verified open intent');
      request.action.cwd = canonical;
    }
    if (Array.isArray(request.resources) || (request.resources === undefined && prepared.body.request?.resources !== undefined)) {
      const resources = (request.resources || []).map(path => known.resource_paths?.[path] ?? path);
      if (request.action.type === 'open') resources.push(request.action.cwd);
      else if (request.action.type === 'send') {
        assert(saved, 'send normalization requires a previously verified session directory');
        resources.push(saved.binding.open_spec.cwd);
      }
      request.resources = [...new Set(resources)].sort();
    }
  }
  assert.deepEqual(prepared.body, expected, 'Node changed request fields outside the permitted normalization');
  assert.deepEqual(prepared.invocation.context, context, 'Node changed the independently verified final context');
  return { body: prepared.body, context };
}

async function connect(input) {
  const key = identity(input.identity);
  assert.match(input.node_public_key, /^[a-f0-9]{64}$/);
  assert.equal(createHash('sha256').update(Buffer.from(input.node_public_key, 'hex')).digest('hex'), input.node_id);
  const socket = new WebSocket(new URL('/api/network/ws', input.url.replace(/^http/, 'ws')));
  socket.binaryType = 'arraybuffer';
  const queue = [], waiting = [], pending = new Map(), events = [], frames = [];
  let channel, closed = false, sequence = 0;
  function close(error = new Error('channel closed')) {
    if (closed) return;
    closed = true;
    for (const waiter of waiting.splice(0)) waiter.reject(error);
    for (const request of pending.values()) { clearTimeout(request.timer); request.reject(error); }
    pending.clear(); socket.close();
  }
  const receive = () => queue.length ? Promise.resolve(queue.shift()) : closed ? Promise.reject(new Error('channel closed')) : new Promise((resolve, reject) => waiting.push({ resolve, reject }));
  socket.addEventListener('message', event => {
    const waiter = waiting.shift();
    if (waiter) waiter.resolve(event.data);
    else if (queue.length < 512) queue.push(event.data);
    else close(new Error('bounded receive queue'));
  });
  socket.addEventListener('close', () => close());
  socket.addEventListener('error', () => close(new Error('WebSocket transport failed')));
  async function control(type) {
    const frame = await receive();
    if (typeof frame !== 'string' || JSON.parse(frame).type !== type) throw new Error(`expected ${type}`);
  }
  const timer = setTimeout(() => close(new Error('handshake timeout')), 15_000);
  try {
    await control('welcome');
    socket.send(JSON.stringify(input.relay ? { type: 'dial', target: input.node_id, service: 'client' } : { type: 'direct', service: 'client' }));
    await control('ready');
    const handshake = new crypto.Handshake(true, key.secretHex(), input.node_id, encoder.encode(input.context || 'vibeface:client:v2'));
    try {
      socket.send(handshake.write(new Uint8Array()));
      const response = await receive();
      if (!(response instanceof ArrayBuffer)) throw new Error('Noise response must be binary');
      handshake.read(new Uint8Array(response));
      socket.send(handshake.write(new Uint8Array()));
      assert(handshake.isFinished());
      assert.equal(handshake.remoteId(), input.node_id);
      channel = handshake.finish();
    } finally { handshake.free(); }
  } catch (error) { close(error); throw error; }
  finally { clearTimeout(timer); }
  (async () => {
    try {
      while (!closed) {
        const frame = await receive();
        if (!(frame instanceof ArrayBuffer)) throw new Error('plaintext application frame rejected');
        const plaintext = channel.openRecord(new Uint8Array(frame));
        if (plaintext === null) continue;
        const value = JSON.parse(decoder.decode(plaintext));
        const request = pending.get(value.id);
        if (request) { pending.delete(value.id); clearTimeout(request.timer); request.resolve(value); }
        else events.push(value);
      }
    } catch (error) { close(error); }
    finally { channel.free(); }
  })();
  return {
    events, frames, nodePublicKey: input.node_public_key, nodeId: input.node_id, url: input.url, relay: Boolean(input.relay), identityName: input.identity || 'client',
    trustRef: input.trust_ref || 'primary',
    request(method, params, invocation) {
      if (closed) throw new Error('channel closed');
      const id = String(++sequence);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => { pending.delete(id); reject(new Error('request timeout')); }, 20_000);
        pending.set(id, { resolve, reject, timer });
        for (const record of channel.sealMessage(encoder.encode(JSON.stringify({ id, method, params, ...(invocation ? { invocation } : {}) })))) {
          frames.push(Buffer.from(record)); socket.send(record);
        }
      });
    },
    async attack(kind) {
      if (!frames.length) throw new Error('no protected request to attack');
      const record = Buffer.from(frames.at(-1));
      if (kind === 'tamper') record[record.length - 1] ^= 1;
      else if (kind !== 'replay') throw new Error('unknown channel attack');
      socket.send(record);
      for (let attempt = 0; attempt < 50 && !closed; attempt++) await new Promise(resolve => setTimeout(resolve, 100));
      if (!closed) throw new Error('invalid protected frame did not close the channel');
      return { rejected: true };
    },
    close,
  };
}

function boundedCarrier(request, response) {
  assert(Buffer.byteLength(JSON.stringify(request)) + Buffer.byteLength(JSON.stringify(response)) <= 65_536,
    'identity carrier exceeds 64 KiB');
}

async function identityRequest(connection, method, params) {
  boundedCarrier(params, {});
  const response = await connection.request('identity', { method, params });
  boundedCarrier(params, response);
  if (response.error) {
    const error = new Error(`identity ${method} rejected`);
    error.rpcMethod = method;
    throw error;
  }
  assert(response.result && typeof response.result === 'object', 'identity response has no result');
  return response.result;
}

function savedTrust(ref = 'primary') {
  const value = ref === 'primary' ? trust : trusts.get(ref);
  assert(value, 'authorization requires independently saved pins');
  return value;
}

function saveTrust(ref, value) {
  const previous = ref === 'primary' ? trust : trusts.get(ref);
  if (previous) {
    assert.deepEqual(value.scope, previous.scope, 'trust update changed its account or space');
    for (const pin of previous.pins) {
      const next = value.pins.find(item => JSON.stringify(item.domain) === JSON.stringify(pin.domain));
      assert(next && next.genesis_root === pin.genesis_root, 'trust update changed an independently pinned root');
      if (pin.minimum_head) {
        const before = pin.minimum_head;
        const after = next.minimum_head;
        assert(after, 'trust update removed a saved policy floor');
        assert.deepEqual(after.domain, before.domain, 'trust update changed a policy floor domain');
        const sameVersion = after.authority_epoch === before.authority_epoch && after.sequence === before.sequence;
        assert(after.authority_epoch > before.authority_epoch
          || after.authority_epoch === before.authority_epoch && after.sequence >= before.sequence,
        'trust update lowered a saved policy floor');
        if (sameVersion) assert.deepEqual(after, before, 'trust update forked a saved policy floor');
      }
    }
  }
  if (ref === 'primary') trust = structuredClone(value);
  else trusts.set(ref, structuredClone(value));
}

function checkedAuthorization(verified, expectedHostPublicKey, selectedTrust) {
  const scope = JSON.parse(verified.scopeJson());
  const policy = JSON.parse(verified.policyJson());
  assert.deepEqual(scope, selectedTrust.scope, 'authorization changed the independently saved scope');
  assert.equal(policy.audience_public_key, expectedHostPublicKey, 'authorization changed the selected Project host');
  assert(Array.isArray(policy.resources) && policy.resources.length === 1, 'authorization must select one Project');
  return { scope, policy, resources: policy.resources };
}

async function authenticate(input, connectionName) {
  assert(!gateways.has(connectionName), 'Gateway connections cannot authenticate; refresh their B source explicitly');
  authorizations.get(connectionName)?.verified.free();
  authorizations.delete(connectionName);
  const connection = connections.get(connectionName);
  const trustRef = input.trust_ref || connection.trustRef;
  const selectedTrust = savedTrust(trustRef);
  const signer = identity(connection.identityName);
  const source = connection.nodePublicKey;
  const expectedHost = input.expected_host_public_key || source;
  assert.match(expectedHost, /^[a-f0-9]{64}$/);
  const pins = JSON.stringify(selectedTrust.pins);
  const buildRequest = selection => JSON.parse(crypto.buildClientPolicyReadRequest(
    selectedTrust.scope.account_id, selectedTrust.scope.space_id, signer, source, pins,
    JSON.stringify(selection), Date.now(), Date.now() + 60_000,
  ));
  const catalogRequest = buildRequest(null);
  let catalog = await identityRequest(connection, 'begin_client_policy_read', catalogRequest);
  let selection;
  const visited = new Set();
  let catalogContext;
  for (;;) {
    const page = JSON.parse(crypto.verifyClientPolicySelections(
      pins, JSON.stringify(catalogRequest), JSON.stringify(catalog.selections), signer, source, Date.now(),
    ));
    const context = JSON.stringify([page.snapshot_id, page.request_hash]);
    if (catalogContext === undefined) catalogContext = context;
    assert.equal(context, catalogContext, 'selection page changed its original catalog');
    for (const choice of page.selections) {
      const matches = (!input.resource_id || choice.resource.resource === input.resource_id)
        && choice.audience.service === crypto.deriveAuthorizationId('service', expectedHost)
        && choice.audience.device === crypto.deriveAuthorizationId('device', expectedHost)
        && (!input.audience || (choice.audience.service === input.audience.service && choice.audience.device === input.audience.device));
      if (input.grant_leaf && choice.grant_leaf !== input.grant_leaf) continue;
      if (matches && !selection) selection = choice;
    }
    if (selection || page.next_cursor === null) break;
    assert(!visited.has(page.next_cursor), 'selection cursor repeated');
    visited.add(page.next_cursor);
    catalog = await identityRequest(connection, 'client_policy_selections', {
      snapshot_id: page.snapshot_id, request_hash: page.request_hash, cursor: page.next_cursor,
    });
  }
  assert(selection, 'no verified selection matches the independently selected Project host');
  const request = buildRequest(selection);
  const begin = await identityRequest(connection, 'begin_client_policy_read', request);
  let importer = crypto.beginClientPolicyImport(pins, JSON.stringify(request), JSON.stringify(begin.snapshot), signer, source, Date.now());
  let imported;
  let confirmed;
  const frames = [{ stage: 'begin', request, snapshot: begin.snapshot }];
  try {
    for (;;) {
      const params = JSON.parse(importer.pageRequestJson());
      const response = await identityRequest(connection, 'client_policy_page', params);
      importer.appendPage(JSON.stringify(response.page), Date.now());
      frames.push({ stage: 'page', page: response.page });
      if (response.page.payload.complete) break;
    }
    const witness = await identityRequest(connection, 'client_policy_witness', {
      snapshot_id: begin.snapshot.payload.snapshot_id, request_hash: begin.snapshot.payload.request_hash, cursor: null,
    });
    const completed = importer;
    importer = undefined;
    imported = completed.finish(JSON.stringify(witness.witness), JSON.stringify(selection), Date.now());
    const confirmRequest = JSON.parse(crypto.buildClientPolicyAuthConfirmRequest(
      imported, signer, Date.now(), Math.min(request.payload.expires_at_ms, begin.snapshot.payload.expires_at_ms, witness.witness.payload.expires_at_ms),
    ));
    const receipt = await identityRequest(connection, 'confirm_client_policy_auth', confirmRequest);
    confirmed = crypto.confirmClientPolicyAuthorization(imported, JSON.stringify(confirmRequest), JSON.stringify(receipt.confirmation), source, Date.now());
    frames.push({ stage: 'finish', witness: witness.witness, confirm_request: confirmRequest, confirmation: receipt.confirmation });
    const checked = checkedAuthorization(confirmed, expectedHost, selectedTrust);
    if (input.resource_kind) assert.equal(checked.resources[0].payload.kind, input.resource_kind);
    const session = JSON.parse(confirmed.clientPolicyContextJson());
    const response = await connection.request('authenticate', session);
    boundedCarrier(session, response);
    if (response.error) {
      const error = new Error('confirmed client authentication rejected');
      error.rpcMethod = 'authenticate';
      throw error;
    }
    assert.deepEqual(response.result.scope, checked.scope, 'session changed the verified scope');
    assert.deepEqual(response.result.policy, checked.policy, 'session changed the verified policy');
    assert.deepEqual(response.result.resources, checked.resources, 'session changed the verified Project');
    authorizations.set(connectionName, { verified: confirmed, ...checked, selection, trustRef, frames,
      forwarded: checked.policy.audience_public_key !== source });
    confirmed = undefined;
    saveTrust(trustRef, { ...selectedTrust, pins: selectedTrust.pins.map(pin => ({ ...pin,
      minimum_head: checked.policy.heads.find(head => head.domain.kind === pin.domain.kind && head.domain.id === pin.domain.id) || pin.minimum_head,
    })) });
    return { ...checked, selection };
  } finally { importer?.free(); imported?.free(); confirmed?.free(); }
}

function gatewayBinding(connectionName) {
  const binding = gateways.get(connectionName);
  assert(binding?.state === 'ready', 'Gateway selection has no complete verified source');
  assert.equal(authorizations.get(binding.sourceConnection), binding.authorization,
    'Gateway source changed; attach the current confirmed B authorization');
  assert.equal(connections.get(connectionName)?.nodePublicKey, binding.card.gateway.node_public_key,
    'Gateway connection changed');
  return binding;
}

async function gatewaySource(connection, frame) {
  boundedCarrier(frame, {});
  const response = await connection.request('gateway_source', frame);
  boundedCarrier(frame, response);
  if (response.error) {
    const error = new Error(`Gateway source rejected: ${JSON.stringify(response.error)}`);
    error.rpcMethod = 'gateway_source';
    throw error;
  }
  assert(response.result && typeof response.result === 'object', 'Gateway returned no source result');
  return response.result;
}

async function attachGateway(input, connectionName) {
  const sourceConnection = input.source_connection || 'main';
  assert.notEqual(connectionName, sourceConnection, 'Gateway needs a separate connection from B');
  const connection = connections.get(connectionName);
  const source = authorizations.get(sourceConnection);
  const sourceChannel = connections.get(sourceConnection);
  assert(source && sourceChannel && connection, 'Gateway requires an existing confirmed B connection');
  const card = structuredClone(input.card);
  assert.equal(card.version, 2);
  assert.equal(card.kind, 'vibeface-gateway');
  assert.equal(connection.nodePublicKey, card.gateway.node_public_key, 'Gateway card changed the actual A pin');
  assert.equal(connection.nodeId, card.gateway.node_id, 'Gateway card changed the actual A Node');
  assert.equal(connection.url, card.gateway.url, 'Gateway card changed the selected endpoint');
  assert.equal(connection.relay, Boolean(card.gateway.relay), 'Gateway card changed the selected transport');
  assert.equal(connection.identityName, sourceChannel.identityName, 'Gateway and B must use the same U identity');
  assert.equal(sourceChannel.nodePublicKey, source.policy.audience_public_key, 'Gateway source must come directly from B');
  const route = card.approval.payload;
  assert.equal(route.gateway, connection.nodePublicKey);
  assert.equal(route.client_key, identity(sourceChannel.identityName).publicKeyHex());
  assert.equal(route.client_account, source.scope.account_id);
  assert.equal(route.target_key, source.policy.audience_public_key);
  assert.deepEqual(route.resource, { space: source.scope.space_id, resource: source.resources[0].payload.resource });
  assert(Number.isSafeInteger(route.generation) && route.generation > 0, 'Gateway route requires a positive generation');
  assert.equal(route.state, 'active', 'Gateway route is not active');
  assert(Number.isSafeInteger(route.issued_at_ms) && Number.isSafeInteger(route.expires_at_ms)
    && route.issued_at_ms <= Date.now() && Date.now() < route.expires_at_ms, 'Gateway route is outside its approved window');
  const approvalHash = crypto.gatewayRouteApprovalHash(JSON.stringify(card.approval));
  const selection = { state: 'selected', card, sourceConnection, authorization: source };
  gateways.set(connectionName, selection);
  const committed = await gatewaySource(connection, { stage: 'route', approval: card.approval });
  assert.deepEqual(committed, { route_id: route.route_id, approval: approvalHash, generation: route.generation },
    'Gateway installed a different owner-approved route');
  let progress;
  let sentPages = 0;
  const totalPages = source.frames.filter(frame => frame.stage === 'page').length;
  for (const original of source.frames) {
    const frame = original.stage === 'begin'
      ? { ...original, route_id: route.route_id } : { ...original, reference: progress.reference };
    const next = await gatewaySource(connection, frame);
    assert.equal(typeof next.reference, 'string', 'Gateway source has no cache reference');
    if (progress) assert.equal(next.reference, progress.reference, 'Gateway source reference changed');
    if (original.stage === 'page') sentPages++;
    assert(Number.isSafeInteger(next.pages) && next.pages >= sentPages && next.pages >= (progress?.pages || 0)
      && next.pages <= totalPages, 'Gateway source did not retain the exact page sequence');
    assert.equal(typeof next.complete, 'boolean', 'Gateway source completion is invalid');
    if (next.complete) assert.equal(next.pages, totalPages, 'Gateway completed without all original pages');
    if (original.stage === 'finish') assert(next.complete, 'Gateway did not confirm the original finish');
    progress = next;
  }
  assert(progress?.complete, 'Gateway source is incomplete');
  assert.equal(gateways.get(connectionName), selection, 'Gateway selection changed during source import');
  assert.equal(authorizations.get(sourceConnection), source, 'B authorization changed during source import');
  const binding = { state: 'ready', card, sourceConnection, authorization: source, approvalHash, sourceRef: progress.reference };
  gateways.set(connectionName, binding);
  return { scope: source.scope, policy: source.policy, resources: source.resources, selection: source.selection,
    gateway: { public_key: connection.nodePublicKey, approval_hash: approvalHash, source_ref: progress.reference,
      generation: route.generation } };
}

async function forwardGateway(connectionName, original) {
  const binding = gatewayBinding(connectionName);
  const connection = connections.get(connectionName);
  const sourceChannel = connections.get(binding.sourceConnection);
  const originalJson = JSON.stringify(original);
  const id = original.kind === 'prepare' ? null : original.body.authorization_id;
  const key = id === null ? null : JSON.stringify([connectionName, id]);
  assert(!key || !gatewayPending.has(key), 'Gateway request outcome is unknown; reconcile the original operation before another send');
  const handle = crypto.prepareGatewayClientReply(binding.authorization.verified, originalJson,
    binding.approvalHash, connection.nodePublicKey, identity(sourceChannel.identityName), Date.now());
  const pending = { handle, original, binding, state: 'unknown' };
  if (key) gatewayPending.set(key, pending);
  let verified = false;
  try {
    const response = await gatewaySource(connection, { stage: 'forward', reference: binding.sourceRef, original });
    assert.equal(gatewayBinding(connectionName), binding, 'Gateway route changed during this request');
    const outcome = JSON.parse(handle.verifyReply(JSON.stringify(response.forward), JSON.stringify(response.reply),
      JSON.stringify(response.outcome), Date.now()));
    assert(['ok', 'error'].includes(outcome.kind), 'Gateway returned an invalid verified outcome');
    verified = true;
    return outcome.kind === 'ok' ? { result: outcome.value } : { error: outcome.value };
  } finally {
    if (verified || !key) {
      handle.free();
      if (key) gatewayPending.delete(key);
    }
  }
}

function retainPreparation(input, kind, prepared, trustRef) {
  assert.equal(typeof input.handle, 'string', 'native preparation needs an explicit handle');
  assert(!preparations.has(input.handle), 'retain the original preparation until its receipt is verified');
  preparations.set(input.handle, { kind, prepared, trustRef });
  return { handle: input.handle, request: JSON.parse(prepared.requestJson()) };
}

async function readRemotePolicy(input, connectionName) {
  const binding = gateways.has(connectionName) ? gatewayBinding(connectionName) : null;
  const sourceName = binding?.sourceConnection || connectionName;
  const source = authorizations.get(sourceName);
  assert(source, 'remote policy needs a confirmed source authorization');
  assert(!remoteAuthorizations.has(input.handle), 'release the previous remote authorization explicitly');
  const b = connections.get(input.source_connection || sourceName);
  const c = connections.get(input.execution_connection);
  assert(b && c, 'remote policy needs separate pinned source and execution connections');
  assert.equal(b.nodePublicKey, source.policy.audience_public_key, 'source policy connection must reach B');
  assert.equal(c.nodePublicKey, input.selection.target_public_key, 'execution policy connection must reach C');
  const executionTrust = savedTrust(input.execution_trust_ref);
  assert.equal(executionTrust.scope.space_id, input.selection.execution_resource.space);
  const signer = identity(connections.get(sourceName).identityName);
  const intent = JSON.parse(crypto.buildRemoteExecutionPolicyReadIntent(source.verified, JSON.stringify({
    selection: input.selection, context: input.raw_context, expected_action: input.action,
    purpose: input.purpose || 'execution', execution_pins: executionTrust.pins,
  }), signer, Date.now(), Date.now() + 60_000));
  const selected = await identityRequest(b, 'foreign_execution_policy_selection', intent);
  let approvalRef = selected.approval_ref;
  if (input.approve) {
    const approved = await identityRequest(b, 'foreign_execution_policy_approve', intent);
    approvalRef = approved.approval_ref;
  }
  assert(approvalRef || input.candidate, 'task requires an independently approved execution root');
  const delegated = await identityRequest(b, 'remote_execution_policy_delegate', intent);
  const request = { intent, delegation: delegated.delegation };
  const begun = await identityRequest(c, 'remote_execution_policy_prepare', request);
  let importer = crypto.beginRemoteExecutionPolicyImport(source.verified, JSON.stringify(request),
    JSON.stringify(begun.snapshot), JSON.stringify(executionTrust.pins), c.nodePublicKey, Date.now());
  let policy;
  try {
    let progress = input.candidate ? null : await identityRequest(b, 'foreign_execution_policy_begin', {
      request, snapshot: begun.snapshot, ...(approvalRef ? { approval_ref: approvalRef } : {}),
    });
    const importRef = progress?.import_ref;
    for (;;) {
      const pageRequest = importer.pageRequestJson();
      const paged = await identityRequest(c, 'remote_execution_policy_page', JSON.parse(pageRequest));
      importer.appendPage(JSON.stringify(paged.page), Date.now());
      if (progress) {
        const next = await identityRequest(b, 'foreign_execution_policy_page', { import_ref: importRef, page: paged.page });
        assert.equal(next.import_ref, importRef);
        assert(next.next_page_index > paged.page.payload.page_index && next.next_page_index >= progress.next_page_index);
        progress = next;
      }
      if (paged.page.payload.complete) break;
      assert.notEqual(importer.pageRequestJson(), pageRequest, 'remote policy page must advance');
    }
    const finished = await identityRequest(c, 'remote_execution_policy_finish', { ...JSON.parse(importer.pageRequestJson()), cursor: null });
    const complete = importer; importer = undefined;
    policy = complete.finish(JSON.stringify(finished.witness), Date.now());
    const current = policy.revalidatePins(JSON.stringify(savedTrust(input.execution_trust_ref).pins), Date.now());
    policy.free(); policy = current;
    if (progress) {
      assert(progress.ready_for_finish, 'B has not accepted the complete transcript');
      const accepted = await identityRequest(b, 'foreign_execution_policy_finish', { import_ref: importRef, witness: finished.witness });
      assert.equal(accepted.approval_ref, approvalRef);
      assert.equal(accepted.purpose, input.purpose || 'execution');
    }
    const remote = crypto.verifyRemoteExecution(source.verified, policy, Date.now());
    remoteAuthorizations.set(input.handle, { remote, source: source.verified, connectionName });
    return { handle: input.handle, metadata: JSON.parse(remote.metadataJson()), approval_ref: approvalRef };
  } finally { importer?.free(); policy?.free(); }
}

function currentRemote(input, connectionName) {
  const auth = gateways.has(connectionName) ? gatewayBinding(connectionName).authorization : authorizations.get(connectionName);
  const remote = remoteAuthorizations.get(input.handle);
  assert(auth && remote && remote.connectionName === connectionName && remote.source === auth.verified,
    'remote authorization belongs to this exact confirmed source session');
  return remote.remote;
}

async function readSharePolicy(input, connectionName) {
  const connection = connections.get(connectionName);
  const sourceTrust = savedTrust(input.source_trust_ref);
  const original = input.intent.intent;
  assert.equal(original.project.space, sourceTrust.scope.space_id);
  assert.equal(original.source_node_public_key, connection.nodePublicKey);
  const request = JSON.parse(crypto.buildRemoteSharePolicyReadRequest(JSON.stringify(input.intent),
    identity(connection.identityName), JSON.stringify(sourceTrust.pins), connection.nodePublicKey, Date.now(),
    Math.min(Date.now() + 60_000, original.expires_at_ms)));
  const begun = await identityRequest(connection, 'execution_share_policy_prepare', request);
  let importer = crypto.beginRemoteSharePolicyImport(JSON.stringify(request), JSON.stringify(begun.snapshot),
    JSON.stringify(sourceTrust.pins), connection.nodePublicKey, Date.now());
  let verified;
  try {
    for (;;) {
      const page = await identityRequest(connection, 'execution_share_policy_page', JSON.parse(importer.pageRequestJson()));
      importer.appendPage(JSON.stringify(page.page), Date.now());
      if (page.page.payload.complete) break;
    }
    const finished = await identityRequest(connection, 'execution_share_policy_finish', { ...JSON.parse(importer.pageRequestJson()), cursor: null });
    const completed = importer; importer = undefined;
    verified = completed.finish(JSON.stringify(finished.witness), Date.now());
    assert(!preparations.has(input.handle));
    preparations.set(input.handle, { kind: 'share_source', prepared: verified, trustRef: input.source_trust_ref });
    const result = { handle: input.handle, summary: JSON.parse(verified.summaryJson()) };
    verified = undefined;
    return result;
  } finally { importer?.free(); verified?.free(); }
}

async function command(input) {
  if (input.op === 'browser_enrollment') {
    if (!browserEnrollment) {
      const generated = new URL('../crates/node/web/generated/vibeface_secure.js', import.meta.url);
      const native = await import(generated.href);
      await native.default({ module_or_path: await readFile(new URL('./vibeface_secure_bg.wasm', generated)) });
      Object.defineProperty(globalThis, 'isSecureContext', { configurable: true, value: true });
      Object.defineProperty(globalThis, 'location', { configurable: true, value: new URL(input.reference.endpoint) });
      const transport = await import(new URL('../crates/node/web/transport.js', import.meta.url).href);
      const own = await transport.initClient({ mode: 'temporary' });
      browserEnrollment = { native, transport, own, library: await import(await browserLibrary('enrollment')), saved: null, pending: null, channel: null, writes: 0 };
    }
    const state = browserEnrollment;
    state.pending?.release(); state.pending = null; state.channel?.close();
    const reference = input.reference;
    state.channel = await state.transport.connectDevice({ address: reference.endpoint, node_id: state.native.nodeFingerprint(reference.node_public_key), relay: reference.relay });
    try {
      state.pending = await state.library.requestDeviceEnrollment({ reference, ownPublicKey: state.own.publicKey, signal: new AbortController().signal, saved: state.saved,
        save(request) { state.saved = JSON.parse(JSON.stringify(request)); },
        async call(method, params) {
          const result = await state.channel.request('identity', { method, params });
          if (method === 'enrollment_request') {
            state.writes++;
            if (input.drop_response) throw new Error('discarded enrollment response');
          }
          return result;
        },
      });
      return { status: state.pending.status.status, request_id: state.pending.requestId, public_key: state.own.publicKey, writes: state.writes };
    } catch (error) {
      if (!input.drop_response || error.message !== 'discarded enrollment response') throw error;
      return { status: 'response_lost', request_id: state.saved.requestId, public_key: state.own.publicKey, writes: state.writes };
    }
  }
  if (input.op === 'browser_possession') {
    const state = browserEnrollment, original = state.pending;
    const lookup = { account_id: original.reference.account_id, offer_hash: original.reference.offer_hash, request_id: original.requestId };
    const status = state.library.verifyEnrollmentStatus(await state.channel.request('identity', { method: 'enrollment_status', params: lookup }), original);
    assert.equal(status.status, 'approved');
    const qualified = state.native.verifyAccountEnrollmentFromOffer(JSON.stringify(status.intent), original.offer, Date.now());
    try {
      const certificate = JSON.parse(state.transport.signClientEnrollmentPossession(qualified, original.nonce));
      return await state.channel.request('identity', { method: 'enrollment_possession', params: { ...lookup, certificate } });
    } finally { qualified.free(); }
  }
  const connectionName = input.connection || 'main';
  if (input.op === 'identity') return exposed(identity(input.identity));
  if (input.op === 'get_trust') return structuredClone(savedTrust(input.trust_ref));
  if (input.op === 'save_trust') { saveTrust(input.trust_ref, input.trust); return { saved: true }; }
  if (input.op === 'release_remote') {
    remoteAuthorizations.get(input.handle)?.remote.free();
    remoteAuthorizations.delete(input.handle);
    return { released: true };
  }
  if (input.op === 'verify_remote_seal') return JSON.parse(crypto.verifyRemoteExecutionSeal(
    JSON.stringify(input.binding), JSON.stringify(input.seal), input.project_public_key,
    input.execution_public_key, JSON.stringify(input.revision)));
  if (input.op === 'derive_authorization_id') return crypto.deriveAuthorizationId(input.kind, input.value);
  if (input.op === 'sign_gateway_route') return JSON.parse(crypto.signGatewayRouteApproval(JSON.stringify(input.payload), identity(input.identity || 'root')));
  if (input.op === 'gateway_route_hash') return crypto.gatewayRouteApprovalHash(JSON.stringify(input.approval));
  if (input.op === 'signed_invocation_hash') return crypto.signedInvocationHash(JSON.stringify(input.invocation));
  if (input.op === 'sign') return identity(input.identity).sign(input.domain, encoder.encode(typeof input.payload === 'string' ? input.payload : JSON.stringify(input.payload)));
  if (input.op === 'verify_space_fixture') {
    const { payload, issuer, signature } = input.signed;
    const prepared = JSON.parse(crypto.prepareAuthorizationSigning('space_document', JSON.stringify(payload), issuer));
    assert.equal(prepared.object_hash, input.head.head, 'fixture differs from the verified policy head');
    assert.equal(payload.space, trust.scope.space_id);
    assert.equal(payload.authority_epoch, input.head.authority_epoch);
    assert.equal(payload.sequence, input.head.sequence);
    crypto.verify(issuer, prepared.domain, encoder.encode(prepared.signing_bytes), signature);
    return payload;
  }
  if (input.op === 'sign_resource_fixture') {
    assert(['resource', 'space_document'].includes(input.kind));
    const root = identity('root');
    const prepared = JSON.parse(crypto.prepareAuthorizationSigning(input.kind, JSON.stringify(input.payload), root.publicKeyHex()));
    return { hash: prepared.object_hash, signed: { payload: input.payload, issuer: root.publicKeyHex(), signature: root.sign(prepared.domain, encoder.encode(prepared.signing_bytes)) } };
  }
  if (input.op === 'prepare_creation') {
    const now = Date.now();
    const claim = crypto.verifyNodeClaim(JSON.stringify(input.claim), input.node_public_key, input.claim_nonce, now);
    try {
      creation?.free();
      creation = crypto.prepareAccountCreation(identity('root'), identity('recovery'), identity(), claim, now + 86_400_000, now);
      return JSON.parse(creation.nodeRequestJson());
    } finally { claim.free(); }
  }
  if (input.op === 'finish_creation') {
    const created = crypto.finishAccountCreation(creation, identity('root'), input.proof, Date.now());
    try {
      trust = { pins: JSON.parse(created.pinsJson()), scope: JSON.parse(created.scopeJson()) };
      const result = { pins: trust.pins, scope: trust.scope, authorization: JSON.parse(created.bundleJson()) };
      if (input.password) result.recovery_envelope = sealedRecovery(crypto.buildAccountRecoveryMaterial(created, identity('root'), identity('recovery'), input.endpoint, Date.now()), input.password);
      return result;
    } finally { created.free(); creation.free(); creation = undefined; }
  }
  if (input.op === 'open_recovery') {
    openedRecoveryMaterial = undefined;
    recoveryRequest = undefined;
    recoveryPreparation?.free(); recoveryPreparation = undefined;
    const plaintext = crypto.recoveryOpen(Uint8Array.from(Buffer.from(input.envelope, 'hex')), input.password);
    try {
      const material = decoder.decode(plaintext);
      const context = JSON.parse(crypto.inspectAccountRecoveryMaterial(material, Date.now()));
      openedRecoveryMaterial = material;
      return context;
    } finally { plaintext.fill(0); }
  }
  if (input.op === 'recovery_context_request') {
    const now = Date.now();
    recoveryRequest = JSON.parse(crypto.buildRecoveryContextRequest(openedRecoveryMaterial, identity(), input.node_public_key, now, now + 60_000));
    return recoveryRequest;
  }
  if (input.op === 'prepare_recovery') {
    assert(openedRecoveryMaterial && recoveryRequest && !recoveryPreparation, 'recovery requires its original private material and an unprepared request');
    const connection = connections.get(connectionName);
    assert.equal(connection.nodePublicKey, recoveryRequest.payload.node_public_key, 'recovery changed the independently pinned Noise source');
    const begin = await identityRequest(connection, 'recovery_context', { request: recoveryRequest });
    let importer = crypto.beginRecoveryContextImport(openedRecoveryMaterial, JSON.stringify(recoveryRequest), JSON.stringify(begin.snapshot), Date.now());
    let verified;
    try {
      const params = { snapshot_id: begin.snapshot.payload.snapshot_id, request_hash: begin.snapshot.payload.request_hash, cursor: null };
      for (;;) {
        const response = await identityRequest(connection, 'recovery_context_page', params);
        const progress = JSON.parse(importer.appendPage(JSON.stringify(response.page), Date.now()));
        params.cursor = progress.cursor;
        if (progress.complete) break;
      }
      const witness = await identityRequest(connection, 'recovery_context_witness', { ...params, cursor: null });
      const completed = importer;
      importer = undefined;
      verified = completed.finish(JSON.stringify(witness.witness), Date.now());
      const now = Date.now();
      recoveryPreparation = crypto.prepareAccountRecovery(openedRecoveryMaterial, verified, identity('root'), identity(), now + 86_400_000, now);
      return JSON.parse(recoveryPreparation.nodeRequestJson());
    } finally { importer?.free(); verified?.free(); }
  }
  if (input.op === 'finish_recovery') {
    const created = crypto.finishAccountRecovery(recoveryPreparation, identity('root'), input.proof, Date.now());
    try {
      const material = crypto.buildRecoveredAccountRecoveryMaterial(created, identity('root'), openedRecoveryMaterial, input.endpoint, Date.now());
      trust = { pins: JSON.parse(created.pinsJson()), scope: JSON.parse(created.scopeJson()) };
      return { pins: trust.pins, scope: trust.scope,
        commit: JSON.parse(recoveryPreparation.commitJson(created)), recovery_envelope: sealedRecovery(material, input.password) };
    } finally { created.free(); recoveryPreparation.free(); recoveryPreparation = undefined; openedRecoveryMaterial = undefined; recoveryRequest = undefined; }
  }
  if (input.op === 'prepare_node_enrollment') {
    assert(!nodeEnrollment && !nodeEnrollmentCommit, 'the original Node enrollment must finish before preparing another');
    const auth = authorizations.get(input.authority_connection || 'main');
    assert(auth && trust && openedRecoveryMaterial, 'Node enrollment requires current verified authority and local root material');
    const now = Date.now();
    const claim = crypto.verifyNodeClaim(JSON.stringify(input.claim), input.node_public_key, input.claim_nonce, now);
    try {
      nodeEnrollment = crypto.prepareAccountNodeEnrollment(
        openedRecoveryMaterial, auth.verified, identity(), claim,
        now + 86_400_000, now,
      );
      return JSON.parse(nodeEnrollment.requestJson());
    } finally { claim.free(); }
  }
  if (input.op === 'prepare_node_enrollment_commit') {
    assert(nodeEnrollment && !nodeEnrollmentCommit, 'Node enrollment commit requires its original uncommitted preparation');
    nodeEnrollmentCommit = crypto.prepareAccountNodeEnrollmentCommit(
      nodeEnrollment, openedRecoveryMaterial, input.possession_signature, Date.now(),
    );
    return JSON.parse(nodeEnrollmentCommit.requestJson());
  }
  if (input.op === 'finish_node_enrollment') {
    const verified = crypto.finishAccountNodeEnrollment(nodeEnrollmentCommit, JSON.stringify(input.journal), Date.now());
    try {
      trust.pins = JSON.parse(verified.pinsJson());
      return { pins: trust.pins, installation: JSON.parse(verified.installationJson()) };
    } finally {
      verified.free(); nodeEnrollmentCommit.free(); nodeEnrollmentCommit = undefined;
      nodeEnrollment.free(); nodeEnrollment = undefined; openedRecoveryMaterial = undefined;
    }
  }
  if (input.op === 'trust') {
    assert(!trust, 'local account trust is already established');
    trust = structuredClone(input.trust);
    return { saved: true };
  }
  if (input.op === 'verify_task_binding') {
    const verified = JSON.parse(crypto.verifyTaskBinding(JSON.stringify(input.binding), input.authority_public_key, JSON.stringify(input.expected)));
    verifiedBindings.set(bindingKey({ method: 'bind_execution', ...verified.binding }), verified);
    return verified;
  }
  if (input.op === 'check_prepared') return checkedPreparation(input, input.raw_context, input.prepared);
  if (input.op === 'verify_event') {
    const verified = crypto.verifySignedEvent(JSON.stringify(input.envelope), input.space_id, input.resource_id, input.author_public_key);
    try { return { hash: verified.hash, body: JSON.parse(verified.bodyJson()), envelope_context_hash: verified.envelopeContextHash() }; }
    finally { verified.free(); }
  }
  if (input.op === 'connect') {
    gateways.delete(connectionName);
    connections.get(connectionName)?.close();
    authorizations.get(connectionName)?.verified.free(); authorizations.delete(connectionName);
    connections.set(connectionName, await connect(input));
    return { connected: true };
  }
  const connection = connections.get(connectionName);
  if (!connection) throw new Error('no connection');
  if (input.op === 'authenticate') return authenticate(input, connectionName);
  if (input.op === 'attach_gateway') return attachGateway(input, connectionName);
  if (input.op === 'reconnect_gateway') {
    const binding = gatewayBinding(connectionName);
    connection.close();
    const channel = await connect({ ...binding.card.gateway,
      identity: connections.get(binding.sourceConnection).identityName });
    assert.equal(gateways.get(connectionName), binding, 'Gateway selection changed during reconnect');
    connections.set(connectionName, channel);
    return { connected: true, source_ref: binding.sourceRef };
  }
  if (input.op === 'create_device_invitation') {
    const auth = authorizations.get(connectionName);
    assert(auth, 'device invitation requires current manager authorization');
    const selectedTrust = savedTrust(auth.trustRef);
    assert.equal(typeof input.endpoint, 'string', 'device invitation requires an explicit reachable origin');
    const encoded = crypto.createDeviceEnrollmentOffer(identity(connection.identityName), selectedTrust.scope.account_id,
      input.endpoint, connection.nodePublicKey, input.relay === true,
      JSON.stringify(input.rights || []), auth.policy.account_enrollment_expires_at_ms, auth.verified, Date.now());
    const offered = JSON.parse(encoded);
    const published = await identityRequest(connection, 'enrollment_offer_publish', { offer: offered });
    const reference = JSON.parse(crypto.parseDeviceEnrollmentReference(JSON.stringify(published.reference), Date.now()));
    assert.equal(reference.offer_hash, published.offer_hash);
    assert.equal(reference.account_id, selectedTrust.scope.account_id);
    assert.equal(reference.node_public_key, connection.nodePublicKey);
    return reference;
  }
  if (input.op === 'approve_device' || input.op === 'complete_device') {
    const auth = authorizations.get(connectionName);
    assert(auth, 'device approval requires current manager authorization');
    const selectedTrust = savedTrust(auth.trustRef);
    const signer = identity(connection.identityName);
    const reference = input.reference;
    const params = { account_id: reference.account_id, offer_hash: reference.offer_hash, request_id: input.request_id };
    const response = await identityRequest(connection, 'enrollment_offer_get', params);
    const descriptor = crypto.verifyDeviceEnrollmentOffer(JSON.stringify(response.offer), Date.now());
    const request = await identityRequest(connection, 'enrollment_status', params);
    try {
      crypto.verifyDeviceEnrollmentIntentForManager(auth.verified, descriptor, reference.offer_hash,
        JSON.stringify(request.unsigned_intent), request.new_device_public_key, Date.now());
      if (input.op === 'approve_device') {
        const intent = JSON.parse(crypto.signDeviceEnrollmentIntent(JSON.stringify(request.unsigned_intent), signer, auth.verified, Date.now()));
        return await identityRequest(connection, 'enrollment_approve', { ...params, intent });
      }
      assert.equal(request.status, 'possessed', 'new device must prove possession before completion');
      const qualified = crypto.verifyAccountEnrollment(JSON.stringify(request.intent), auth.verified, Date.now());
      let prepared;
      let completed;
      try {
        prepared = crypto.prepareAccountDeviceEnrollment(qualified, JSON.stringify(request.certificate), signer, auth.verified, Date.now());
        const receipt = await identityRequest(connection, 'account_device_commit', { request: JSON.parse(prepared.requestJson()), request_id: input.request_id });
        completed = crypto.finishAccountDeviceChange(prepared, JSON.stringify(receipt.committed), Date.now());
        saveTrust(auth.trustRef, { ...selectedTrust, pins: JSON.parse(completed.pinsJson()) });
        return { completed: true };
      } finally { completed?.free(); prepared?.free(); qualified.free(); }
    } finally { descriptor.free(); }
  }
  if (input.op === 'gateway_pending') return [...gatewayPending.values()].map(pending => ({
    authorization_id: pending.original.body.authorization_id, operation_id: pending.original.body.operation_id
      || pending.original.body.request?.operation_id, state: pending.state }));
  if (input.op === 'check_authorization_host') {
    const auth = gateways.has(connectionName) ? gatewayBinding(connectionName).authorization : authorizations.get(connectionName);
    assert(auth, 'no confirmed authorization');
    return checkedAuthorization(auth.verified, input.expected_host_public_key, savedTrust(auth.trustRef));
  }
  if (input.op === 'read_remote_policy') return readRemotePolicy(input, connectionName);
  if (input.op === 'verify_project_metadata') {
    const auth = gateways.has(connectionName) ? gatewayBinding(connectionName).authorization : authorizations.get(connectionName);
    assert(auth, 'Project metadata requires a confirmed source authorization');
    return JSON.parse(crypto.verifyProjectExecutionMetadata(auth.verified, JSON.stringify(input.source), JSON.stringify(input.binding)));
  }
  if (input.op === 'sign_observation_request') {
    assert(!gateways.has(connectionName), 'observations use the explicitly selected direct B connection');
    assert.equal(connection.nodePublicKey, authorizations.get(connectionName)?.policy.audience_public_key,
      'observation connection must reach the confirmed Project host');
    return JSON.parse(currentRemote(input, connectionName).signObservationRequest(JSON.stringify(input.original),
      input.authorization_id, connection.nodePublicKey, identity(connection.identityName), Date.now()));
  }
  if (input.op === 'verify_observation') return JSON.parse(currentRemote(input, connectionName).observationJson(
    JSON.stringify(input.actual), JSON.stringify(input.binding), Date.now()));
  if (input.op === 'resolution_body') return JSON.parse(currentRemote(input, connectionName).resolutionBodyJson(
    JSON.stringify(input.actual), input.authorization_id, input.outcome, input.evidence, Date.now()));
  if (input.op === 'verify_resolution_result') return JSON.parse(currentRemote(input, connectionName).resolutionResultJson(
    JSON.stringify(input.actual), JSON.stringify(input.original.body), JSON.stringify(input.original.invocation), Date.now()));
  if (input.op === 'verify_remote_result') {
    const remote = remoteAuthorizations.get(input.handle);
    assert(remote, 'result verification requires its original remote authorization');
    return JSON.parse(remote.remote.resultJson(JSON.stringify(input.actual), JSON.stringify(input.original.body),
      JSON.stringify(input.original.invocation), JSON.stringify(input.binding), Date.now()));
  }
  if (input.op === 'verify_remote_session') {
    const remote = remoteAuthorizations.get(input.handle);
    assert(remote, 'session verification requires its original remote authorization');
    return JSON.parse(remote.remote.sessionJson(JSON.stringify(input.receipt), JSON.stringify(input.binding), Date.now()));
  }
  if (input.op === 'read_share_policy') return readSharePolicy(input, connectionName);
  if (input.op === 'build_share_intent') {
    const auth = authorizations.get(connectionName);
    assert(auth, 'source request needs a confirmed Project authorization');
    return JSON.parse(crypto.buildRemoteShareIntent(JSON.stringify(input.intent), auth.verified,
      identity(connection.identityName), Date.now()));
  }
  if (input.op === 'prepare_governance') {
    const auth = authorizations.get(connectionName);
    assert(auth, 'governance needs a confirmed source authorization');
    const signer = identity(connection.identityName);
    const builders = { resource: 'prepareSpaceResourceRegistration', membership: 'prepareSpaceMembershipChange',
      grant: 'prepareDirectionalGrant' };
    assert(builders[input.kind], 'unknown governance intent');
    return retainPreparation(input, 'governance', crypto[builders[input.kind]](JSON.stringify(input.intent), signer, auth.verified, Date.now()), auth.trustRef);
  }
  if (input.op === 'prepare_revoke_grant') {
    const auth = authorizations.get(connectionName);
    assert(auth, 'grant revocation needs a confirmed owner authorization');
    return retainPreparation(input, 'governance', crypto.prepareSpaceGrantRevocation(auth.scope.space_id,
      input.grant, identity(connection.identityName), auth.verified, Date.now()), auth.trustRef);
  }
  if (input.op === 'prepare_team') {
    const auth = authorizations.get(connectionName);
    assert(auth, 'Team creation needs a confirmed personal Project authorization');
    if (!keys.has(input.handle)) keys.set(input.handle, crypto.Identity.generate());
    return retainPreparation(input, 'team', crypto.prepareTeamCreation(identity(input.handle), identity(connection.identityName),
      auth.scope.account_id, connection.nodePublicKey, identity('recovery').publicKeyHex(), input.source_project,
      auth.verified, Date.now() + 86_400_000, Date.now()), input.trust_ref);
  }
  if (input.op === 'prepare_execution_service') {
    const auth = authorizations.get(connectionName);
    const owner = authorizations.get(input.owner_connection || connectionName);
    assert(auth && owner && openedRecoveryMaterial, 'registration needs current Project and owner authorizations plus private recovery material');
    let cursor = null;
    for (;;) {
      const page = crypto.selectExecutionServiceManagement(auth.verified, JSON.stringify(input.source_project), connection.nodePublicKey, JSON.stringify(cursor), Date.now());
      try {
        const metadata = JSON.parse(page.metadataJson());
        const management = page.selection();
        if (management) {
          try {
            return retainPreparation(input, 'execution_service', crypto.prepareExecutionServiceRegistration(auth.verified,
              management, owner.verified, openedRecoveryMaterial, identity(connection.identityName), connection.nodePublicKey,
              Date.now(), Date.now() + 86_400_000), input.trust_ref);
          } finally { management.free(); }
        }
        assert(metadata.next_cursor && metadata.next_cursor !== cursor, 'Project has no execution management grant');
        cursor = metadata.next_cursor;
      } finally { page.free(); }
    }
  }
  if (input.op === 'prepare_share') {
    const auth = authorizations.get(connectionName);
    const source = preparations.get(input.source_handle);
    assert(auth && source?.kind === 'share_source', 'share needs a confirmed execution owner and native source import');
    return retainPreparation(input, 'share', crypto.prepareRemoteExecutionShare(JSON.stringify(input.intent),
      identity(connection.identityName), auth.verified, source.prepared, Date.now()), auth.trustRef);
  }
  if (input.op === 'finish_preparation') {
    const pending = preparations.get(input.handle);
    assert(pending && pending.kind !== 'share_source', 'retain the original native preparation');
    const finish = { governance: 'finishSpaceGovernanceChange', team: 'finishTeamCreation',
      execution_service: 'finishExecutionServiceRegistration', share: 'finishRemoteExecutionShare' }[pending.kind];
    const verified = crypto[finish](pending.prepared, JSON.stringify(input.receipt), Date.now());
    try {
      const scope = JSON.parse(verified.scopeJson());
      assert(scope, 'committed policy removed this verification actor');
      const pins = JSON.parse(verified.pinsJson());
      saveTrust(pending.trustRef, { scope, pins });
      const result = { trust_ref: pending.trustRef, scope, pins };
      if (pending.kind === 'team') result.resource_id = verified.projectResourceId();
      if (pending.kind === 'execution_service') {
        result.resource_id = verified.resourceId();
        const auth = verified.authorization();
        try { result.grant_leaf = JSON.parse(auth.policyJson()).grant_leaf; } finally { auth.free(); }
      }
      if (pending.kind === 'share') result.share = JSON.parse(verified.referenceJson());
      if (input.password && typeof verified.recoveryMaterialJson === 'function') result.recovery_envelope = sealedRecovery(verified.recoveryMaterialJson(), input.password);
      pending.prepared.free(); preparations.delete(input.handle);
      return result;
    } finally { verified.free(); }
  }
  if (input.op === 'request') {
    if (gateways.has(connectionName) && input.method === 'rpc') {
      assert(input.invocation, 'Gateway business calls need the original signed invocation');
      return forwardGateway(connectionName, { kind: input.params.method === 'operation' ? 'status' : 'invoke',
        body: input.params, invocation: input.invocation });
    }
    return connection.request(input.method, input.params || {}, input.invocation);
  }
  if (input.op === 'signed_request' || input.op === 'prepare_signed') {
    const binding = gateways.has(connectionName) ? gatewayBinding(connectionName) : null;
    const auth = binding?.authorization || authorizations.get(connectionName);
    if (!auth) throw new Error('authenticate before signing a business request');
    assert(!binding || !gatewayPending.has(JSON.stringify([connectionName, input.body.authorization_id])),
      'Gateway request outcome is unknown; preserve its original signature and reconcile first');
    assert.equal(input.body.space_id, auth.scope.space_id);
    const action = expectedAction(input.body);
    const rawContext = input.raw_context || {
      context: {
        conversation: input.body.conversation_id ?? null, task: null,
        sdk_session: null, execution_session: null, executor_kind: null,
        workspace_resource: input.body.resource_id,
      },
      binding_generation: null, canonical_cwd: null, execution_target_public_key: null,
    };
    assert(input.raw_context || (!input.body.task_id && input.body.method !== 'operate'), 'task calls require independently verified execution context');
    const body = JSON.stringify(input.body);
    const remote = input.remote_handle ? remoteAuthorizations.get(input.remote_handle) : null;
    assert(!input.remote_handle || remote && remote.connectionName === connectionName && remote.source === auth.verified,
      'remote authorization belongs to this exact confirmed source session');
    const signer = identity(connection.identityName);
    const intent = JSON.parse(remote ? remote.remote.prepare(auth.verified, body, JSON.stringify(rawContext), action,
      input.body.authorization_id, connection.nodePublicKey, signer, Date.now()) : crypto.signClientPrepareIntent(
      auth.verified, body, JSON.stringify(rawContext), action,
      input.body.authorization_id, connection.nodePublicKey, signer, Date.now(),
    ));
    const prepared = binding ? await forwardGateway(connectionName, { kind: 'prepare', body: input.body, intent })
      : await connection.request('prepare_invocation', { body: input.body, intent });
    if (prepared.error) return prepared;
    const checked = checkedPreparation(input, rawContext, prepared.result);
    const template = JSON.stringify(prepared.result.invocation);
    const finalBody = JSON.stringify(checked.body);
    const invocation = JSON.parse(remote ? remote.remote.sign(auth.verified, template, finalBody, action,
      input.body.authorization_id, connection.nodePublicKey, signer, Date.now()) : binding || auth.forwarded
      ? crypto.signForwardedClientInvocation(auth.verified, template, finalBody, action, input.body.authorization_id, connection.nodePublicKey, signer, Date.now())
      : crypto.signClientInvocation(auth.verified, template, finalBody, action, input.body.authorization_id, signer, Date.now()));
    if (input.op === 'prepare_signed') return { body: checked.body, invocation };
    return binding ? forwardGateway(connectionName, { kind: checked.body.method === 'operation' ? 'status' : 'invoke',
      body: checked.body, invocation }) : connection.request(input.method || 'rpc', checked.body, invocation);
  }
  if (input.op === 'events') return connection.events;
  if (input.op === 'wire') return connection.frames.map(frame => frame.toString('hex'));
  if (input.op === 'attack') return connection.attack(input.kind);
  if (input.op === 'close') { connection.close(); return { closed: true }; }
  throw new Error('unknown client command');
}

for await (const line of createInterface({ input: process.stdin })) {
  try { process.stdout.write(`${JSON.stringify({ ok: true, result: await command(JSON.parse(line)) })}\n`); }
  catch (error) { process.stdout.write(`${JSON.stringify({ ok: false, error: String(error), ...(error.rpcMethod ? { rpc_method: error.rpcMethod } : {}) })}\n`); }
}
for (const connection of connections.values()) connection.close();
for (const auth of authorizations.values()) auth.verified.free();
for (const pending of preparations.values()) pending.prepared.free();
for (const pending of gatewayPending.values()) pending.handle.free();
for (const remote of remoteAuthorizations.values()) remote.remote.free();
browserEnrollment?.pending?.release();
browserEnrollment?.channel?.close();
browserEnrollment?.transport.lockClient();
creation?.free();
recoveryPreparation?.free();
nodeEnrollment?.free();
nodeEnrollmentCommit?.free();
for (const key of keys.values()) key.free();
