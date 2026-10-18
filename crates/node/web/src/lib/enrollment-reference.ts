import { parseDeviceEnrollmentReference } from '../../generated/vibeface_secure.js'
import { boolean, integer, record, text } from './node'

export type EnrollmentReference = {
  version: 2
  kind: 'vibeface-device-enrollment'
  endpoint: string
  node_public_key: string
  relay: boolean
  account_id: string
  offer_hash: string
  expires_at_ms: number
}

export function parseEnrollmentReference(value: unknown, now = Date.now()): EnrollmentReference {
  const input = record(JSON.parse(parseDeviceEnrollmentReference(JSON.stringify(value), now)), '设备邀请')
  return {
    version: 2, kind: 'vibeface-device-enrollment', endpoint: text(input.endpoint),
    node_public_key: text(input.node_public_key), relay: boolean(input.relay),
    account_id: text(input.account_id), offer_hash: text(input.offer_hash), expires_at_ms: integer(input.expires_at_ms),
  }
}

export function enrollmentHref(currentHref: string, reference: EnrollmentReference) {
  const source = new URL(currentHref)
  const url = new URL(source.origin + source.pathname)
  const bytes = new TextEncoder().encode(JSON.stringify(reference))
  const encoded = btoa(String.fromCharCode(...bytes)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
  url.hash = new URLSearchParams({ enroll: encoded }).toString()
  return url.href
}
