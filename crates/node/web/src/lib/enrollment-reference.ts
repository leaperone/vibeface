import { validEndpoint, validTarget } from '../../transport.js'
import { integer, record, text } from './node'

export type EnrollmentReference = {
  version: 2
  kind: 'vibeface-device-enrollment'
  endpoint: string
  node_public_key: string
  account_id: string
  offer_hash: string
  expires_at_ms: number
}

export function parseEnrollmentReference(value: unknown): EnrollmentReference {
  const input = record(value, '设备邀请链接')
  if (Object.keys(input).sort().join(',') !== 'account_id,endpoint,expires_at_ms,kind,node_public_key,offer_hash,version'
    || input.version !== 2 || input.kind !== 'vibeface-device-enrollment') throw new Error('设备邀请链接格式无效。请从自己的已授权设备重新生成。')
  const accountId = text(input.account_id), hash = text(input.offer_hash), expires = integer(input.expires_at_ms)
  if (!/^acc2_[a-f0-9]{64}$/.test(accountId) || !/^h2_[a-f0-9]{64}$/.test(hash)) throw new Error('设备邀请的账户或摘要无效。')
  if (expires <= Date.now()) throw new Error('设备邀请已过期，请在原设备上重新生成。')
  return { version: 2, kind: 'vibeface-device-enrollment', endpoint: validEndpoint(text(input.endpoint)), node_public_key: validTarget(input.node_public_key), account_id: accountId, offer_hash: hash, expires_at_ms: expires }
}

export function enrollmentHref(currentHref: string, reference: EnrollmentReference) {
  const source = new URL(currentHref)
  const url = new URL(source.origin + source.pathname)
  const bytes = new TextEncoder().encode(JSON.stringify(reference))
  const encoded = btoa(String.fromCharCode(...bytes)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
  url.hash = new URLSearchParams({ enroll: encoded }).toString()
  return url.href
}
