import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
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
const verifiedBindings = new Map();
let creation;
let trust;
let openedRecoveryMaterial;
let recoveryRequest;
let recoveryPreparation;
let nodeEnrollment;
let nodeEnrollmentCommit;
let crossNodeRequest;
let crossNodePreparation;
let crossNodeCommit;
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
    events, frames, nodePublicKey: input.node_public_key,
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

function installAuthorization(response, connection, expectedHostPublicKey) {
  if (!trust) throw new Error('authorization requires independently saved pins');
  const verified = crypto.verifyClientAuthorization(JSON.stringify(response.authorization), JSON.stringify(trust.pins), trust.scope.account_id, trust.scope.space_id, identity().publicKeyHex(), Date.now());
  try {
    const scope = JSON.parse(verified.scopeJson());
    const policy = JSON.parse(verified.policyJson());
    assert.deepEqual(response.scope, scope);
    const gatewayPublicKey = connections.get(connection).nodePublicKey;
    assert.equal(policy.audience_public_key, expectedHostPublicKey || gatewayPublicKey);
    const resources = policy.resources;
    assert(Array.isArray(resources));
    assert.deepEqual(response.resources, resources);
    authorizations.get(connection)?.verified.free();
    authorizations.set(connection, { verified, scope, policy, bundle: structuredClone(response.authorization), forwarded: policy.audience_public_key !== gatewayPublicKey });
    trust.pins = trust.pins.map(pin => ({ ...pin, minimum_head: policy.heads.find(head => head.domain.kind === pin.domain.kind && head.domain.id === pin.domain.id) || pin.minimum_head }));
    return { scope, policy, resources };
  } catch (error) { verified.free(); throw error; }
}

async function command(input) {
  const connectionName = input.connection || 'main';
  if (input.op === 'identity') return exposed(identity(input.identity));
  if (input.op === 'derive_authorization_id') return crypto.deriveAuthorizationId(input.kind, input.value);
  if (input.op === 'sign') return identity(input.identity).sign(input.domain, encoder.encode(typeof input.payload === 'string' ? input.payload : JSON.stringify(input.payload)));
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
    const now = Date.now();
    const claim = crypto.verifyNodeClaim(JSON.stringify(input.claim), input.node_public_key, recoveryRequest.payload.nonce, now);
    try {
      recoveryPreparation?.free();
      recoveryPreparation = crypto.prepareAccountRecovery(openedRecoveryMaterial, JSON.stringify(input.authorization), identity('root'), identity(), claim, now + 86_400_000, now);
      return JSON.parse(recoveryPreparation.nodeRequestJson());
    } finally { claim.free(); }
  }
  if (input.op === 'finish_recovery') {
    const created = crypto.finishAccountRecovery(recoveryPreparation, identity('root'), input.proof, Date.now());
    try {
      const material = crypto.buildRecoveredAccountRecoveryMaterial(created, identity('root'), openedRecoveryMaterial, input.endpoint, Date.now());
      trust = { pins: JSON.parse(created.pinsJson()), scope: JSON.parse(created.scopeJson()) };
      return { pins: trust.pins, scope: trust.scope, authorization: JSON.parse(created.bundleJson()),
        commit: JSON.parse(recoveryPreparation.commitJson(created)), recovery_envelope: sealedRecovery(material, input.password) };
    } finally { created.free(); recoveryPreparation.free(); recoveryPreparation = undefined; openedRecoveryMaterial = undefined; recoveryRequest = undefined; }
  }
  if (input.op === 'prepare_node_enrollment') {
    nodeEnrollment?.free(); nodeEnrollment = undefined;
    nodeEnrollmentCommit?.free(); nodeEnrollmentCommit = undefined;
    const auth = authorizations.get(input.authority_connection || 'main');
    assert(auth && trust && openedRecoveryMaterial, 'Node enrollment requires current verified authority and local root material');
    const now = Date.now();
    const claim = crypto.verifyNodeClaim(JSON.stringify(input.claim), input.node_public_key, input.claim_nonce, now);
    try {
      nodeEnrollment = crypto.prepareAccountNodeEnrollment(
        openedRecoveryMaterial, JSON.stringify(trust.pins), JSON.stringify(auth.bundle), identity(), claim,
        now + 86_400_000, now,
      );
      return JSON.parse(nodeEnrollment.requestJson());
    } finally { claim.free(); }
  }
  if (input.op === 'prepare_node_enrollment_commit') {
    nodeEnrollmentCommit?.free(); nodeEnrollmentCommit = undefined;
    nodeEnrollmentCommit = crypto.prepareAccountNodeEnrollmentCommit(
      nodeEnrollment, openedRecoveryMaterial, input.possession_signature, Date.now(),
    );
    return JSON.parse(nodeEnrollmentCommit.requestJson());
  }
  if (input.op === 'finish_node_enrollment') {
    const verified = crypto.finishAccountNodeEnrollment(nodeEnrollmentCommit, JSON.stringify(input.journal), Date.now());
    try {
      trust.pins = JSON.parse(verified.pinsJson());
      return { pins: trust.pins, authorization: JSON.parse(verified.bundleJson()), installation: JSON.parse(verified.installationJson()) };
    } finally {
      verified.free(); nodeEnrollmentCommit.free(); nodeEnrollmentCommit = undefined;
      nodeEnrollment.free(); nodeEnrollment = undefined; openedRecoveryMaterial = undefined;
    }
  }
  if (input.op === 'cross_recovery_sources') {
    return JSON.parse(crypto.inspectCrossNodeRecoverySources(openedRecoveryMaterial, Date.now()));
  }
  if (input.op === 'build_cross_recovery') {
    assert(openedRecoveryMaterial, 'cross-Node recovery requires locally opened recovery material');
    assert(!crossNodeRequest && !crossNodePreparation && !crossNodeCommit, 'cross-Node recovery already has an original request');
    const now = Date.now();
    const claim = crypto.verifyNodeClaim(JSON.stringify(input.claim), input.node_public_key, input.claim_nonce, now);
    try {
      const resources = input.source_projects || JSON.parse(crypto.inspectCrossNodeRecoverySources(openedRecoveryMaterial, now)).source_projects;
      crossNodeRequest = JSON.parse(crypto.buildCrossNodeRecoveryRequest(
        openedRecoveryMaterial, identity('root'), identity(), claim, JSON.stringify(resources),
        now + 3_600_000, now, Math.min(now + 60_000, input.claim.payload.expires_at_ms),
      ));
      return crossNodeRequest;
    } finally { claim.free(); }
  }
  if (input.op === 'prepare_cross_recovery') {
    assert(crossNodeRequest && !crossNodePreparation && !crossNodeCommit, 'cross-Node preparation requires the saved original request');
    const now = Date.now();
    const verified = crypto.verifyCrossNodeRecoveryContext(JSON.stringify(input.context), JSON.stringify(crossNodeRequest), now);
    try {
      crossNodePreparation = crypto.prepareCrossNodeAccountRecovery(
        openedRecoveryMaterial, verified, identity('root'), identity(), now + 86_400_000, now,
      );
      return JSON.parse(crossNodePreparation.possessionRequestJson());
    } finally { verified.free(); }
  }
  if (input.op === 'finish_cross_preparation') {
    assert(crossNodePreparation && !crossNodeCommit, 'cross-Node completion requires the saved preparation');
    const now = Date.now();
    crossNodeCommit = crypto.finishCrossNodeAccountRecovery(
      crossNodePreparation, identity('root'), input.target_proof, input.source_proof, now,
    );
    const pending = crossNodeCommit.recoveryMaterialJson(identity('root'), openedRecoveryMaterial, input.endpoint, now);
    return { request: JSON.parse(crossNodeCommit.requestJson()), review: JSON.parse(crossNodeCommit.publicReviewJson()),
      pending_envelope: sealedRecovery(pending, input.password) };
  }
  if (input.op === 'restore_cross_preparation') {
    assert(connections.size === 0 && !crossNodeRequest && !crossNodePreparation && !crossNodeCommit,
      'pending cross-Node recovery restores before any new connection or request');
    const plaintext = crypto.recoveryOpen(Uint8Array.from(Buffer.from(input.pending_envelope, 'hex')), input.password);
    try {
      crossNodeCommit = crypto.restorePreparedCrossNodeRecoveryCommit(decoder.decode(plaintext), Date.now());
      const client = crossNodeCommit.clientIdentity();
      keys.get('client').free(); keys.set('client', client);
      return { request: JSON.parse(crossNodeCommit.requestJson()), review: JSON.parse(crossNodeCommit.publicReviewJson()),
        resume: JSON.parse(crossNodeCommit.resumeContextJson()), identity: exposed(client) };
    } finally { plaintext.fill(0); }
  }
  if (input.op === 'finish_cross_recovery') {
    assert(crossNodeCommit, 'cross-Node recovery requires the saved original commit preparation');
    const now = Date.now();
    const verified = crypto.finishCrossNodeRecoveryCommit(crossNodeCommit, JSON.stringify(input.committed), now);
    try {
      const pins = JSON.parse(verified.pinsJson());
      const scope = JSON.parse(verified.scopeJson());
      const review = JSON.parse(crossNodeCommit.publicReviewJson());
      const sourceReadsCurrent = scope !== null && now < review.source_read_expires_at_ms;
      const result = { pins, scope, authorization: JSON.parse(verified.bundleJson()),
        installation: JSON.parse(verified.installationJson()),
        source_reads_current: sourceReadsCurrent,
        source_read_selections: sourceReadsCurrent ? JSON.parse(verified.sourceReadSelectionsJson()) : null,
        recovery_envelope: sealedRecovery(verified.finalRecoveryMaterialJson(input.endpoint, now), input.password) };
      trust = scope ? { pins, scope } : undefined;
      crossNodeCommit.free(); crossNodeCommit = undefined;
      crossNodePreparation?.free(); crossNodePreparation = undefined;
      crossNodeRequest = undefined; openedRecoveryMaterial = undefined;
      return result;
    } finally { verified.free(); }
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
    connections.get(connectionName)?.close();
    authorizations.get(connectionName)?.verified.free(); authorizations.delete(connectionName);
    connections.set(connectionName, await connect(input));
    return { connected: true };
  }
  const connection = connections.get(connectionName);
  if (!connection) throw new Error('no connection');
  if (input.op === 'authorize') return installAuthorization(input.response, connectionName, input.expected_host_public_key);
  if (input.op === 'request') return connection.request(input.method, input.params || {}, input.invocation);
  if (input.op === 'signed_request' || input.op === 'prepare_signed') {
    const auth = authorizations.get(connectionName);
    if (!auth) throw new Error('authenticate before signing a business request');
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
    const intent = JSON.parse(crypto.signClientPrepareIntent(
      auth.verified, body, JSON.stringify(rawContext), action,
      input.body.authorization_id, connection.nodePublicKey, identity(), Date.now(),
    ));
    const prepared = await connection.request('prepare_invocation', { body: input.body, intent });
    if (prepared.error) return prepared;
    const checked = checkedPreparation(input, rawContext, prepared.result);
    const template = JSON.stringify(prepared.result.invocation);
    const finalBody = JSON.stringify(checked.body);
    const invocation = JSON.parse(auth.forwarded
      ? crypto.signForwardedClientInvocation(auth.verified, template, finalBody, action, input.body.authorization_id, connection.nodePublicKey, identity(), Date.now())
      : crypto.signClientInvocation(auth.verified, template, finalBody, action, input.body.authorization_id, identity(), Date.now()));
    if (input.op === 'prepare_signed') return { body: checked.body, invocation };
    return connection.request(input.method || 'rpc', checked.body, invocation);
  }
  if (input.op === 'events') return connection.events;
  if (input.op === 'wire') return connection.frames.map(frame => frame.toString('hex'));
  if (input.op === 'attack') return connection.attack(input.kind);
  if (input.op === 'close') { connection.close(); return { closed: true }; }
  throw new Error('unknown client command');
}

for await (const line of createInterface({ input: process.stdin })) {
  try { process.stdout.write(`${JSON.stringify({ ok: true, result: await command(JSON.parse(line)) })}\n`); }
  catch (error) { process.stdout.write(`${JSON.stringify({ ok: false, error: String(error) })}\n`); }
}
for (const connection of connections.values()) connection.close();
for (const auth of authorizations.values()) auth.verified.free();
creation?.free();
recoveryPreparation?.free();
nodeEnrollment?.free();
nodeEnrollmentCommit?.free();
crossNodePreparation?.free();
crossNodeCommit?.free();
for (const key of keys.values()) key.free();
