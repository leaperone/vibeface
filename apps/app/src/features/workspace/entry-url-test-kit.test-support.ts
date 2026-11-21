



import * as byteCodec from '@vibeface/client/lib/byte-codec.ts'

import { selectionOwner } from '@vibeface/client/lib/selection.ts'


import * as workspaceHistory from '@vibeface/client/workspace/history.ts'
import { completeHistory } from '@vibeface/client/workspace/history-fixture.ts'
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { dirname, resolve } from "node:path"

import { fileURLToPath } from "node:url"
import { createContext, Script } from "node:vm"
import { JsxEmit, ModuleKind, transpileModule } from "typescript"
import qrcode from 'qrcode-generator'



import type { OutboxItem, OutboxRecord } from '@vibeface/client/lib/outbox.ts'

import type { HistorySection, WorkRecord, WorkspaceDomainState } from "@vibeface/client/workspace/types.ts"

import { botReadiness, parseConversation, pendingChecks, parseBot } from "@vibeface/client/workspace/model.ts"
import { botCopy, placeChanges, scheduleName, scheduleTrigger } from "@vibeface/client/workspace/bot-settings.ts"
import { awaitsAnswer, foldAnswers, questionOpen, sharedProject } from "@vibeface/client/workspace/questions.ts"
import * as errors from "@vibeface/client/lib/errors.ts"



import { messageKey, replyStatusLabel, inputTargetKey, promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, starters, reachDevice } from "@vibeface/client/chat/protocol.ts"
import { setUpOnManager } from "@vibeface/client/identity/copy.ts"
import { chatTimeline, firstUnread, taskMessage } from "@vibeface/client/chat/timeline.ts"

import { networkKey, storageOf } from "@vibeface/client/workspace/storage.ts"
import { unreachable } from "@vibeface/client/workspace/home.ts"

import * as taskLabels from "@vibeface/client/workspace/labels.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"
import * as workspaceQuestions from "@vibeface/client/workspace/questions.ts"


import { hookHarness, moduleLoader } from '../../lib/test-harness.ts'
import { createChatDrafts } from './test-chat-drafts.test-support.ts'
import * as conversationCache from '@vibeface/client/workspace/conversation-cache.ts'
import * as systemNotifications from '@vibeface/client/workspace/system-notifications.ts'


const isMessageOutbox = (item: OutboxItem): item is OutboxRecord<'message'> => item.method === 'message'
const isWorkspaceCommandOutbox = (item: OutboxItem): item is OutboxRecord<'workspace_command'> => item.method === 'workspace_command'

const context = {
  history_authority: "authority", history: completeHistory(),
  conversation: { conversation_id: "chat", title: "test", owner: "gateway", owner_epoch: 1 }, messages: [],
  works: [], deliveries: [], checks: [], local_node: "gateway", changes: [], overwritten: [], bot_settings: { skills: [], schedules: [], memory: [], memory_sync_conflicts: { items: [], has_more: false } }, sent: [], sent_coverage: { scope: 'main', omitted_unknown: 0 }, questions: [], sessions: [],
}
const historyActions = { historyLoading: new Set<HistorySection>(), loadHistory: async (_section: HistorySection) => {}, cancelHistory(_section: HistorySection) {} }
type Props = Record<string, unknown> & { name?: string; type?: string; role?: string; disabled?: boolean; value?: unknown; onChange?: (event: unknown) => void; onSubmit?: (event?: unknown) => void; onClick?: () => void; action?: () => Promise<unknown> }
type FixtureElement = { type: unknown; props: Props; key?: string }
type ComponentFixture = {
  render(name: string, props?: unknown): FixtureElement[]
  focus(): void
  blur(): void
  close(): void
  readonly output: unknown
  module: Record<string, unknown>
  readonly drafts: ReturnType<typeof createChatDrafts>
  ownsDraftFile(uri: string): boolean
}

function componentHarness(path: string, dependencies: Record<string, unknown>, globals: Record<string, unknown> = {}, privateExport = '', realModules: Record<string, string> = {}): ComponentFixture {
  const hooks = hookHarness(true), { effects, afterRender } = hooks
  const focusEffects = new Map<number, { run: () => (() => void) | void; cleanup?: () => void }>()
  const elements: { type: unknown; props: Props; key?: string }[] = []
  let focused = true, output: unknown
  let scopedChild: { key?: string; name: string; app: ComponentFixture } | undefined
  const react = {
    ...hooks.react,
    lazy: () => 'Lazy', Suspense: 'Suspense',
    useId: () => 'component',
    useSyncExternalStore: (_subscribe: unknown, snapshot: () => unknown) => snapshot(),
    useLayoutEffect: hooks.react.useEffect,
  }
  const jsx = (type: unknown, props: Props, key?: string) => { const element = key === undefined ? { type, props } : { type, props, key }; elements.push(element); return element }
  const ui = new Proxy({}, { get: (_target, name) => name === 'useAction' ? () => ({ busy: null, error: null, run() {}, clear() {} }) : String(name) })
  const module: { exports: Record<string, unknown> } = { exports: {} }
  const draftStorage = new Map<string, string>()
  const platformStorage = { getItem: (key: string) => draftStorage.get(key) ?? null, setItem: (key: string, value: string) => { draftStorage.set(key, value) } }
  // Keep the real draft repository and hook: only native storage and file ownership are controlled.
  let draftRevision = 0
  // Virtual files keep stable URIs, with counted references for independent durable owners.
  const retainedDraftFiles = new Map<string, number>()
  const draftRepository = createChatDrafts({
    storage: { get: key => draftStorage.get(key), set: (key, value) => { draftStorage.set(key, value) }, remove: key => { draftStorage.delete(key) } },
    revision: () => `revision-${++draftRevision}`,
    files: {
      exists: uri => uri.startsWith('file:') || retainedDraftFiles.has(uri),
      owns: uri => retainedDraftFiles.has(uri),
      retain: async file => { retainedDraftFiles.set(file.uri, (retainedDraftFiles.get(file.uri) ?? 0) + 1); return { ...file } },
      release: uri => { const remaining = (retainedDraftFiles.get(uri) ?? 1) - 1; if (remaining) retainedDraftFiles.set(uri, remaining); else retainedDraftFiles.delete(uri) },
    },
  })
  const chatDrafts = {
    ...draftRepository,
    lease(snapshot: Parameters<typeof draftRepository.lease>[0]) {
      const lease = draftRepository.lease(snapshot)
      return { ...lease, files: lease.files.map(file => ({ ...file, async arrayBuffer() {
        const filesystem = dependencies['expo-file-system']
        assert.ok(filesystem && typeof filesystem === 'object' && 'File' in filesystem && typeof filesystem.File === 'function')
        const native: unknown = Reflect.construct(filesystem.File, [file.uri])
        assert.ok(native && typeof native === 'object' && 'arrayBuffer' in native && typeof native.arrayBuffer === 'function')
        const bytes: unknown = await Reflect.apply(native.arrayBuffer, native, [])
        assert.ok(bytes instanceof ArrayBuffer)
        return bytes
      } })) }
    },
  }
  const nativePureModules = new Set(['@/lib/use-selection-owner', '@/features/chat/use-chat-draft', '@/platform/reading-position', '@/platform/chat-location', '@/platform/workspace-history', '@/platform/viewport-read', '@/platform/native-workspace-history-cache'])
  const useFocusEffect = (run: () => (() => void) | void) => {
    const index = hooks.next(), previous = focusEffects.get(index)
    if (!previous || previous.run !== run) afterRender.push(() => {
      previous?.cleanup?.()
      focusEffects.set(index, { run, cleanup: focused ? run() ?? undefined : undefined })
    })
  }
  const nativeRouterModule = dependencies['expo-router']
  const nativeRouter = typeof nativeRouterModule === 'object' && nativeRouterModule !== null && 'router' in nativeRouterModule ? nativeRouterModule.router : {}
  const require = (specifier: string): unknown => {
    if (nativePureModules.has(specifier)) return loadComponentModule(fileURLToPath(new URL(`../../../../../apps/mobile/src/${specifier.slice(2)}.ts`, import.meta.url)))
    if (specifier === './cached') return loadComponentModule(fileURLToPath(new URL('../../../../../apps/mobile/src/data/cached.ts', import.meta.url)))
    if (specifier === '@vibeface/client/lib/use-generation-action.ts') return loadComponentModule(fileURLToPath(new URL('../../../../../packages/client/src/lib/use-generation-action.ts', import.meta.url)))
    if (specifier === '@vibeface/client/lib/selection.ts') return { selectionOwner }
    if (specifier === '@vibeface/client/workspace/conversation-cache.ts') return conversationCache
    if (specifier === '@/platform/native-chat-drafts') return { chatDrafts }
    if (specifier === '@vibeface/client/workspace/system-notifications.ts') return systemNotifications
    if (realModules[specifier]) return loadComponentModule(fileURLToPath(new URL(realModules[specifier], import.meta.url)))
    if (specifier === '@vibeface/client/lib/byte-codec.ts') return dependencies[specifier] ?? byteCodec
    if (specifier === '@/lib/qr-code') return loadQr(fileURLToPath(new URL('../../lib/qr-code.ts', import.meta.url)))
    if (specifier === 'qrcode-generator') return dependencies[specifier] ?? qrcode
    if (specifier === '@/components/arc/routed-action') return { RoutedAction: 'Link', ActionLink: 'a' }
    if (specifier === '@/components/arc/task-run-summary') return { TaskRunSummary: function TaskRunSummary() {} }
    if (path.includes('/mobile/') && specifier === '@/components/arc/input') return { Input: 'TextInput' }
    if (path.includes('/mobile/') && specifier === '@/components/arc/textarea') return { Textarea: 'TextInput' }
    if (path.includes('/mobile/') && specifier === '@/components/arc/chat') return { ComposerInput: 'TextInput', ComposerPanel: 'ComposerPanel', MessageBubble: 'View' }
    if (path.includes('/mobile/') && specifier === '@/components/arc/selection') return { SelectionItem: 'Pressable' }
    if (path.includes('/mobile/') && specifier === '@/lib/use-clipboard') return loadComponentModule(fileURLToPath(new URL('../../../../../apps/mobile/src/lib/use-clipboard.ts', import.meta.url)))
    if (specifier === '@/lib/use-clipboard') return loadComponentModule(fileURLToPath(new URL('../../lib/use-clipboard.ts', import.meta.url)))
    if (specifier === '@vibeface/client/workspace/model.ts') {
      const override = dependencies[specifier]
      return { ...workspaceModel, ...(typeof override === 'object' && override !== null ? override : {}) }
    }
    if (specifier === '@vibeface/client/workspace/overview-policy.ts' || specifier === '@vibeface/client/workspace/presentation.ts') {
      const filename = fileURLToPath(new URL(`../../../../../packages/client/src/workspace/${specifier.endsWith('overview-policy.ts') ? 'overview-policy' : 'presentation'}.ts`, import.meta.url))
      return loadComponentModule(filename)
    }
    if (specifier === '@vibeface/client/workspace/history.ts') return workspaceHistory
    if (specifier === '@vibeface/client/chat/protocol.ts') return { promptIntent, ...dependencies[specifier] as object }
    if (specifier === '@/features/chat/memory-summary') {
      const filename = fileURLToPath(new URL('../../../../../apps/mobile/src/features/chat/memory-summary.tsx', import.meta.url))
      const compiled = transpileModule(readFileSync(filename, 'utf8'), { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
      const memoryModule = { exports: {} }
      new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInNewContext({ Error, TextEncoder, ...globals })(memoryModule.exports, require, memoryModule)
      return memoryModule.exports
    }
    if (specifier === '@/features/chat/chrome' || specifier === './chrome') return { ChatMaterial: 'ChatMaterial', useChatChrome: () => ({ kind: 'solid' }), ...dependencies[specifier] as object }
    if (specifier === '@vibeface/client/workspace/storage.ts') return { storageOf, networkKey, ...dependencies[specifier] as object }
    if (specifier === '@/lib/platform') return { webPlatform: { storage: platformStorage }, ...dependencies[specifier] as object }
    if (specifier === '@vibeface/client/identity/context.tsx' && path.endsWith('/features/chat/composer.tsx')) {
      const identity = dependencies[specifier] as { useDevice?: () => object } | undefined
      return { ...identity, useDevice: () => ({ connectionGeneration: () => 0, ...identity?.useDevice?.() }) }
    }
    if (specifier === '@/platform/files') return dependencies[specifier] ?? { privateFiles: { run: async (operation: (current: () => boolean) => Promise<unknown>) => operation(() => true), retain: async (uri: string) => ({ uri }), ownedFile: (file: { uri: string }) => ({ ...file, text: async () => '', arrayBuffer: async () => new ArrayBuffer(0), dispose() {} }), release() {}, releasePicker() {} } }
    if (specifier === '@/platform') return { mobilePlatform: { storage: platformStorage }, ...dependencies[specifier] as object }
    if (specifier === '@tanstack/react-router') return { Link: 'Link', useNavigate: () => () => {}, useRouterState: ({ select }: { select: (state: unknown) => unknown }) => select({ location: { search: {} } }), ...dependencies[specifier] as object }
    return specifier === 'react' ? react : specifier === 'react/jsx-runtime' ? { jsx, jsxs: jsx } : specifier === 'expo-router' ? { Link: 'Link', useFocusEffect, useRouter: () => nativeRouter, ...dependencies[specifier] as object } : dependencies[specifier] ?? ui
  }
  const loadComponentModule = moduleLoader(createContext({ setTimeout, clearTimeout, TextEncoder, TextDecoder, Uint8Array, Error, ...globals }), (specifier, filename) => {
    if (specifier.startsWith('.') && !specifier.endsWith('.tsx')) return loadComponentModule(resolve(dirname(filename), specifier.endsWith('.ts') ? specifier : `${specifier}.ts`))
    return require(specifier)
  })
  const loadQr = moduleLoader(createContext({}), specifier => require(specifier))
  const filename = fileURLToPath(new URL(path, import.meta.url))
  const source = readFileSync(filename, 'utf8') + (privateExport ? `\nexport { ${privateExport} }\n` : '')
  const compiled = transpileModule(source, { compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX } }).outputText
  new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInNewContext({ Map, Error, TextEncoder, ...globals })(module.exports, require, module)
  return {
    render(name: string, props: unknown = {}) {
      hooks.reset(); elements.length = 0
      const component = module.exports[name]
      assert.ok(typeof component === 'function')
      output = component(props)
      afterRender.splice(0).forEach(run => run())
      if (name === 'ScopedWorkspaceScreen') {
        const child = elements.find(element => typeof element.type === 'function' && ['ScopedWorkspaceContent', 'OfflineWorkspaceHistory'].includes(element.type.name))
        if (child && typeof child.type === 'function') {
          if (!scopedChild || scopedChild.key !== child.key || scopedChild.name !== child.type.name) {
            scopedChild?.app.close()
            scopedChild = { key: child.key, name: child.type.name, app: componentHarness(path, dependencies, globals, child.type.name, realModules) }
          }
          const nested = scopedChild.app.render(scopedChild.name, child.props)
          output = scopedChild.app.output
          return [...elements, ...nested]
        }
      }
      return elements
    },
    focus() { scopedChild?.app.focus(); if (!focused) { focused = true; for (const effect of focusEffects.values()) effect.cleanup = effect.run() ?? undefined } },
    blur() { scopedChild?.app.blur(); if (focused) { focused = false; for (const effect of focusEffects.values()) { effect.cleanup?.(); effect.cleanup = undefined } } },
    close() { scopedChild?.app.close(); for (const effect of [...effects.values(), ...focusEffects.values()]) effect.cleanup?.() },
    get output() { return output },
    module: module.exports,
    get drafts() { return scopedChild?.app.drafts ?? chatDrafts },
    ownsDraftFile: uri => scopedChild ? scopedChild.app.ownsDraftFile(uri) : retainedDraftFiles.has(uri),
  }
}

const pendingResult = () => {
  let resolve: (value: unknown) => void = () => {}, reject: (cause: Error) => void = () => {}
  const promise = new Promise<unknown>((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
const settleComponent = () => new Promise(setImmediate)

const elementText = (node: unknown): string => typeof node === 'string' ? node : Array.isArray(node) ? node.map(elementText).join('')
  : node && typeof node === 'object' && 'props' in node && node.props && typeof node.props === 'object' && 'children' in node.props ? elementText(node.props.children) : ''

function nestedElements(elements: readonly { type: unknown; props: Record<string, unknown> }[], names: readonly string[]) {
  const expanded = [...elements]
  const pending = [...elements]
  const enqueue = (value: unknown) => {
    if (Array.isArray(value)) { value.forEach(enqueue); return }
    if (!value || typeof value !== "object" || !("type" in value) || !("props" in value)) return
    const element = value as { type: unknown; props: Record<string, unknown> }
    if (!expanded.includes(element)) expanded.push(element)
    pending.push(element)
    if (element.props && "children" in element.props) enqueue(element.props.children)
  }
  while (pending.length) {
    const element = pending.shift()!
    if (typeof element.type !== "function" || !names.includes(element.type.name)) continue
    enqueue(element.type(element.props))
  }
  return expanded
}

function componentText(node: unknown): string {
  if (typeof node === 'string' || typeof node === 'number') return String(node)
  if (Array.isArray(node)) return node.map(componentText).join('')
  if (node && typeof node === 'object' && 'props' in node && node.props && typeof node.props === 'object' && 'children' in node.props) return componentText(node.props.children)
  return ''
}

type HarnessElement = { type: unknown; props: Record<string, unknown> }

function findActionProps(node: unknown, label: string): Record<string, unknown> | undefined {
  if (!node || typeof node !== 'object') return undefined
  if (Array.isArray(node)) {
    for (const item of node) {
      const found = findActionProps(item, label)
      if (found) return found
    }
    return undefined
  }
  if ('props' in node && node.props && typeof node.props === 'object') {
    const props = node.props as Record<string, unknown>
    if (componentText(node).includes(label) && (props.action || props.onClick)) return props
    if ('children' in props) return findActionProps(props.children, label)
  }
  return undefined
}

function conversationHarness(workspace: () => Record<string, unknown>, device: Record<string, unknown> = {}) {
  const chatGlobals = {
    setInterval: () => 1, clearInterval() {},
    matchMedia: () => ({ matches: false }),
    document: { getElementById: () => ({ focus() {}, scrollIntoView() {} }), hidden: false, addEventListener() {}, removeEventListener() {} },
    IntersectionObserver: class {
      observe() {}
      disconnect() {}
      constructor(callback: (entries: { isIntersecting: boolean }[]) => void) { callback([{ isIntersecting: false }]) }
    },
  }
  const app = componentHarness('../chat/conversation-screen.tsx', {
    '@/components/app/bot-face': { BotFace: 'BotFace', botMood: () => 'idle', botTint: () => ({}) },
    '@/lib/glossary': { terms: { inbox: { label: '待处理', icon: 'InboxIcon' }, undelivered: { icon: 'UndeliveredIcon' }, approval: { icon: 'ApprovalIcon' } } },
    '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) },
    '@vibeface/client/workspace/context.tsx': { useWorkspace: workspace },
    '@vibeface/client/workspace/labels.ts': taskLabels,
    '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null, ready: { entry: { device_id: 'gateway' }, principal: { can_manage: false, source: 'membership' } }, retryNow: () => {}, ...device }) },
    '@vibeface/client/identity/copy.ts': { setUpOnManager },
    '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice, botReach, botCanSend, canRetryJob, runtimeLabel, canSend, composerHint, reachDevice, starters },
    '@vibeface/client/chat/timeline.ts': { chatTimeline, firstUnread, taskMessage },
    '@vibeface/client/workspace/bot-settings.ts': { placeChanges, scheduleName, scheduleTrigger },
    '@vibeface/client/workspace/model.ts': { botReadiness, pendingChecks },
    '@vibeface/client/workspace/questions.ts': { awaitsAnswer, foldAnswers, questionOpen, sharedProject, workApproval: workspaceQuestions.workApproval },
    '@vibeface/client/lib/errors.ts': errors,
  }, chatGlobals)
  const messages = componentHarness('../chat/elements/message.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: workspace },
    '@vibeface/client/chat/protocol.ts': { promptIntent, messageKey, replyStatusLabel },
    '@vibeface/client/lib/errors.ts': errors,
  })
  const render = (props: object = {}) => app.render('ConversationScreen', props)
  const composer = () => render().find(({ type }) => type === 'PromptInput')?.props as Record<string, unknown> | undefined
  const chatEntries = () => (render().find(({ type }) => type === 'Chat')?.props.entries ?? []) as { key: string; message?: { message_id: string; state?: string; text?: string }; node?: HarnessElement }[]
  const messageTree = (messageId: string) => {
    const message = chatEntries().find((entry) => entry.message?.message_id === messageId)?.message
    assert.ok(message)
    return messages.render('Message', { message })
  }
  return { app: { close: () => { app.close(); messages.close() } }, render, composer, chatEntries, messageTree, findAction: (label: string) => findActionProps(composer()?.above, label) }
}

const parityBot = parseBot({ bot_id: 'bot', conversation_id: 'chat', name: '项目 Bot', instructions: '', models: {}, skills: null, avatar: null, avatar_version: null, event_hash: 'bot-event', last_message: null, busy: null })
const parityNode = { device_id: 'worker', name: '工作设备', online: true, local: false, revoked: false, capabilities: { manager: 'codex', manager_state: 'ready' as const, relay: false, roots: [], codex_connected: true } }
const parityWork = (extra: Partial<WorkRecord> = {}) => {
  const work = parseConversation({ ...context, works: [{
    dispatcher: 'gateway', call_ref: 'dispatch',
    continues: null, parent: null,
    thread: null, phase: null,
    resolved: { provider: 'codex', model: null }, work_id: 'work', trigger: 'message:one', target_node: 'worker', cwd: '/project', title: '核对项目', prompt: '完整请求', state: 'running', summary: '✓ 读记录\n→ 核对结果\n· 等待同步', outcome: null, result_text: '', failure_reason: null, kind: 'codex', thread_id: 'session', ...extra }] }).works[0]
  assert.ok(work && work.resolved)
  return work
}
const paritySnapshot = (value: unknown) => value === null ? null : parseConversation(value)
const parityShared = () => ({
  ...historyActions, generation: 1, isCurrent: () => true, synced: true, connectionOnline: true, conversationId: 'chat', data: paritySnapshot(context),
  bots: [parityBot], selectedBot: parityBot, nodes: [parityNode], openWorks: [{ ...parityWork(), conversation_id: 'chat' }],
  overview: workspaceModel.parseWorkspaceOverview({}), questions: [] as ReturnType<typeof parseConversation>['questions'], answered: {} as Record<string, string>, liveProgress: [], writable: true,
  writeState: { state: 'writable' as const, conflicts: [] }, notifications: [], ignoredNotifications: new Set<string>(), pendingMessages: [], canSaveMessages: true, workspaceState: null as WorkspaceDomainState | null,
  setView(_view: string) {}, selectBot: async (_bot: typeof parityBot) => {}, interruptWork: async (_work: string) => {}, sendMessage: async () => true,
  async readWork(input: { conversation_id: string; work_id: string }) {
    const work = this.data?.conversation?.conversation_id === input.conversation_id ? this.data.works.find((item) => item.work_id === input.work_id) : undefined
    if (!work) throw new Error('共享历史中找不到这个工作')
    return { work: { ...work, target: work.target ?? null, source_events: ['a'.repeat(64)], conversation_id: input.conversation_id }, coverage: { source: 'shared_history' as const, complete: false as const } }
  },
})

const taskOverviewUi = {
  '@vibeface/client/lib/errors.ts': errors,
  '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') },
  '@/components/kit': { providerIcons: { codex: 'CodexIcon' } },
}
function taskOverviewHarness(
  platform: string, shared: ReturnType<typeof parityShared>, dependencies: Record<string, unknown>,
  inbox: () => unknown[] = () => [],
) {
  const app = componentHarness(platform === 'web' ? './tasks-overview.tsx' : '../../../../../apps/mobile/src/features/home/tasks.tsx', {
    '@vibeface/client/workspace/context.tsx': { useWorkspace: () => shared }, '@/data/live': { useLive: () => shared, useInbox: inbox },
    '@/data/context': { useWorkspace: () => ({ bots: shared.bots }) }, '@vibeface/client/identity/context.tsx': { useDevice: () => ({ directory: null }) },
    '@vibeface/client/workspace/labels.ts': taskLabels, '@vibeface/client/workspace/model.ts': workspaceModel, '@vibeface/client/workspace/questions.ts': workspaceQuestions,
    './inbox': { useInbox: inbox }, './workspace-shell': { useCreateBot: () => () => {} },
    ...dependencies,
  })
  return {
    app,
    render: () => app.render(platform === 'web' ? 'TasksOverview' : 'TasksScreen'),
    cardName: platform === 'web' ? 'TaskOverviewCard' : 'TaskRow',
    mastheadName: platform === 'web' ? 'TaskRunSummary' : 'MobileMasthead',
  }
}

function nativeChatHarness(os: string, dependencies: Record<string, unknown>) {
  return componentHarness('../../../../../apps/mobile/app/b/[botId].tsx', {
    'expo-router/react-navigation': { useHeaderHeight: () => 96 },
    'react-native': { Platform: { OS: os }, FlatList: 'FlatList', View: 'View', StyleSheet: { absoluteFill: {} } },
    'react-native-keyboard-controller': { useReanimatedKeyboardAnimation: () => ({ height: { value: 0 }, progress: { value: 0 } }) },
    'react-native-reanimated': { default: { View: 'AnimatedView' }, useAnimatedStyle: (read: () => unknown) => read(), useSharedValue: (value: number) => ({ value }) },
    'react-native-safe-area-context': { useSafeAreaInsets: () => ({ bottom: 34 }), useSafeAreaFrame: () => ({ width: 390 }) },
    'uniwind': { withUniwind: (value: unknown) => value, useUniwind: () => ({ theme: 'light' }), useCSSVariable: () => 'white' },
    '@vibeface/client/chat/protocol.ts': { promptIntent, awaitsDevice: () => false, botReach, botCanSend, composerHint, inputTargetKey, reachDevice, starters }, '@vibeface/client/chat/timeline.ts': { messageTimes: () => new Map() },
    '@vibeface/client/avatar/face.ts': { faceOf: () => ({ hue: 0 }) }, '@vibeface/client/workspace/home.ts': { unreachable }, '@vibeface/client/workspace/bot-settings.ts': { botCopy },
    '@/components/kit': { botTint: () => ({}), EmptyState: 'EmptyState' }, '@/components/bot-face': { BotFace: 'BotFace', ScreenFaces: 'ScreenFaces', botMood: () => 'idle' },
    '@/features/chat/work': { useChatExtras: () => ({ first: [], after: new Map(), rest: [], cards: [] }) }, '@/lib/layout': { useWide: () => false },
    ...dependencies,
  })
}

function nativeFileFixture() {
  const index = new Map<string, string>()
  const files = new Map<string, Uint8Array>(), directories = new Set<string>(['file:///documents', 'file:///cache'])
  let failure = ''
  const uriOf = (...parts: (string | { uri: string })[]) => parts.map(part => typeof part === 'string' ? part : part.uri).join('/').replace(/\/$/, '')
  class Directory {
    readonly uri: string
    constructor(...parts: (string | { uri: string })[]) { this.uri = uriOf(...parts) }
    get exists() { return directories.has(this.uri) }
    create() { directories.add(this.uri) }
    list() {
      const direct = (uri: string) => uri.startsWith(this.uri + '/') && !uri.slice(this.uri.length + 1).includes('/')
      return [...[...directories].filter(direct).map(uri => new Directory(uri)), ...[...files.keys()].filter(direct).map(uri => new File(uri))]
    }
    delete() {
      if (failure === this.uri) throw new Error('controlled delete failure')
      for (const uri of files.keys()) if (uri.startsWith(this.uri + '/')) files.delete(uri)
      directories.delete(this.uri)
    }
  }
  class File {
    uri: string
    constructor(...parts: (string | { uri: string })[]) { this.uri = uriOf(...parts) }
    get exists() { return files.has(this.uri) }
    get name() { return this.uri.split('/').at(-1) ?? '' }
    get size() { return files.get(this.uri)?.length ?? 0 }
    create() { files.set(this.uri, new Uint8Array()) }
    writeSync(value: string | Uint8Array) { if (failure === this.uri || failure === 'recovery-write' && this.uri.endsWith('.tmp')) throw new Error('controlled write failure'); files.set(this.uri, typeof value === 'string' ? new TextEncoder().encode(value) : value.slice()) }
    async write(value: string | Uint8Array) { this.writeSync(value) }
    textSync() { return new TextDecoder().decode(files.get(this.uri)) }
    async text() { return this.textSync() }
    async bytes() { return files.get(this.uri)?.slice() ?? new Uint8Array() }
    async arrayBuffer() { return Uint8Array.from(await this.bytes()).buffer }
    async copy(destination: File): Promise<void> { const value = files.get(this.uri); assert.ok(value); destination.writeSync(value) }
    copySync(): never { assert.fail('attachment retain must use async copy') }
    moveSync(destination: File) { const value = files.get(this.uri); assert.ok(value); destination.writeSync(value); files.delete(this.uri); this.uri = destination.uri }
    delete() { if (failure === this.uri) throw new Error('controlled delete failure'); files.delete(this.uri) }
  }
  const native = { Directory, File, Paths: { document: new Directory('file:///documents'), cache: new Directory('file:///cache') } }
  const mmkv = { createMMKV: () => ({ getString: (key: string) => index.get(key), getAllKeys: () => [...index.keys()], set: (key: string, value: string) => { if (failure === 'recovery-index') throw new Error('controlled index failure'); index.set(key, value) }, remove: (key: string) => index.delete(key), clearAll: () => index.clear() }) }
  return { native, mmkv, files, fail: (uri: string) => { failure = uri } }
}


export { isMessageOutbox, isWorkspaceCommandOutbox, context, historyActions, componentHarness, pendingResult, elementText, nestedElements, componentText, findActionProps, conversationHarness, parityBot, parityNode, parityWork, paritySnapshot, parityShared, taskOverviewUi, taskOverviewHarness, nativeChatHarness, nativeFileFixture, settleComponent }
export type { Props, FixtureElement, ComponentFixture, HarnessElement }
