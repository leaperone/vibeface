import { beginClientPolicyImport, buildClientPolicyAuthConfirmRequest, buildClientPolicyReadRequest, verifyClientPolicySelections } from '../../transport.js'
import { confirmClientPolicyAuthorization } from '../../generated/vibeface_secure.js'
import type { VerifiedClientAuthorization } from '../../generated/vibeface_secure.js'
import { parseAuthorizationSelection } from './authorization'
import type { AuthorizationSelection, AuthorizationTrust } from './authorization'
import { typedId } from './authorization-trust'
import { array, record } from './node'

type PolicyRead = {
  call(method: string, params: Record<string, unknown>): Promise<unknown>
  trust: AuthorizationTrust
  gatewayPublicKey: string
  selection?: AuthorizationSelection
  resourceId?: string
  signal?: AbortSignal
  check?(): void
}

function selectedValue(selection: AuthorizationSelection, space: string) {
  return { resource: { space, resource: selection.resourceId }, audience: selection.audience, grant_leaf: typedId(selection.grantLeaf, 'h2_') }
}

export async function readClientPolicyAuthorization(input: PolicyRead): Promise<VerifiedClientAuthorization> {
  const check = () => { input.signal?.throwIfAborted(); input.check?.() }
  const call = async (method: string, params: Record<string, unknown>) => {
    check()
    const result = await input.call(method, params)
    check()
    return record(result)
  }
  const pinsJson = JSON.stringify(input.trust.pins)
  let selection = input.selection
  check()
  const resourceId = input.resourceId === undefined ? undefined : typedId(input.resourceId, 'res2_')
  if (selection && resourceId !== undefined && selection.resourceId !== resourceId) throw new Error('项目选择与请求资源不同。')
  if (!selection?.grantLeaf) {
    const requestJson = buildClientPolicyReadRequest(input.trust.accountId, input.trust.spaceId, input.gatewayPublicKey, pinsJson, 'null', Date.now() + 120_000)
    const response = await call('begin_client_policy_read', record(JSON.parse(requestJson)))
    let signedPage = response.selections
    let snapshotId: string | undefined
    let cursor: string | null = null
    for (;;) {
      check()
      const page = record(JSON.parse(verifyClientPolicySelections(pinsJson, requestJson, JSON.stringify(signedPage), input.gatewayPublicKey)))
      const id = typedId(page.snapshot_id, '')
      if (snapshotId !== undefined && snapshotId !== id) throw new Error('项目选择分页切换了原签名快照。')
      snapshotId = id
      for (const candidate of array(page.selections)) {
        const value = record(candidate), resource = record(value.resource)
        if (resource.space !== input.trust.spaceId) throw new Error('项目选择属于其他空间。')
        const current = parseAuthorizationSelection({ resourceId: resource.resource, audience: value.audience, grantLeaf: value.grant_leaf })
        if (resourceId !== undefined && current.resourceId !== resourceId) continue
        if (!selection || current.resourceId === selection.resourceId && current.audience.device === selection.audience.device && current.audience.service === selection.audience.service) {
          selection = current
          break
        }
      }
      if (selection?.grantLeaf) break
      if (page.next_cursor === null) throw new Error('当前入口没有匹配的已授权项目。')
      const nextCursor = typedId(page.next_cursor, 'h2_')
      if (nextCursor === cursor) throw new Error('项目选择分页没有继续推进。')
      cursor = nextCursor
      const response = await call('client_policy_selections', { snapshot_id: id, request_hash: typedId(page.request_hash, 'h2_'), cursor })
      signedPage = response.selections
    }
  }
  const selectionJson = JSON.stringify(selectedValue(selection, input.trust.spaceId))
  const expiresAt = Date.now() + 120_000
  const requestJson = buildClientPolicyReadRequest(input.trust.accountId, input.trust.spaceId, input.gatewayPublicKey, pinsJson, selectionJson, expiresAt)
  const begun = await call('begin_client_policy_read', record(JSON.parse(requestJson)))
  let importer: ReturnType<typeof beginClientPolicyImport> | null = null
  let authorization: VerifiedClientAuthorization | null = null
  let confirmed: VerifiedClientAuthorization | null = null
  try {
    importer = beginClientPolicyImport(pinsJson, requestJson, JSON.stringify(begun.snapshot), input.gatewayPublicKey)
    for (;;) {
      const params = record(JSON.parse(importer.pageRequestJson()))
      const response = await call('client_policy_page', params)
      const page = record(response.page)
      importer.appendPage(JSON.stringify(page), Date.now())
      if (record(page.payload).complete === true) break
    }
    const witnessParams = record(JSON.parse(importer.pageRequestJson()))
    const witnessed = await call('client_policy_witness', { ...witnessParams, cursor: null })
    const witnessJson = JSON.stringify(record(witnessed.witness))
    const completed = importer
    importer = null
    authorization = completed.finish(witnessJson, selectionJson, Date.now())
    check()
    const confirmationRequest = buildClientPolicyAuthConfirmRequest(authorization, expiresAt)
    const reply = await call('confirm_client_policy_auth', record(JSON.parse(confirmationRequest)))
    confirmed = confirmClientPolicyAuthorization(authorization, confirmationRequest, JSON.stringify(reply.confirmation), input.gatewayPublicKey, Date.now())
    check()
    const result = confirmed
    confirmed = null
    return result
  } finally {
    importer?.free()
    authorization?.free()
    confirmed?.free()
  }
}
