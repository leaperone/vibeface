import { validTarget } from '../lib/transport.ts'
import { OutboxCorruptError, type OutboxItem } from '../lib/outbox.ts'
import { record, text } from '../lib/node.ts'
import { base64ToBytes, bytesToBase64, sha256Hex } from '../lib/byte-codec.ts'
import { requestKey } from '../lib/secure.ts'
import type { SecureApi } from '../secure-api.ts'
import { attachmentLimit, parseInputReceipt, parseWorkspaceCommandNotSaved } from './model.ts'
import { parseAttestation, parseWriteRequest, writeBody, writeKeySuffix, ownerBody, type WriteMethod } from './write-request.ts'
import { inputTargetKey } from '../chat/protocol.ts'

const methods: readonly WriteMethod[] = ['message', 'workspace_command', 'control_memory', 'ack', 'create_bot', 'delete_bot', 'set_primary_bot', 'set_bot_project', 'set_bot_avatar', 'control_schedule', 'set_owner', 'dismiss_work', 'answer', 'resolve']
function fail(): never { throw new OutboxCorruptError() }
const own = (value: object, key: string) => Object.prototype.hasOwnProperty.call(value, key)
function keys(data: Record<string, unknown>, required: string[], optional: string[] = []) {
  if (required.some(key => !own(data, key) || data[key] === undefined) || Object.keys(data).some(key => !required.includes(key) && !optional.includes(key))) fail()
}
/** Compare a domain parser's projection without changing a single stored value or key order. */
export function exactProjection(raw: unknown, parsed: unknown): void {
  if (raw === parsed) return
  if (Array.isArray(raw) && Array.isArray(parsed)) {
    if (raw.length !== parsed.length || Object.keys(raw).length !== raw.length || Object.keys(raw).some((key, index) => key !== String(index))) fail()
    raw.forEach((value, index) => exactProjection(value, parsed[index])); return
  }
  if (raw && parsed && typeof raw === 'object' && typeof parsed === 'object' && !Array.isArray(raw) && !Array.isArray(parsed)) {
    const source = record(raw), projected = record(parsed)
    if (Object.keys(source).some(key => !own(projected, key)) || Object.keys(projected).some(key => projected[key] !== undefined && !own(source, key))) fail()
    for (const key of Object.keys(source)) exactProjection(source[key], projected[key])
    return
  }
  fail()
}
const count = (value: unknown): number => { if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) fail(); return value }
function attachment(value: unknown) {
  const file = record(value)
  keys(file, ['hash', 'size', 'name'])
  validTarget(file.hash); text(file.name)
  if (count(file.size) < 1 || Number(file.size) > attachmentLimit) fail()
}
function decode(value: unknown, physicalKey: unknown, nested = false): OutboxItem {
  const item = record(value), method = methods.find(method => method === item.method)
  if (!method) fail()
  const options = ['rejected', ...(method === 'message' ? ['uploads', 'delivery', 'entry_device_id', 'receipt', 'release_expected_event', 'release_command_id', 'rejection_code', 'prerequisite'] : method === 'workspace_command' ? ['command_rejection'] : [])]
  keys(item, ['key', 'method', 'params', 'scope_id', 'created_at_ms', ...(method === 'message' ? ['delivery'] : [])], options)
  const key = text(item.key), scope = text(item.scope_id), p = record(item.params)
  if (!scope || physicalKey !== key || key[64] !== '.' || key[129] !== ':') fail()
  validTarget(key.slice(0, 64))
  const clientId = validTarget(key.slice(65, 129))
  const parsed = { ...parseWriteRequest(method, p), attestation: parseAttestation(p.attestation) }
  exactProjection(p, parsed)
  if (parsed.attestation.device_id !== clientId) fail()
  count(item.created_at_ms)
  if (item.rejected !== undefined && typeof item.rejected !== 'string') fail()
  // The projection comparison above validates every member of this closed discriminated union.
  const result = item as OutboxItem
  if (key.slice(130) !== writeKeySuffix(result)) fail()
  const expectedScope = method === 'workspace_command' ? 'workspace' : method === 'set_primary_bot' ? 'bots' : 'conversation_id' in p ? p.conversation_id : scope
  if (scope !== expectedScope) fail()
  if (result.method === 'create_bot') {
    if (result.params.owner_attestation.device_id !== clientId) fail()
    if (result.params.project) validTarget(result.params.project.target_node)
  }
  if (result.method === 'workspace_command') {
    if (nested && result.prerequisite !== undefined) fail()
    if (result.command_rejection !== undefined) {
      const rejection = parseWorkspaceCommandNotSaved(result.command_rejection, result.params.command_id)
      if (!rejection) fail()
      exactProjection(result.command_rejection, rejection)
    }
  }
  if (result.method === 'message') {
    if (!['saved', 'uploading', 'submitting', 'accepted', 'unknown'].includes(result.delivery)) fail()
    if (result.entry_device_id !== undefined) validTarget(result.entry_device_id)
    if ((result.delivery === 'unknown' || result.delivery === 'submitting') && result.entry_device_id === undefined) fail()
    if (result.rejection_code !== undefined && !['input_not_saved', 'input_conflict', 'input_missing'].includes(result.rejection_code)) fail()
    if (result.rejection_code === 'input_not_saved' && (result.delivery !== 'saved' || result.entry_device_id === undefined)) fail()
    if ((result.rejection_code === 'input_conflict' || result.rejection_code === 'input_missing') && result.delivery !== 'unknown') fail()
    const files = result.params.attachments ?? []
    files.forEach(attachment)
    if (new Set(files.map(file => file.hash)).size !== files.length) fail()
    if (result.uploads !== undefined) {
      if (!Array.isArray(result.uploads)) fail()
      const hashes = new Set<string>()
      for (const file of result.uploads) {
        const raw = record(file); keys(raw, ['hash', 'size', 'name', 'data'])
        attachment({ hash: file.hash, size: file.size, name: file.name })
        const bytes = base64ToBytes(text(file.data))
        if (bytes.length !== file.size || bytesToBase64(bytes) !== file.data || hashes.has(file.hash) || !files.some(signed => signed.hash === file.hash && signed.size === file.size && signed.name === file.name)) fail()
        hashes.add(file.hash)
      }
    }
    if (result.delivery === 'accepted' && result.receipt === undefined) fail()
    if (result.receipt !== undefined) {
      keys(record(result.receipt), ['message_id', 'target', 'stage', 'actual_turn_id', 'reason', 'retry_safe', 'control_event'])
      const receipt = parseInputReceipt(result.receipt)
      exactProjection(result.receipt, receipt)
      if (receipt.message_id !== result.params.message_id || inputTargetKey(receipt.target) !== inputTargetKey(result.params.target)) fail()
      // Local submission has no receipt yet. Remote stages (including submitting and
      // refused) are accepted evidence; a pending release retains its refused receipt.
      if (result.delivery === 'saved' || result.delivery === 'uploading' || result.delivery === 'submitting') fail()
      if (result.delivery === 'accepted' && receipt.stage === 'unknown') fail()
      if (result.delivery === 'unknown' && receipt.stage !== 'unknown' && !(receipt.stage === 'refused' && result.release_expected_event !== undefined)) fail()
    }
    if ((result.release_expected_event === undefined) !== (result.release_command_id === undefined)) fail()
    if (result.release_expected_event !== undefined) { validTarget(result.release_expected_event); validTarget(result.release_command_id); if (result.delivery !== 'unknown' || result.receipt?.stage !== 'refused' || result.receipt.control_event !== result.release_expected_event) fail() }
    if (result.prerequisite !== undefined) {
      const prerequisite = decode(result.prerequisite, result.prerequisite.key, true)
      if (prerequisite.method !== 'workspace_command' || prerequisite.key.slice(0, 130) !== key.slice(0, 130) || prerequisite.params.command.kind !== 'create_task') fail()
      const command = prerequisite.params.command, target = result.params.target
      if ((target.kind !== 'task' && target.kind !== 'channel_task') || target.task_id !== command.task_id || (target.kind === 'channel_task' ? target.channel_id !== command.channel_id || target.bot_id !== command.owner_bot : command.channel_id !== undefined)) fail()
    }
  }
  return result
}
export function decodeOutbox(value: unknown, physicalKey: unknown): OutboxItem {
  try { return decode(value, physicalKey) } catch { return fail() }
}
/** Verify the entire batch before a caller can select or send a single item. */
export async function verifyOutbox(items: OutboxItem[], secure: Pick<SecureApi, 'verify' | 'attestationPayload'>): Promise<void> {
  try {
    if (typeof secure?.verify !== 'function' || typeof secure.attestationPayload !== 'function') fail()
    for (const item of items) {
      const network = item.key.slice(0, 64), client = item.key.slice(65, 129)
      const body = writeBody(item, client)
      const payload = secure.attestationPayload(network, item.scope_id, JSON.stringify(body))
      secure.verify(client, 'attestation', payload, item.params.attestation.signature)
      if (item.method === 'create_bot') secure.verify(client, 'attestation', secure.attestationPayload(network, item.scope_id, JSON.stringify(ownerBody(item.params.scheduler_node, 1))), item.params.owner_attestation.signature)
      if (item.method === 'control_memory' && await requestKey(payload, client) !== item.params.operation_id) fail()
      if (item.method === 'workspace_command' && await requestKey(secure.attestationPayload(network, 'workspace', JSON.stringify({ type: 'workspace_commanded', command: item.params.command })), client) !== item.params.command_id) fail()
      if (item.method === 'message') {
        for (const file of item.uploads ?? []) if (await sha256Hex(base64ToBytes(file.data)) !== file.hash) fail()
        if (item.prerequisite) await verifyOutbox([item.prerequisite], secure)
        if (item.release_command_id !== undefined) {
          const release = { kind: 'control_input', message_id: item.params.message_id, expected_event: item.release_expected_event, action: { kind: 'release' } }
          if (await requestKey(secure.attestationPayload(network, 'workspace', JSON.stringify({ type: 'workspace_commanded', command: release })), client) !== item.release_command_id) fail()
          const command = items.find(candidate => candidate.method === 'workspace_command' && candidate.key.slice(0, 130) === item.key.slice(0, 130) && candidate.params.command_id === item.release_command_id)
          if (command?.method === 'workspace_command' && (command.params.command.kind !== 'control_input' || command.params.command.message_id !== item.params.message_id || command.params.command.expected_event !== item.release_expected_event || command.params.command.action.kind !== 'release')) fail()
        }
      }
    }
  } catch { fail() }
}
