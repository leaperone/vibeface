import assert from 'node:assert/strict'
import { createHash, createPublicKey, generateKeyPairSync, sign, verify } from 'node:crypto'
import test from 'node:test'
import type { JoinRequest } from '../lib/node.ts'
import { approvalMatches, joinMatches, parseApprovalPins, parsePendingJoin, parseWaitRecord, pendingJoinJson, readIdentityRecord, recoveryCheckpointHash, waitRecordJson } from './persistence.ts'

const nodeId = signer().id, deviceId = signer().id, nextRoot = signer().id
const gateway = { address: 'https://node.example', device_id: nodeId }
const inviteId = '4'.repeat(32), nonce = '5'.repeat(64)
const commit = createHash('sha256').update(Buffer.from(deviceId + nonce, 'hex')).digest('hex')
const storedJoin = { entry: gateway, join_id: inviteId, invite_id: inviteId, name: '新设备', nonce }
const pin = { nonce: '6'.repeat(64), device_id: deviceId, invite_id: inviteId, commit }
const encode = new TextEncoder()

test('storage distinguishes absence, unreadable storage and malformed JSON without replacing any value', () => {
  const read = (raw: string | null) => readIdentityRecord({ getItem(key) { assert.equal(key, 'vibeface.pending-join'); return raw } }, 'pending-join', parsePendingJoin)
  assert.deepEqual(read(null), { kind: 'empty' })
  assert.deepEqual(read('null'), { kind: 'empty' })
  assert.deepEqual(read(''), { kind: 'invalid' })
  assert.deepEqual(read('{'), { kind: 'invalid' })
  assert.deepEqual(read('false'), { kind: 'invalid' })
  assert.deepEqual(readIdentityRecord({ getItem() { throw new Error() } }, 'pending-join', parsePendingJoin), { kind: 'unreadable' })
  const value = read(JSON.stringify(storedJoin))
  assert.equal(value.kind, 'valid')
  if (value.kind === 'valid') assert.deepEqual(pendingJoinJson(value.value), storedJoin)
})

test('pending join round trips the original nonce and optional manager pin and rejects non-writer shapes', () => {
  const saved = { ...storedJoin, pinned: { network_id: '7'.repeat(64), admin_id: '8'.repeat(64), admin_nonce: pin.nonce } }
  assert.deepEqual(pendingJoinJson(parsePendingJoin(saved)), saved)
  const lan = { ...storedJoin, entry: { addrs: ['192.168.1.10:4790', '[fd00::1]:4790'], device_id: nodeId } }
  assert.deepEqual(pendingJoinJson(parsePendingJoin(lan)), lan)
  for (const bad of [[], {}, { ...saved, name: undefined }, { ...saved, name: {} }, { ...saved, nonce: '000' }, { ...saved, join_id: '9'.repeat(32) },
    { ...saved, pinned: [] }, { ...saved, pinned: null }, { ...saved, pinned: { ...saved.pinned, admin_nonce: 'Z'.repeat(64) } },
    { ...saved, entry: { address: 'https://node.example/unexpected', device_id: nodeId } }, { ...saved, entry: { ...gateway, local: false } }]) {
    assert.throws(() => parsePendingJoin(bad))
  }
})

test('approval pins are indexed by the exact join id and a damaged entry leaves a valid nonce readable', () => {
  const wrongId = '9'.repeat(32)
  assert.throws(() => parseApprovalPins([pin]))
  assert.equal(parseApprovalPins({}).damaged, false)
  assert.deepEqual([...parseApprovalPins({ [inviteId]: pin }).pins], [[inviteId, pin]])
  for (const bad of [[], { ...pin, nonce: false }, { ...pin, device_id: '9' }, { ...pin, commit: {} }, pin]) {
    const result = parseApprovalPins({ [inviteId]: pin, [wrongId]: bad })
    assert.equal(result.damaged, true)
    assert.deepEqual([...result.pins], [[inviteId, pin]])
  }
  assert.deepEqual([...parseApprovalPins(JSON.parse('{"__proto__":[]}')).pins], [])
})

test('join and manager bindings check the complete local intent before using a nonce', () => {
  const networkId = '7'.repeat(64)
  const request: JoinRequest = { join_id: inviteId, invite_id: inviteId, network_id: networkId, device_id: deviceId, commit, name: '新设备', kind: 'client', status: 'confirming', admin_id: '8'.repeat(64), admin_nonce: pin.nonce, joiner_nonce: nonce, expires_at_ms: 1000, reason: null }
  const pending = parsePendingJoin(storedJoin)
  assert.equal(joinMatches(request, pending, deviceId, commit), true)
  assert.equal(joinMatches({ ...request, join_id: '9'.repeat(32) }, pending, deviceId, commit), false)
  assert.equal(joinMatches({ ...request, joiner_nonce: '9'.repeat(64) }, pending, deviceId, commit), false)
  assert.equal(joinMatches({ ...request, device_id: nodeId }, pending, deviceId, commit), false)
  assert.equal(joinMatches({ ...request, commit: '9'.repeat(64) }, pending, deviceId, commit), false)
  assert.equal(approvalMatches(request, networkId, pin), true)
  assert.equal(approvalMatches({ ...request, network_id: deviceId }, networkId, pin), false)
  assert.equal(approvalMatches({ ...request, invite_id: '9'.repeat(32) }, networkId, pin), false)
})

const signatureMessage = (domain: string, payload: Uint8Array) =>
  Buffer.from(`vibeface:message:v1:${domain}:${createHash('sha256').update(payload).digest('hex')}`)
function signer() {
  const { privateKey, publicKey } = generateKeyPairSync('ed25519')
  return {
    id: publicKey.export({ format: 'der', type: 'spki' }).subarray(-32).toString('hex'),
    sign(domain: string, payload: string) { return sign(null, signatureMessage(domain, encode.encode(payload)), privateKey).toString('hex') },
  }
}

const genesisRoot = signer(), rotatedRoot = signer()
const first_client = { device_id: deviceId, kind: 'client', manage: true, name: '\uFEFF' }
const first_node = { device_id: nodeId, kind: 'node', manage: false, name: '节点' }
const unsignedGenesis = { first_client, first_node, network_id: genesisRoot.id, signature: '', version: 1 }
const genesis = { ...unsignedGenesis, signature: genesisRoot.sign('identity-genesis', JSON.stringify(unsignedGenesis)).toUpperCase() }
const checkpointMembers = [first_node, first_client].sort((a, b) => a.device_id.localeCompare(b.device_id))

function signedCheckpoint(epoch: number, root: ReturnType<typeof signer>) {
  const payload = { directory: { members: checkpointMembers, retired: [] }, epoch, heads: [], issued_at_ms: 2_000, network_id: genesisRoot.id, recovery_key: nextRoot, signature: '', version: 1 }
  return { ...payload, signature: root.sign('identity-recovery', JSON.stringify(payload)) }
}

function verifyFixture(rootKey: string, domain: string, payload: Uint8Array, signature: string) {
  const publicKey = createPublicKey({ key: Buffer.from(`302a300506032b6570032100${rootKey}`, 'hex'), type: 'spki', format: 'der' })
  assert.ok(verify(null, signatureMessage(domain, payload), publicKey, Buffer.from(signature, 'hex')), 'record signature must match the original contents and signer')
}

const waitTrust = {
  deviceId,
  verifyGenesis(record, networkId) {
    const { first_client, first_node, network_id, version, signature } = record
    verifyFixture(networkId, 'identity-genesis', encode.encode(JSON.stringify({ first_client, first_node, network_id, signature: '', version })), signature)
  },
  verify: verifyFixture,
} satisfies Parameters<typeof parseWaitRecord>[1]

test('recovery preserves the signed genesis, exact checkpoint and both optional checkpoint states', async () => {
  const saved = { entry: gateway, networkId: genesisRoot.id, rootKey: genesisRoot.id, genesis }
  assert.deepEqual(waitRecordJson(parseWaitRecord(saved, waitTrust)), saved)
  for (const [epoch, signer] of [[2, genesisRoot], [3, rotatedRoot]] as const) {
    const checkpoint = signedCheckpoint(epoch, signer)
    const original = { ...saved, rootKey: signer.id, checkpoint }
    const wait = parseWaitRecord(JSON.parse(JSON.stringify(original)), waitTrust)
    assert.deepEqual(waitRecordJson(wait), original)
    assert.ok(wait.checkpoint)
    assert.equal(await recoveryCheckpointHash(wait.checkpoint), createHash('sha256').update(JSON.stringify(checkpoint)).digest('hex'))
  }
})

test('recovery refuses malformed content and changes to anchors, signatures and device bindings', () => {
  const checkpoint = signedCheckpoint(2, genesisRoot)
  const saved = { entry: gateway, networkId: genesisRoot.id, rootKey: genesisRoot.id, genesis, checkpoint }
  for (const malformed of [[], { ...saved, entry: { ...gateway, device_id: '7'.repeat(64) } }, { ...saved, networkId: nextRoot },
    { ...saved, rootKey: rotatedRoot.id }, { ...saved, genesis: { ...genesis, signature: '0'.repeat(128) } }, { ...saved, checkpoint: null },
    { ...saved, checkpoint: { ...checkpoint, network_id: nextRoot } }, { ...saved, checkpoint: { ...checkpoint, epoch: 2.5 } },
    { ...saved, checkpoint: { ...checkpoint, issued_at_ms: -1 } }, { ...saved, checkpoint: { ...checkpoint, issued_at_ms: 2_001 } },
    { ...saved, checkpoint: { ...checkpoint, directory: [] } }, { ...saved, checkpoint: { ...checkpoint, heads: ['wrong'] } },
    { ...saved, checkpoint: { ...checkpoint, directory: { members: [...checkpointMembers].reverse(), retired: [] } } },
    { ...saved, checkpoint: { ...checkpoint, directory: { members: [first_node], retired: [] } } }]) {
    assert.throws(() => parseWaitRecord(malformed, waitTrust))
  }
  assert.throws(() => parseWaitRecord(saved, { ...waitTrust, deviceId: '7'.repeat(64) }))
  assert.throws(() => parseWaitRecord({ ...saved, rootKey: genesisRoot.id, checkpoint: signedCheckpoint(3, rotatedRoot) }, waitTrust))
})
