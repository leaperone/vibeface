import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { moduleLoader } from '../../../app/src/lib/test-harness.ts'

const preferences = { enabled: true, sound: false, badge: false, mutedConversations: [] }
function fixture(delayedToken = false) {
  const calls: string[] = []
  let tokenRequests = 0, refuseUnregister = false
  let releaseToken = (_value: { data: string }) => {}
  const token = new Promise<{ data: string }>((resolve) => { releaseToken = resolve })
  const load = moduleLoader(createContext({}), (specifier) => {
    if (specifier === 'expo-constants') return { __esModule: true, default: { expoConfig: { extra: { eas: { projectId: '11111111-1111-4111-8111-111111111111' } } } } }
    if (specifier === 'expo-notifications') return {
      getPermissionsAsync: async () => ({ status: 'granted' }),
      getExpoPushTokenAsync: () => { tokenRequests++; return delayedToken ? token : Promise.resolve({ data: 'ExpoPushToken[controlled-test]' }) },
    }
    throw new Error(`Unexpected dependency: ${specifier}`)
  })
  const remote = load(fileURLToPath(new URL('./remote-notifications.ts', import.meta.url))).remoteNotifications
  assert(remote && typeof remote === 'object' && 'sync' in remote && typeof remote.sync === 'function' && 'prepareErase' in remote && typeof remote.prepareErase === 'function')
  const syncMethod = remote.sync, eraseMethod = remote.prepareErase
  const connection = {
    principal: { network_id: 'network', device_id: 'phone' }, entry: { device_id: 'node' }, isCurrent: () => true,
    async rpc(method: string) { calls.push(method); if (method === 'unregister_push' && refuseUnregister) throw new Error('controlled transport failure'); return { registered: method === 'register_push' } },
  }
  return {
    calls, tokens: () => tokenRequests,
    releaseToken: () => releaseToken({ data: 'ExpoPushToken[controlled-test]' }),
    refuseUnregister: (value: boolean) => { refuseUnregister = value },
    sync: () => Promise.resolve(Reflect.apply(syncMethod, remote, [connection, preferences])),
    erase: () => Promise.resolve(Reflect.apply(eraseMethod, remote, [connection])),
  }
}

test('reset fences a pending native token registration before unregistering the old device', async () => {
  const f = fixture(true), registering = f.sync()
  for (let step = 0; step < 20 && !f.tokens(); step++) await Promise.resolve()
  assert.equal(f.tokens(), 1)
  const erasing = f.erase()
  f.releaseToken()
  await Promise.all([registering, erasing])
  assert.deepEqual(f.calls, ['unregister_push'])
  await f.sync()
  assert.deepEqual(f.calls, ['unregister_push'])
})

test('an unconfirmed unregister reports failure and permits recovery without losing identity', async () => {
  const f = fixture()
  f.refuseUnregister(true)
  await assert.rejects(f.erase(), /后台提醒尚未关闭/)
  f.refuseUnregister(false)
  await f.sync()
  assert.deepEqual(f.calls, ['unregister_push', 'register_push'])
})
