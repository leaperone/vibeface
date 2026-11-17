import { validTarget } from '@vibeface/client/lib/transport.ts'
import { parseMember, record, type Member as MembershipMember } from '@vibeface/client/lib/node.ts'
import { parseBots } from '@vibeface/client/workspace/model.ts'
import type { BotReadiness, BotRecord } from '@vibeface/client/workspace/types.ts'
import type { DisplayScope } from '@vibeface/client/identity/context.tsx'
import type { Member } from './types'

export type LastSeenRecord = DisplayScope & {
  entry_label: string; bots: BotRecord[]; primary: string | null
  members: (MembershipMember & { bot: BotReadiness | null })[]
}
/** A display cache cannot establish its own network or identity. Reject the whole snapshot. */
export function decodeLastSeen(value: unknown, scope: DisplayScope | null): LastSeenRecord | null {
  if (!scope) return null
  try {
    const data = record(value)
    if (validTarget(data.network_id) !== scope.network_id || validTarget(data.device_id) !== scope.device_id || validTarget(data.entry_device_id) !== scope.entry_device_id || typeof data.entry_label !== 'string' || !Array.isArray(data.members)) return null
    const members = data.members.map((value): MembershipMember & { bot: BotReadiness | null } => {
      const member = parseMember(value), bot = record(value).bot
      if (bot !== null && bot !== 'ready' && bot !== 'starting' && bot !== 'unverified' && bot !== 'needs_setup') throw new Error('invalid display record')
      return { ...member, bot }
    })
    if (new Set(members.map(member => member.device_id)).size !== members.length || !members.some(member => member.device_id === scope.device_id && member.kind === 'client') || !members.some(member => member.device_id === scope.entry_device_id && member.kind === 'node')) return null
    const bots = parseBots({ bots: data.bots }).map(bot => ({ ...bot, busy: null }))
    if (new Set(bots.map(bot => bot.bot_id)).size !== bots.length || new Set(bots.map(bot => bot.conversation_id)).size !== bots.length || data.primary !== null && (typeof data.primary !== 'string' || !bots.some(bot => bot.conversation_id === data.primary))) return null
    return { network_id: scope.network_id, device_id: scope.device_id, entry_device_id: scope.entry_device_id, entry_label: data.entry_label, members, bots, primary: data.primary }
  } catch { return null }
}
export function displayLastSeen(record: LastSeenRecord) {
  const members: Member[] = record.members.map(member => ({ device_id: member.device_id, name: member.name, kind: member.kind, admin: member.manage, current: member.device_id === record.device_id, entry: member.device_id === record.entry_device_id, online: undefined, bot: member.bot }))
  return { bots: record.bots, primary: record.primary, entry: record.entry_label, members }
}
export function projectLastSeen(scope: DisplayScope, entryLabel: string, bots: BotRecord[], primary: string | null, members: Member[]): LastSeenRecord | null {
  return decodeLastSeen({ ...scope, entry_label: entryLabel, bots, primary, members: members.map(member => ({ device_id: member.device_id, name: member.name, kind: member.kind, manage: member.admin, bot: member.bot })) }, scope)
}
