import { validTarget } from '../lib/transport.ts'
import type { MessageRequest } from '../chat/types.ts'
import type { Avatar } from '../avatar/face.ts'
import type { BotProject, CreateBotRequest, MemoryBase, ScheduleAction, WorkspaceCommand } from './types.ts'
import { record, text } from '../lib/node.ts'
import { parseCreateBot, parseMessageRequest, parseWorkspaceCommand, avatar } from './model.ts'
import { parseMemoryRequest } from './memory.ts'
import { inputEventBody } from '../chat/protocol.ts'
import { answerBody } from './questions.ts'

export type Attestation = { device_id: string; signature: string }
export type StoredWorkspaceCommand = Exclude<WorkspaceCommand, { kind: 'post_channel_message' }> | (Omit<Extract<WorkspaceCommand, { kind: 'post_channel_message' }>, 'attachments'> & { attachments?: Extract<WorkspaceCommand, { kind: 'post_channel_message' }>['attachments'] })
export type WriteRequests = {
  message: MessageRequest
  workspace_command: { command_id: string; command: StoredWorkspaceCommand }
  control_memory: { conversation_id: string; memory_id: string; base: MemoryBase; after: string | null; operation_id: string }
  ack: { notification_id: string }
  create_bot: CreateBotRequest & { scheduler_node: string; owner_attestation: Attestation }
  delete_bot: { conversation_id: string }
  set_primary_bot: { conversation_id: string }
  set_bot_project: { conversation_id: string; project: BotProject | null }
  set_bot_avatar: { conversation_id: string; version: string | null; avatar: Avatar | null }
  control_schedule: { conversation_id: string; schedule_id: string; action: ScheduleAction }
  set_owner: { conversation_id: string; scheduler_node: string; epoch: number }
  dismiss_work: { conversation_id: string; work_id: string }
  answer: { question_id: string; text: string }
  resolve: { conversation_id: string; check_id: string; outcome: 'completed' | 'interrupted'; note: string }
}
export type WriteMethod = keyof WriteRequests
export type SignedOperation = { [M in WriteMethod]: { method: M; params: SignedRequest<M> } }[WriteMethod]
export type SignedRequest<M extends WriteMethod> = WriteRequests[M] & { attestation: Attestation }
export function parseAttestation(value: unknown): Attestation {
  const data = record(value), signature = text(data.signature)
  if (!/^[a-f0-9]{128}$/.test(signature)) throw new Error('invalid signature')
  return { device_id: validTarget(data.device_id), signature }
}
/** Narrow request projections, shared by the writer and persistence decoder. */
export function parseWriteRequest(method: WriteMethod, value: unknown): WriteRequests[WriteMethod] {
  const p = record(value)
  switch (method) {
    case 'message': return parseMessageRequest(p)
    case 'workspace_command': {
      const parsed = parseWorkspaceCommand(p.command)
      // The protocol omits an empty attachment list in this command's signed body.
      const command = parsed.kind === 'post_channel_message' && !parsed.attachments.length && record(p.command).attachments === undefined ? (({ attachments: _, ...rest }) => rest)(parsed) : parsed
      return { command_id: validTarget(p.command_id), command }
    }
    case 'control_memory': {
      const { attestation: _, ...request } = parseMemoryRequest(p)
      return request
    }
    case 'ack': return { notification_id: text(p.notification_id) }
    case 'create_bot': return { ...parseCreateBot(p), scheduler_node: validTarget(p.scheduler_node), owner_attestation: parseAttestation(p.owner_attestation) }
    case 'delete_bot': case 'set_primary_bot': return { conversation_id: text(p.conversation_id) }
    case 'set_bot_project': {
      const project = p.project === null ? null : record(p.project)
      return { conversation_id: text(p.conversation_id), project: project && { target_node: validTarget(project.target_node), cwd: text(project.cwd) } }
    }
    case 'set_bot_avatar': return { conversation_id: text(p.conversation_id), version: p.version === null ? null : text(p.version), avatar: p.avatar === null ? null : avatar(record(p.avatar)) }
    case 'control_schedule': {
      if (p.action !== 'pause' && p.action !== 'resume' && p.action !== 'delete') throw new Error('invalid schedule action')
      return { conversation_id: text(p.conversation_id), schedule_id: text(p.schedule_id), action: p.action }
    }
    case 'set_owner': {
      if (typeof p.epoch !== 'number' || !Number.isSafeInteger(p.epoch) || p.epoch < 1) throw new Error('invalid epoch')
      return { conversation_id: text(p.conversation_id), scheduler_node: validTarget(p.scheduler_node), epoch: p.epoch }
    }
    case 'dismiss_work': return { conversation_id: text(p.conversation_id), work_id: text(p.work_id) }
    case 'answer': return { question_id: text(p.question_id), text: text(p.text) }
    case 'resolve': {
      if (p.outcome !== 'completed' && p.outcome !== 'interrupted' || !text(p.check_id).startsWith('work:') || typeof p.note !== 'string' || new TextEncoder().encode(p.note).length > 4096) throw new Error('invalid resolution')
      return { conversation_id: text(p.conversation_id), check_id: text(p.check_id), outcome: p.outcome, note: p.note }
    }
  }
}
export type WriteOperation = { [M in WriteMethod]: { method: M; params: WriteRequests[M] } }[WriteMethod]
/** The exact production event body; callers sign and verify this same projection. */
export function writeBody(operation: WriteOperation, clientId: string): Record<string, unknown> {
  switch (operation.method) {
    case 'message': return inputEventBody(operation.params)
    case 'workspace_command': return { type: 'workspace_commanded', command_id: operation.params.command_id, command: operation.params.command }
    case 'control_memory': return { type: 'memory_controlled', memory_id: operation.params.memory_id, base: operation.params.base, after: operation.params.after }
    case 'ack': return { type: 'notification_acked', notification_id: operation.params.notification_id, client_id: clientId }
    case 'create_bot': { const p = operation.params; return { type: 'bot_created', bot_id: p.bot_id, name: p.name, instructions: p.instructions, ...(p.project ? { project: p.project } : {}) } }
    case 'delete_bot': return { type: 'bot_deleted' }
    case 'set_primary_bot': return { type: 'primary_bot_set', conversation_id: operation.params.conversation_id }
    case 'set_bot_project': return { type: 'bot_project_set', project: operation.params.project }
    case 'set_bot_avatar': return { type: 'bot_avatar_set', version: operation.params.version, avatar: operation.params.avatar }
    case 'control_schedule': return { type: 'schedule_controlled', schedule_id: operation.params.schedule_id, action: operation.params.action }
    case 'set_owner': return ownerBody(operation.params.scheduler_node, operation.params.epoch)
    case 'dismiss_work': return { type: 'notification_acked', notification_id: operation.params.work_id, client_id: 'inbox' }
    case 'answer': return answerBody(operation.params.question_id, operation.params.text)
    case 'resolve': return { type: 'work_finished', work_id: operation.params.check_id.slice(5), outcome: operation.params.outcome, result_text: operation.params.note }
  }
}
export const ownerBody = (scheduler_node: string, epoch: number) => ({ type: 'conversation_owner', scheduler_node, epoch })
export function writeKeySuffix(operation: WriteOperation): string {
  switch (operation.method) {
    case 'message': return `message:${operation.params.message_id}`
    case 'workspace_command': return `workspace-command:${operation.params.command_id}`
    case 'control_memory': return `memory:${JSON.stringify([operation.params.conversation_id, operation.params.memory_id])}`
    case 'ack': return `ack:${operation.params.notification_id}`
    case 'create_bot': return `bot:${operation.params.bot_id}`
    case 'delete_bot': return `bot-delete:${operation.params.conversation_id}`
    case 'set_primary_bot': return `primary:${operation.params.conversation_id}`
    case 'set_bot_project': return `bot-project:${operation.params.conversation_id}:${operation.params.project?.target_node ?? ''}:${operation.params.project?.cwd ?? ''}`
    case 'set_bot_avatar': return `bot-avatar:${operation.params.conversation_id}`
    case 'control_schedule': return `schedule:${operation.params.schedule_id}:${operation.params.action}`
    case 'set_owner': return `owner:${operation.params.conversation_id}:${operation.params.epoch}`
    case 'dismiss_work': return `dismiss:${operation.params.work_id}`
    case 'answer': return `answer:${operation.params.question_id}`
    case 'resolve': return `resolve:${operation.params.check_id}`
  }
}
