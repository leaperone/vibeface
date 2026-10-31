import assert from 'node:assert/strict'
import test from 'node:test'
import { nextLink, type Link } from '@vibeface/client/lib/link.ts'
import { enqueue, settle, type OutboxItem, type OutboxStore } from '@vibeface/client/lib/outbox.ts'
import { joinTabs } from './tabs.ts'
import { candidates, isPublicUrl, markReached, mergeEntrances, storedEntrances, verifiedEntrances } from '@vibeface/client/lib/entrances.ts'
import { entryAddress } from '@vibeface/client/lib/node.ts'
import { joinCopy } from '@vibeface/client/identity/copy.ts'
import { passphrase, passwordStrength } from '@vibeface/client/lib/secure.ts'
import { failureKinds, failureText, humanError, outcomeUnknown, rawError, spentInvite, spentInviteText, workFailure } from '@vibeface/client/lib/errors.ts'

test('the link moves through join, connect, catch-up, loss and revocation', () => {
  let link: Link = { kind: 'booting' }
  link = nextLink(link, { type: 'ready', joined: false })
  assert.deepEqual(link, { kind: 'unjoined' })
  link = nextLink(link, { type: 'connected', behind: false })
  assert.deepEqual(link, { kind: 'unjoined' }, 'an unjoined browser cannot be online')
  link = nextLink(link, { type: 'ready', joined: true })
  link = nextLink(link, { type: 'connected', behind: true })
  assert.deepEqual(link, { kind: 'online', catchingUp: true })
  link = nextLink(link, { type: 'caught-up' })
  link = nextLink(link, { type: 'lost', reason: 'closed' })
  assert.deepEqual(link, { kind: 'offline', attempt: 1, retryInMs: 500, reason: 'closed' })
  link = nextLink(nextLink(link, { type: 'connect' }), { type: 'lost', reason: 'again' })
  assert.deepEqual(link, { kind: 'offline', attempt: 2, retryInMs: 1000, reason: 'again' })
  for (let i = 0; i < 10; i++) link = nextLink(nextLink(link, { type: 'connect' }), { type: 'lost', reason: 'down' })
  assert.equal(link.kind === 'offline' && link.retryInMs, 10_000)
  link = nextLink(link, { type: 'revoked' })
  assert.deepEqual(nextLink(link, { type: 'connect' }), { kind: 'revoked' })
  assert.deepEqual(nextLink(link, { type: 'left' }), { kind: 'unjoined' })
})

function memory(): OutboxStore & { items: Map<string, OutboxItem> } {
  const items = new Map<string, OutboxItem>()
  return { items, all: async () => [...items.values()], put: async item => { items.set(item.key, structuredClone(item)) }, delete: async key => { items.delete(key) } }
}
const write = (key: string, at: number, text = key): OutboxItem => ({ key, method: 'message', params: { message_id: key, text, attestation: { device_id: 'd', signature: `sig-${text}` } }, created_at_ms: at })

test('the outbox confirms recorded writes, resubmits the rest unchanged, and stops at an unknown result', async () => {
  const store = memory()
  await enqueue(store, write('m1', 1))
  await enqueue(store, write('m2', 2))
  await enqueue(store, write('m3', 3))
  await enqueue(store, write('m4', 4))
  assert.equal((await enqueue(store, write('m1', 9))).created_at_ms, 1, 'the same request keeps its first copy')
  await assert.rejects(enqueue(store, write('m1', 9, 'changed')), /不同内容/)
  const submitted: unknown[] = []
  const result = await settle(store, {
    recorded: async item => item.key === 'm1',
    submit: async item => {
      submitted.push(item.params)
      if (item.key === 'm3') throw Object.assign(new Error('conversation not found'), { responded: true })
      if (item.key === 'm4') throw new Error('connection lost')
    },
  })
  assert.deepEqual(result, { confirmed: ['m1'], resubmitted: ['m2'], rejected: ['m3'], unknown: 'm4' })
  assert.deepEqual(submitted, [write('m2', 2).params, write('m3', 3).params, write('m4', 4).params], 'resubmitted byte-for-byte')
  assert.deepEqual([...store.items.keys()], ['m3', 'm4'])
  assert.equal(store.items.get('m3')?.rejected, 'conversation not found')
  const again = await settle(store, { recorded: async () => true, submit: async () => assert.fail('nothing to send') })
  assert.deepEqual(again, { confirmed: ['m4'], resubmitted: [], rejected: [], unknown: null }, 'a refused write is not retried')
})

test('one tab leads and the next takes over when it closes', { skip: !globalThis.navigator?.locks && 'Web Locks need Node 24 or later' }, async () => {
  const name = `vibeface-test-${Math.random()}`
  const events: string[] = []
  const settle = () => new Promise(resolve => setTimeout(resolve, 20))
  const first = joinTabs<string>(name, { lead: () => { events.push('first leads'); return () => events.push('first resigns') }, message: value => events.push(`first got ${value}`) })
  await settle()
  const second = joinTabs<string>(name, { lead: () => { events.push('second leads') }, message: value => events.push(`second got ${value}`) })
  await settle()
  first.post('changed')
  await settle()
  first.close()
  await settle()
  assert.deepEqual(events, ['first leads', 'second got changed', 'first resigns', 'second leads'])
  second.close()
})

const node = (id: string) => ({ device_id: id.repeat(64), kind: 'node' as const, name: id, manage: false })
const view = { network_id: 'n'.repeat(64), epoch: 1, recovery_key: 'n'.repeat(64), heads: [], members: [node('a'), node('b'), node('c'), { ...node('d'), kind: 'client' as const }], revoked: [{ device_id: 'c'.repeat(64), cutoff_seq: 0, cutoff_hash: null }], attesters: [], pending: null, stopped: [] }
const address = (id: string, gateway: string | null, network = view.network_id, seq = 1) => ({ payload: JSON.stringify({ version: 1, network_id: network, device_id: id.repeat(64), seq, endpoint: { id: id.repeat(64), addrs: [] }, gateway_url: gateway }), signature: `signed-by-${id}` })
const signedBySelf = (deviceId: string, _payload: string, signature: string) => signature === `signed-by-${deviceId[0]}`

test('only entrance records signed by a current member Node of this network survive', () => {
  const records = [
    address('a', 'https://a.example.com/'), address('b', 'http://192.168.1.4:4790/'),
    address('c', 'https://revoked.example.com/'), address('d', 'https://client.example.com/'),
    address('a', 'https://a.example.com/', 'o'.repeat(64)), { ...address('b', 'https://forged.example.com/'), signature: 'signed-by-a' },
    address('e', 'https://stranger.example.com/'), address('a', 'https://a.example.com/path'), 'junk',
  ]
  const found = verifiedEntrances(records, view, signedBySelf)
  assert.deepEqual(found.map(item => [item.device_id[0], item.gateway_url, item.public]), [['a', 'https://a.example.com', true], ['b', 'http://192.168.1.4:4790', false]])
})

test('switching tries the entry that dropped, then entrances by last success; revoked ones leave the list', () => {
  const saved = [
    { device_id: 'a'.repeat(64), seq: 1, gateway_url: 'https://a.example.com', public: true, direct: [], last_ok_ms: 5 },
    { device_id: 'b'.repeat(64), seq: 1, gateway_url: 'https://b.example.com', public: true, direct: [], last_ok_ms: 9 },
    { device_id: 'c'.repeat(64), seq: 1, gateway_url: 'https://c.example.com', public: true, direct: [], last_ok_ms: 99 },
  ]
  const merged = markReached(mergeEntrances(saved, [], view), { kind: 'gateway', address: 'https://a.example.com', device_id: 'a'.repeat(64) }, 1)
  assert.deepEqual(merged.map(item => [item.device_id[0], item.last_ok_ms]), [['a', 1], ['b', 9]], 'c was revoked')
  const order = candidates(merged, { kind: 'gateway', address: 'http://127.0.0.1:5101', device_id: 'a'.repeat(64) })
  assert.deepEqual(order.map(entryAddress), ['http://127.0.0.1:5101', 'https://b.example.com', 'https://a.example.com'])
  for (const local of ['https://localhost', 'https://10.0.0.2', 'https://box.local', 'http://a.example.com', 'https://intranet']) assert.equal(isPublicUrl(local), false, local)
})

test('after a reload an older address record never replaces a newer one, withdrawn addresses included', () => {
  const a = 'a'.repeat(64), b = 'b'.repeat(64)
  const saved = mergeEntrances([], verifiedEntrances([address('a', 'https://new.example.com/', view.network_id, 7), address('b', null, view.network_id, 4)], view, signedBySelf), view)
  const reloaded = JSON.parse(JSON.stringify(saved))
  const stale = verifiedEntrances([address('a', 'https://old.example.com/', view.network_id, 6), address('b', 'https://withdrawn.example.com/', view.network_id, 3)], view, signedBySelf)
  const merged = mergeEntrances(reloaded, stale, view)
  assert.deepEqual(merged.map(item => [item.device_id[0], item.seq, item.gateway_url]), [['a', 7, 'https://new.example.com'], ['b', 4, null]])
  assert.deepEqual(candidates(merged, { kind: 'gateway', address: 'http://127.0.0.1:5101', device_id: a }).map(entryAddress), ['http://127.0.0.1:5101', 'https://new.example.com'], 'a withdrawn address is not tried')
  const newer = mergeEntrances(merged, verifiedEntrances([address('b', 'https://back.example.com/', view.network_id, 5)], view, signedBySelf), view)
  assert.equal(newer.find(item => item.device_id === b)?.gateway_url, 'https://back.example.com')
})

test('the app dials only the Nodes it sees on its LAN, and an unseen one only when no web address is left', () => {
  const a = 'a'.repeat(64), b = 'b'.repeat(64)
  const record = (id: string, gateway: string | null, ips: string[]) => ({ payload: JSON.stringify({ version: 1, network_id: view.network_id, device_id: id.repeat(64), seq: 1, endpoint: { id: id.repeat(64), addrs: [...ips.map(ip => ({ Ip: ip })), { Relay: 'https://relay.example.com/' }] }, gateway_url: gateway }), signature: `signed-by-${id}` })
  const list = verifiedEntrances([record('a', 'https://a.example.com/', ['192.168.1.4:7001', 'junk']), record('b', null, ['192.168.1.5:7002'])], view, signedBySelf)
  assert.deepEqual(list.map(item => item.direct), [['192.168.1.4:7001'], ['192.168.1.5:7002']])
  assert.deepEqual(storedEntrances(JSON.parse(JSON.stringify(list))), list)
  assert.deepEqual(storedEntrances([{ device_id: a, seq: 1, gateway_url: null, public: false, last_ok_ms: 0 }]), [], 'a stored entrance of another shape is dropped')
  const ways = (...args: Parameters<typeof candidates>) => candidates(...args).map(entry => `${entry.kind} ${entryAddress(entry)}`)
  const web = { kind: 'gateway' as const, address: 'https://a.example.com', device_id: a }
  assert.deepEqual(ways(list, web), ['gateway https://a.example.com'], 'a browser has no LAN')
  assert.deepEqual(ways(list, web, new Map()), ['gateway https://a.example.com'], 'an address not seen now is not dialed while a web address remains')
  assert.deepEqual(ways(list, web, new Map([[a, ['192.168.1.8:7001']], [b, ['192.168.1.9:7002']]])), ['lan 192.168.1.8:7001', 'gateway https://a.example.com', 'lan 192.168.1.9:7002'])
  const lan = { kind: 'lan' as const, addrs: ['192.168.1.8:7002'], device_id: b }
  assert.equal(markReached(list, lan, 5), list, 'only web entrances keep a last success')
  const lanOnly = list.filter(item => item.device_id === b)
  assert.deepEqual(candidates(lanOnly, lan, new Map()), [{ kind: 'lan', addrs: ['192.168.1.8:7002', '192.168.1.5:7002'], device_id: b }], 'with no web address left, where it was last reached or signed')
  assert.deepEqual(ways(lanOnly, lan, new Map([[b, ['192.168.1.5:7002']]])), ['lan 192.168.1.5:7002', 'lan 192.168.1.8:7002'])
})

test('backend English and gateway refusal codes reach the page as plain Chinese', () => {
  assert.equal(humanError('unknown member', 'target_not_member'), '那台电脑不在我的设备里，访问地址不会转发给它。')
  assert.match(humanError('conversation not found'), /还没有这个对话/)
  assert.match(failureText('missing_key'), /缺少密钥.*设备 → Bot 与任务/)
  assert.match(failureText('execution_disabled'), /没开执行任务.*设备 → Bot 与任务/)
  assert.match(failureText('quota'), /用量到上限/)
  assert.equal(failureText('unknown'), '', 'an unknown kind is worded by its caller')
  for (const kind of failureKinds) if (kind !== 'unknown') assert.match(failureText(kind), /[一-鿿]/, kind)
  assert.match(humanError('client response timed out; outcome may be unknown'), /结果未知/)
  assert.equal(humanError('已断开设备连接。'), '已断开设备连接。')
  assert.match(humanError('peer did not answer in time'), /另一台设备没有回应/)
  assert.match(humanError('Codex rejected request: {"code":-32600,"message":"no rollout found for thread id 01a15c84"}'), /新对话还没准备好/)
  assert.equal(humanError('set an explicit App Server socket before enabling system execution'), '还没填 Codex 的连接路径。')
  assert.equal(humanError('the App Server socket must be an absolute path'), '连接路径要是绝对路径。')
  assert.match(humanError('set an explicit absolute CODEX_HOME before starting a managed App Server'), /Codex 目录/)
  assert.match(humanError("this computer's Codex folder has no sessions folder yet"), /先用 Codex 跑一次对话/)
  assert.match(humanError('no Codex folder found on this computer', 'codex_missing'), /没找到这台电脑上的 Codex 目录/)
  assert.equal(humanError('something nobody mapped'), '这次操作没有成功。再试一次。')
  assert.equal(humanError('something nobody mapped', undefined, '读取最新内容'), '读取最新内容没有成功。再试一次。')
  assert.equal(rawError(new Error('conversation not found')), 'conversation not found')
  assert.equal(rawError(new Error('已断开')), '')
})

test('a failed task leads with its cause, and one whose outcome is unknown is checked, not run again', () => {
  assert.equal(workFailure({ failure: 'timed_out', failure_reason: 'Codex management timed out' }), failureText('timed_out'))
  assert.equal(workFailure({ failure_reason: 'Codex management timed out' }), '设备没有及时回复，结果未知。先确认结果，别重复提交。')
  assert.equal(workFailure({ failure_reason: null }), '')
  assert.ok(outcomeUnknown('timed_out'))
  assert.ok(outcomeUnknown('lost'))
  assert.ok(outcomeUnknown(undefined, 'Codex management timed out'))
  assert.ok(outcomeUnknown('unknown', 'client response timed out; outcome may be unknown'))
  assert.ok(!outcomeUnknown('quota', 'quota exceeded'))
  assert.ok(!outcomeUnknown('rejected'))
})

test('a spent join link reads the same, and clears the field the same, whether the Node or the page says so', () => {
  for (const raw of ['ticket has already been used', 'ticket is unknown or already expired', 'ticket has expired', spentInviteText]) {
    assert.ok(spentInvite.test(raw), raw)
    assert.equal(humanError(raw), spentInviteText)
  }
  assert.equal(spentInvite.test(joinCopy.rejected), false)
})

test('a recovery password is refused when it is short, common, a run, or short and one kind', () => {
  for (const [password, problem] of [['abc12', 'short'], ['12345678', 'common'], ['Password123', 'common'], ['qwertyuiop', 'common'], ['aaaabbbb', 'common'], ['abcdefgz', 'simple'], ['80417263', 'simple']] as const) {
    assert.deepEqual(passwordStrength(password), { level: 'weak', problem }, password)
    assert.throws(() => passphrase(password), password)
  }
  assert.equal(passwordStrength('tiger7lamp').level, 'fair')
  assert.equal(passwordStrength('river stone 42 lamp').level, 'strong')
  assert.equal(passphrase('tiger7lamp'), 'tiger7lamp')
})

test('this computer\'s join failure reads as the terminal says it, and a spent link clears the field the same', async () => {
  const { nodeJoinCopy, nodeJoinFailure } = await import('@vibeface/client/identity/copy.ts')
  assert.equal(nodeJoinFailure('ticket has already been used'), nodeJoinCopy.used)
  assert.equal(nodeJoinFailure('join request Expired: expired'), nodeJoinCopy.expired)
  assert.match(nodeJoinFailure('join request Rejected: rejected'), /管理设备拒绝了这次加入/)
  assert.match(nodeJoinFailure('another join is in progress; delete join.json to abandon it'), /上次的名字/)
  assert.equal(nodeJoinFailure('connection refused'), nodeJoinCopy.failed)
  for (const text of [nodeJoinCopy.used, nodeJoinCopy.expired]) assert.ok(spentInvite.test(text), text)
  assert.equal(spentInvite.test(nodeJoinCopy.failed), false)
})

test('a provider catalog keeps model effort and distinguishes unavailable from an empty known list', async () => {
  const { parseProviderModels, modelTyped, parseBackupPolicy } = await import('@vibeface/client/lib/node.ts')
  const models = [{ id: 'gpt-a', name: 'GPT A', default: true, effort: { state: 'selectable', options: ['low', 'high'], default: 'high' } }]
  assert.deepEqual(parseProviderModels({ state: 'available', models, coverage: 'account_available' }), { state: 'available', models, coverage: 'account_available' })
  const unavailable = parseProviderModels({ state: 'unavailable', reason: 'catalog_unavailable' })
  assert.deepEqual(unavailable, { state: 'unavailable', reason: 'catalog_unavailable' })
  assert.equal(modelTyped('my-manual-model', null), true)
  assert.deepEqual(parseProviderModels({ state: 'available', models: [], coverage: 'complete' }), { state: 'available', models: [], coverage: 'complete' })
  assert.throws(() => parseProviderModels({ state: 'available', models: [{ name: 'no id' }], coverage: 'complete' }))
  assert.throws(() => parseProviderModels({ state: 'unknown' }))
  assert.deepEqual(parseBackupPolicy({ automatic: false, candidates: [] }), { automatic: false, candidates: [] })
  assert.throws(() => parseBackupPolicy({ automatic: true, candidates: [] }))
  const candidate = { provider: 'claude', model: 'opus', reasoning_effort: null }
  assert.throws(() => parseBackupPolicy({ automatic: true, candidates: [candidate, candidate] }))
})

test('node settings keep a neutral unchecked provider and parse explicit probe evidence', async () => {
  const { parseNodeSettings, availability, summaryReadiness, parseProvider } = await import('@vibeface/client/lib/node.ts')
  const readiness = { state: 'unverified', issue: { stage: 'authentication', code: 'authentication_unverified' } }
  const provider = { provider: 'claude', roles: ['agent'], supports: ['agent', 'execution'], bin: 'claude', app_server: null, installed: true, home: null, sessions: null, env: [], state: 'unverified', issues: [], readiness, execution_model: '', execution_readiness: readiness, probe: { provider: 'claude', config_id: 'config', checked_at_ms: null, launch: { state: 'unchecked' }, authentication: 'unverified', catalog: { state: 'unavailable', reason: 'catalog_unavailable' }, version: null } }
  const manager = { provider: 'claude', workspace: '/work', model: 'my-manual-model', reasoning_effort: null, backup: { automatic: false, candidates: [] } }
  const parsed = parseNodeSettings({ settings: { roots: [], manager }, defaults: { workspace: '/work' }, readiness: { manager: readiness }, providers: [provider], display_paths: {}, suggested_roots: [] })
  assert.equal(availability(parsed.providers[0]), 'unverified')
  const setup = { directory: '/node/providers/claude', command: "env CLAUDE_CONFIG_DIR=/node/providers/claude claude auth login" }
  assert.deepEqual(parseProvider({ ...provider, setup }).setup, setup)
  assert.throws(() => parseProvider({ ...provider, setup: { directory: '/private', command: 7 } }))
  assert.deepEqual(parsed.settings.manager, manager)
  assert.throws(() => parseProvider({ ...provider, probe: { ...provider.probe, launch: { state: 'success' } } }))
  assert.throws(() => parseProvider({ ...provider, probe: { ...provider.probe, provider: 'pi' } }))
  assert.throws(() => parseProvider({ ...provider, readiness: { state: 'maybe' } }))
  assert.throws(() => parseProvider({ ...provider, execution_model: undefined }))
  assert.throws(() => parseProvider({ ...provider, execution_readiness: undefined }))
  assert.throws(() => parseProvider({ ...provider, execution_readiness: null }))
  assert.throws(() => parseProvider({ ...provider, execution_readiness: { state: 'maybe' } }))
  const execution = { state: 'unverified', issue: { stage: 'model_list', code: 'model_unverified' } }
  const split = parseProvider({ ...provider, roles: ['execution'], readiness: { state: 'ready' }, execution_model: 'manual', execution_readiness: execution })
  assert.deepEqual(summaryReadiness(split), execution)
  assert.equal(availability(split), 'unverified')
  assert.equal(availability(parseProvider({ ...provider, roles: ['execution'], execution_readiness: { state: 'ready' } })), 'ready')
  assert.equal(availability(parseProvider({ ...provider, roles: ['agent', 'execution'], readiness: { state: 'ready' }, execution_readiness: execution })), 'ready')
})
