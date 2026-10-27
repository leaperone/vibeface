import * as Clipboard from 'expo-clipboard'
import { Bell, Bot, ChevronDown, ChevronRight, CircleAlert, CircleCheck, CircleDashed, CircleX, Copy, FolderOpen, Inbox, Layers, ListChecks, LoaderCircle, Monitor, MousePointer2, Pencil, Play, PlugZap, Plus, Sparkles, SquareTerminal, Trash2, X, Zap, type LucideIcon } from 'lucide-react-native'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { messageOf, parseAppServerLink, parseNodeSettings, providerNames, type AppServer, type AppServerLink, type AppServerMode, type ManagerSettings, type NodeSettingsResponse, type ProviderId, type ProviderIssue, type ProviderStatus } from '@vibeface/client/lib/node.ts'
import { parseTriageSettings } from '@vibeface/client/workspace/model.ts'
import type { Outlet } from '@vibeface/client/workspace/types.ts'
import { ErrorText } from '@/components/action'
import { EmptyState, toneText, type Tone } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Segmented } from '@/components/ui/segmented'
import { Text } from '@/components/ui/text'
import { useLive } from '@/data/live'
import { cn } from '@/lib/utils'

/**
 * The AI and reminders of the device this phone connects through, as on the web's device and settings pages. Both go over
 * the encrypted connection, so an admin phone can set them up remotely. Variable values are write-only.
 */
export default function AiScreen() {
  const { ready } = useDevice()
  if (!ready) return null
  if (!ready.principal.can_manage && ready.principal.source !== 'local_session') return <EmptyState icon={Sparkles} title="只有管理设备能修改" />
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-2xl self-center gap-8 p-4 pb-10">
    <Section icon={Sparkles} title={ready.info.name || 'AI'}><AiSettings /></Section>
    <Section icon={Bell} title="会话等你时"><Triage /></Section>
  </KeyboardAwareScrollView>
}

/** Calls the connected device; a newer call or a new connection makes an older answer stale. */
function useCall<T>(parse: (value: unknown) => T, loadMethod: string) {
  const { ready } = useDevice()
  const [value, setValue] = useState<T | null>(null)
  const [busy, setBusy] = useState('')
  const [error, setError] = useState<{ at: string; text: string } | null>(null)
  const sequence = useRef(0)
  const call = async (key: string, method: string, params?: Record<string, unknown>, at = '') => {
    if (!ready?.isCurrent()) return null
    const connection = ready, owner = ++sequence.current
    setBusy(key); setError(null)
    try {
      const result = await connection.request(method, params)
      if (owner !== sequence.current || !connection.isCurrent()) return null
      setValue(parse(method === loadMethod || method.startsWith('configure') ? result : await connection.request(loadMethod)))
      return result
    } catch (cause) {
      if (owner === sequence.current) setError({ at, text: messageOf(cause) })
      return null
    } finally { if (owner === sequence.current) setBusy('') }
  }
  const load = () => call('load', loadMethod)
  useEffect(() => { setValue(null); void load(); return () => { sequence.current += 1 } }, [ready])
  return { value, busy, error, call, load, setError }
}

const providerIcons: Record<ProviderId, LucideIcon> = { codex: SquareTerminal, cursor: MousePointer2 }
const states: Record<ProviderStatus['state'], { label: string; tone: Tone; icon: LucideIcon }> = {
  ready: { label: '可用', tone: 'success', icon: CircleCheck },
  needs_setup: { label: '需设置', tone: 'warning', icon: CircleAlert },
  starting: { label: '启动中', tone: 'neutral', icon: LoaderCircle },
  off: { label: '未使用', tone: 'neutral', icon: CircleDashed },
}
// Names come from runtime/settings.rs readiness.missing.
const checks: Record<ProviderId, { key: string; label: string }[]> = {
  codex: [
    { key: 'Codex CLI', label: '缺少 Codex 命令行' },
  ],
  cursor: [{ key: 'Cursor SDK Bridge', label: '缺少 Cursor 组件' }],
}

const services: Record<string, string> = { CURSOR_API_KEY: 'Cursor', OPENAI_API_KEY: 'OpenAI', ANTHROPIC_API_KEY: 'Anthropic', GEMINI_API_KEY: 'Gemini' }
const keyName = (name: string) => services[name] ? `${services[name]} 密钥` : name

function issueText(provider: ProviderId, issue: ProviderIssue): string {
  const name = providerNames[provider]
  switch (issue.code) {
    case 'bin_missing': return `找不到 ${name} 程序`
    case 'env_missing': return `缺少 ${keyName(issue.name)}`
    case 'launch_failed': return `${name} 没能启动`
    case 'config_invalid': return `${name} 的配置文件读不了`
    case 'app_server_not_found': return '没找到这台电脑上运行的 Codex'
    case 'app_server_unreachable': return '连不上这台电脑上的 Codex'
  }
}

type Change = { bot?: boolean; execution?: boolean; bin?: string; env?: Record<string, string | null>; app_server?: { mode: AppServerMode; socket: string | null } }

function AiSettings() {
  const { ready } = useDevice()
  const { value: response, busy, error, call, load, setError } = useCall(parseNodeSettings, 'node_settings')
  const starting = response?.providers.some((item) => item.state === 'starting')
  useEffect(() => {
    if (!starting) return
    const timer = setTimeout(() => void load(), 2000)
    return () => clearTimeout(timer)
  }, [response])
  // Nothing pushes a device's bot readiness or folders; the new bot form and the chats read them from `nodes`, so read them again when they move.
  const live = useLive()
  const botState = response?.providers.find((item) => item.roles.includes('agent'))?.state
  const folders = response?.settings.roots.join('\n')
  useEffect(() => { if (folders !== undefined) void live?.refresh().catch(() => {}) }, [botState, folders])
  if (!response) return error ? <Retry error={error.text} onRetry={load} /> : <View className="h-28 rounded-xl bg-muted" />
  const { roots, manager } = response.settings
  const test = async (): Promise<AppServerLink | null> => {
    if (!ready?.isCurrent()) return null
    try { const link = parseAppServerLink(await ready.request('test_codex_app_server')); void load(); return link }
    catch (cause) { setError({ at: 'codex', text: messageOf(cause) }); return null }
  }
  return <View className="gap-6">
    {response.providers.map((item) => <ProviderCard key={item.provider} item={item} response={response} busy={busy} test={test}
      error={error?.at === item.provider ? error.text : ''}
      change={(key, change) => call(`${item.provider}:${key}`, 'configure_provider', { provider: item.provider, ...change }, item.provider).then(Boolean)}
      saveAssistant={(next) => call(`${item.provider}:assistant_details`, 'configure_node', { roots, manager: next }, item.provider).then(Boolean)} />)}
    <PathList label="可用文件夹" items={roots} busy={busy !== ''} onChange={(next) => void call('roots', 'configure_node', { roots: next, manager }, 'roots')} />
    {error && (error.at === 'roots' || error.at === '') ? <ErrorText error={error.text} /> : null}
  </View>
}

function ProviderCard({ item, response, busy, error, change, saveAssistant, test }: {
  item: ProviderStatus; response: NodeSettingsResponse; busy: string; error: string
  change(key: string, value: Change): Promise<boolean>; saveAssistant(manager: ManagerSettings): Promise<boolean>; test(): Promise<AppServerLink | null>
}) {
  const name = providerNames[item.provider]
  const assistant = item.roles.includes('agent'), execution = item.roles.includes('execution')
  const manager = assistant ? response.settings.manager : null
  const missing = assistant ? checks[item.provider].filter((check) => response.readiness.missing.includes(check.key)) : []
  const state = states[item.state === 'ready' && missing.length ? 'needs_setup' : item.state]
  const issue = item.state === 'needs_setup' ? item.issues[0] : undefined
  const [open, setOpen] = useState(false)
  const [bin, setBin] = useState(item.bin ?? '')
  useEffect(() => setBin(item.bin ?? ''), [item.bin])
  const working = busy.startsWith(`${item.provider}:`)
  return <View className="overflow-hidden rounded-xl border border-border">
    <View className="min-h-14 flex-row items-center gap-3 px-3 py-2">
      <Icon as={providerIcons[item.provider]} className="text-muted-foreground" />
      <Text className="min-w-0 flex-1 font-medium">{name}</Text>
      <View className="flex-row items-center gap-1">
        <Icon as={state.icon} className={cn('size-4', toneText[state.tone])} />
        <Text className={cn('text-meta', toneText[state.tone])}>{state.label}</Text>
      </View>
      <Toggle icon={Bot} label={`${name} 运行 Bot`} pressed={assistant} busy={busy === `${item.provider}:bot`} disabled={busy !== ''} onPress={() => void change('bot', { bot: !assistant })} />
      {/* An empty slot keeps the bot toggle in the same column on providers that cannot run tasks. */}
      {item.supports.includes('execution') ? <Toggle icon={ListChecks} label={`${name} 执行任务`} pressed={execution} busy={busy === `${item.provider}:execution`} disabled={busy !== ''} onPress={() => void change('execution', { execution: !execution })} /> : <View className="size-11" />}
    </View>
    {issue ? <View className="flex-row flex-wrap items-center gap-x-2 px-3 pb-2">
      <Icon as={CircleAlert} className="size-4 text-warning" />
      <Text className="shrink text-meta text-warning">{issueText(item.provider, issue)}{item.issues.length > 1 ? `，还有 ${item.issues.length - 1} 项` : ''}</Text>
      {issue.code === 'app_server_not_found' && item.app_server
        ? <Button variant="link" className="min-h-11 px-0" disabled={working} onPress={() => void change('app_server', { app_server: { mode: 'spawn', socket: item.app_server?.socket ?? null } })}><Text>改为自己启动</Text></Button>
        : <Button variant="link" className="min-h-11 px-0" onPress={() => setOpen(true)}><Text>去设置</Text></Button>}
    </View> : missing.map((check) => <View key={check.key} className="flex-row items-center gap-2 px-3 pb-2">
      <Icon as={CircleX} className="size-4 text-danger" /><Text className="shrink text-meta">{check.label}</Text>
    </View>)}
    <Pressable accessibilityRole="button" accessibilityState={{ expanded: open }} className="min-h-11 flex-row items-center gap-1 border-t border-border px-3 active:bg-muted/60" onPress={() => setOpen(!open)}>
      <Icon as={open ? ChevronDown : ChevronRight} className="size-4 text-muted-foreground" /><Text className="text-meta text-muted-foreground">设置</Text>
    </Pressable>
    {open ? <View className="gap-5 px-3 pb-3">
      {item.app_server ? <AppServerSettings server={item.app_server} working={working} save={change} test={test} /> : null}
      {item.bin !== null ? <Field label="程序">
        <SaveRow value={bin} saved={item.bin ?? ''} placeholder="codex" mono working={working} onChange={setBin} onSave={() => void change('bin', { bin: bin.trim() })} />
      </Field> : null}
      {manager ? <AssistantDetails key={JSON.stringify(manager)} manager={manager} working={working} save={saveAssistant} /> : null}
      <Variables item={item} working={working} change={change} />
      {issue?.code === 'launch_failed' ? <Field label="启动失败原因"><Text selectable className="font-mono text-meta">{issue.detail}</Text></Field> : null}
    </View> : null}
    {error ? <View className="px-3 pb-3"><ErrorText error={error} /></View> : null}
  </View>
}

function Toggle({ icon, label, pressed, busy, disabled, onPress }: { icon: LucideIcon; label: string; pressed: boolean; busy: boolean; disabled: boolean; onPress(): void }) {
  // While a save runs the toggles keep their look and ignore taps; the spinner says which one is changing.
  return <Button variant={pressed ? 'outline' : 'ghost'} size="icon" className="size-11 rounded-full" accessibilityRole="switch" accessibilityLabel={label} accessibilityState={{ checked: pressed, disabled }} onPress={disabled ? undefined : onPress}>
    <Icon as={busy ? LoaderCircle : icon} className={pressed ? undefined : 'text-muted-foreground'} />
  </Button>
}

const modes: [AppServerMode, string, LucideIcon][] = [['system', '用已在运行的', Monitor], ['spawn', '自己启动', Play]]
const links: Record<AppServerLink['state'], [string, LucideIcon, string]> = {
  connected: ['text-success', CircleCheck, '已连接'],
  starting: ['text-muted-foreground', LoaderCircle, '连接中'],
  off: ['text-muted-foreground', CircleDashed, '未用于执行任务'],
  not_found: ['text-warning', CircleAlert, '未找到'],
  handshake_failed: ['text-warning', CircleAlert, '连不上'],
  launch_failed: ['text-warning', CircleAlert, '没能启动'],
}

/** Where Codex work runs: the App Server already running on that computer, or one the device starts. */
function AppServerSettings({ server, working, save, test }: { server: AppServer; working: boolean; save(key: string, change: Change): Promise<boolean>; test(): Promise<AppServerLink | null> }) {
  const [socket, setSocket] = useState(server.socket ?? '')
  const [tested, setTested] = useState<AppServerLink | null>(null)
  const [testing, setTesting] = useState(false)
  useEffect(() => setSocket(server.socket ?? ''), [server.socket])
  useEffect(() => setTested(null), [server.mode, server.socket])
  const shown = tested ?? server.link
  const [tone, icon, label] = links[shown.state]
  return <Field label="连接方式">
    <View accessibilityRole="radiogroup" className="flex-row gap-1 self-start rounded-lg bg-muted p-1">
      {modes.map(([mode, text, modeIcon]) => <Button key={mode} accessibilityRole="radio" accessibilityState={{ checked: server.mode === mode }} disabled={working}
        variant={server.mode === mode ? 'outline' : 'ghost'} className="min-h-11" onPress={() => { if (mode !== server.mode) void save('app_server', { app_server: { mode, socket: server.socket } }) }}>
        <Icon as={modeIcon} /><Text>{text}</Text>
      </Button>)}
    </View>
    <View className="flex-row flex-wrap items-center gap-2">
      <Icon as={icon} className={cn('size-4', tone)} />
      <Text className={cn('shrink text-meta', tone)}>{tested ? '测试：' : ''}{label}</Text>
      <Button variant="ghost" className="min-h-11" disabled={working || testing || server.link.state === 'off'} onPress={async () => { setTesting(true); try { setTested(await test()) } finally { setTesting(false) } }}>
        <Icon as={testing ? LoaderCircle : PlugZap} /><Text>测试连接</Text>
      </Button>
    </View>
    {shown.state === 'handshake_failed' || shown.state === 'launch_failed' ? <Text selectable className="font-mono text-meta text-muted-foreground">{shown.reason}</Text> : null}
    {server.mode === 'system' ? <SaveRow value={socket} saved={server.socket ?? ''} placeholder="App Server socket 的绝对路径" mono working={working} onChange={setSocket}
      onSave={() => void save('socket', { app_server: { mode: 'system', socket: socket.trim() || null } })} /> : null}
  </Field>
}

const assistantFields = ['model', 'workspace', 'cursor_sdk_bin'] as const

/** The bot's model, folder and programs, inside the settings of the provider it runs on. */
function AssistantDetails({ manager, working, save }: { manager: ManagerSettings; working: boolean; save(manager: ManagerSettings): Promise<boolean> }) {
  const [draft, setDraft] = useState(manager)
  const cursor = manager.provider === 'cursor_sdk'
  const dirty = assistantFields.some((key) => draft[key] !== manager[key])
  const field = (key: (typeof assistantFields)[number], label: string, placeholder?: string) => <Field key={key} label={label}>
    <Input accessibilityLabel={label} className={cn('h-11', key !== 'model' && 'font-mono')} autoCapitalize="none" autoCorrect={false} placeholder={placeholder}
      value={draft[key]} editable={!working} onChangeText={(text) => setDraft({ ...draft, [key]: text })} />
  </Field>
  return <View className="gap-3">
    <Text className="text-meta font-medium text-muted-foreground">Bot</Text>
    {field('model', '模型', cursor ? 'auto' : '默认')}
    {field('workspace', 'Bot 工作文件夹')}
    {cursor ? field('cursor_sdk_bin', 'Cursor 组件') : null}
    {dirty ? <Button variant="outline" className="min-h-11 self-start" disabled={working || (cursor && !draft.model.trim())}
      onPress={() => void save(Object.fromEntries(Object.entries(draft).map(([key, value]) => [key, typeof value === 'string' ? value.trim() : value])) as ManagerSettings)}><Text>保存</Text></Button> : null}
  </View>
}

/** Each variable says only whether it is set; a value is typed once and never shown again. */
function Variables({ item, working, change }: { item: ProviderStatus; working: boolean; change(key: string, value: Change): Promise<boolean> }) {
  const [editing, setEditing] = useState<string | null>(null)
  const [value, setValue] = useState('')
  const [newName, setNewName] = useState('')
  const [newValue, setNewValue] = useState('')
  const addName = newName.trim().toUpperCase()
  const set = (name: string, next: string | null) => change(`env:${name}`, { env: { [name]: next } })
  return <Field label="环境变量">
    {item.env.map((row) => {
      const missing = item.state === 'needs_setup' && !row.set && !row.inherited && row.required
      return <View key={row.name} className="gap-2 rounded-lg bg-muted/60 py-1 ps-3 pe-1">
        <View className="min-h-11 flex-row items-center gap-2">
          <Text numberOfLines={1} className="min-w-0 flex-1 font-mono text-meta">{row.name}</Text>
          <Text className={cn('text-meta', missing ? 'text-warning' : 'text-muted-foreground')}>{row.set ? '••••••' : row.inherited ? '沿用本机' : '未填'}</Text>
          <Button variant="ghost" size="icon" className="size-11" accessibilityLabel={`${row.set ? '替换' : '填写'} ${row.name}`} disabled={working} onPress={() => { setEditing(row.name); setValue('') }}><Icon as={Pencil} className="size-4" /></Button>
          {row.set ? <Button variant="ghost" size="icon" className="size-11" accessibilityLabel={`删除 ${row.name}`} disabled={working} onPress={() => void set(row.name, null)}><Icon as={Trash2} className="size-4" /></Button> : null}
        </View>
        {editing === row.name ? <View className="flex-row gap-2 pe-2 pb-1">
          <Input secureTextEntry autoFocus accessibilityLabel={`${row.name} 的值`} className="min-h-11 min-w-0 flex-1" maxLength={4096} value={value} onChangeText={setValue} />
          <Button variant="outline" className="min-h-11" disabled={working || !value} onPress={() => void set(row.name, value).then((ok) => { if (ok) { setEditing(null); setValue('') } })}><Text>保存</Text></Button>
          <Button variant="ghost" size="icon" className="size-11" accessibilityLabel="取消" onPress={() => { setEditing(null); setValue('') }}><Icon as={X} /></Button>
        </View> : null}
      </View>
    })}
    <Input accessibilityLabel="变量名" placeholder="变量名" className="min-h-11 font-mono" autoCapitalize="characters" autoCorrect={false} maxLength={256} value={newName} editable={!working} onChangeText={setNewName} />
    <View className="flex-row gap-2">
      <Input secureTextEntry accessibilityLabel="值" placeholder="值" className="min-h-11 min-w-0 flex-1" maxLength={4096} value={newValue} editable={!working} onChangeText={setNewValue} />
      <Button variant="outline" className="min-h-11" disabled={working || !addName || !newValue} onPress={() => void set(addName, newValue).then((ok) => { if (ok) { setNewName(''); setNewValue('') } })}>
        <Icon as={Plus} /><Text>添加</Text>
      </Button>
    </View>
    <Text className="text-meta text-muted-foreground">只存在这台电脑上，保存后看不到原值</Text>
    {item.app_server?.mode === 'system' ? <Text className="text-meta text-muted-foreground">用已在运行的 Codex 时，这些变量不生效</Text> : null}
  </Field>
}

const outlets: [Outlet, string, LucideIcon][] = [['record', '只在待处理', Inbox], ['both', '也发给外部聊天', Layers]]

/** Where that device sends sessions that need the person, plus the chat bot channel both ways. Saves on each change. */
function Triage() {
  const { value: settings, busy, error, call, load } = useCall(parseTriageSettings, 'triage_settings')
  const [url, setUrl] = useState('')
  const [secret, setSecret] = useState('')
  const [label, setLabel] = useState('')
  const [token, setToken] = useState('')
  const [test, setTest] = useState<{ ok: boolean; text: string } | null>(null)
  useEffect(() => { if (settings) setUrl(settings.webhook_url) }, [settings?.webhook_url])
  if (!settings) return error ? <Retry error={error.text} onRetry={load} /> : <View className="h-24 rounded-xl bg-muted" />
  const config = { default_outlet: settings.default_outlet, projects: settings.projects, webhook_url: settings.webhook_url, scan_codex_sessions: settings.scan_codex_sessions, codex_sessions: settings.codex_sessions }
  const save = (key: string, patch: Partial<typeof config>, extra: Record<string, unknown> = {}) => {
    const next = { ...config, ...patch }
    return call(key, 'configure_triage', { ...next, webhook_url: next.webhook_url.trim() || null, ...extra })
  }
  const other = (outlet: Outlet): Outlet => outlet === 'both' ? 'record' : 'both'
  const { pending, delivered, dead } = settings.outbox
  return <View className="gap-6">
    <Segmented options={outlets.map(([value, label, icon]) => ({ value, label, icon }))} value={config.default_outlet} disabled={busy !== ''}
      onChange={(value) => void save('default', { default_outlet: value })} />
    <Field label="按文件夹另设">
      <PathList label="" items={config.projects.map((project) => project.prefix)} busy={busy !== ''}
        trailing={(prefix) => {
          const project = config.projects.find((item) => item.prefix === prefix)!
          const [, text] = outlets.find(([value]) => value === project.outlet)!
          return <Button variant="ghost" className="min-h-11 px-2" accessibilityHint="点一下切换" disabled={busy !== ''}
            onPress={() => void save('projects', { projects: config.projects.map((item) => item.prefix === prefix ? { ...item, outlet: other(item.outlet) } : item) })}><Text className="text-meta">{text}</Text></Button>
        }}
        onChange={(prefixes) => void save('projects', { projects: prefixes.map((prefix) => config.projects.find((item) => item.prefix === prefix) ?? { prefix, outlet: other(config.default_outlet) }) })} />
    </Field>
    <Field label="转发到外部聊天">
      <Input accessibilityLabel="外部聊天地址" placeholder="https://" keyboardType="url" autoCapitalize="none" autoCorrect={false} className="min-h-11 font-mono" value={url} onChangeText={setUrl} />
      <View className="flex-row gap-2">
        <Input secureTextEntry accessibilityLabel="外部聊天密钥" placeholder={settings.webhook_secret_set ? '密钥已设置' : '外部聊天密钥'} maxLength={512} className="min-h-11 min-w-0 flex-1" value={secret} onChangeText={setSecret} />
        {settings.webhook_secret_set ? <Button variant="ghost" size="icon" className="size-11" accessibilityLabel="清除密钥" disabled={busy !== ''} onPress={() => void save('webhook', {}, { webhook_secret: null })}><Icon as={Trash2} /></Button> : null}
      </View>
      <View className="flex-row flex-wrap items-center gap-2">
        <Button variant="outline" className="min-h-11" disabled={busy !== '' || (url.trim() === settings.webhook_url && !secret) || (secret.length > 0 && secret.length < 16)}
          onPress={() => void save('webhook', { webhook_url: url }, secret ? { webhook_secret: secret } : {}).then(() => setSecret(''))}>
          {busy === 'webhook' ? <Icon as={LoaderCircle} /> : null}<Text>保存</Text>
        </Button>
        <Button variant="ghost" className="min-h-11" disabled={busy !== '' || !settings.webhook_url || !settings.webhook_secret_set} onPress={() => {
          setTest(null)
          void call('test', 'test_webhook').then((result) => {
            const data = result as { ok?: boolean; status?: number | null; error?: string | null } | null
            if (data) setTest({ ok: data.ok === true, text: data.ok ? `${data.status}` : data.error || `${data.status ?? '没有响应'}` })
          })
        }}><Icon as={busy === 'test' ? LoaderCircle : Zap} /><Text>测试</Text></Button>
        {test ? <View className="flex-row items-center gap-1"><Icon as={test.ok ? CircleCheck : CircleX} className={cn('size-4', test.ok ? 'text-success' : 'text-danger')} /><Text className={cn('text-meta', test.ok ? 'text-success' : 'text-danger')}>{test.text}</Text></View> : null}
      </View>
      {pending + delivered + dead ? <View className="flex-row items-center gap-3">
        {([[delivered, '已送达', CircleCheck, 'text-success'], [pending, '待重试', LoaderCircle, 'text-muted-foreground'], [dead, '失败', CircleX, 'text-danger']] as const).map(([count, text, icon, tone]) =>
          count ? <View key={text} accessible accessibilityLabel={`${text} ${count}`} className="flex-row items-center gap-1"><Icon as={icon} className={cn('size-4', tone)} /><Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{count}</Text></View> : null)}
      </View> : null}
    </Field>
    <Field label="让外部聊天代你回答">
      <Text selectable numberOfLines={1} className="rounded-lg bg-muted/60 px-3 py-2 font-mono text-meta">{settings.decision_url}</Text>
      {token ? <View className="flex-row items-center gap-2 rounded-lg border border-warning-border bg-warning-soft ps-3">
        <Text selectable className="min-w-0 flex-1 font-mono text-meta">{token}</Text>
        <Button variant="ghost" size="icon" className="size-11" accessibilityLabel="复制授权码" accessibilityHint="只显示这一次" onPress={() => void Clipboard.setStringAsync(token)}><Icon as={Copy} /></Button>
        <Button variant="ghost" size="icon" className="size-11" accessibilityLabel="收起" onPress={() => setToken('')}><Icon as={X} /></Button>
      </View> : null}
      {settings.decision_tokens.map((item) => <View key={item.token_id} className="min-h-11 flex-row items-center gap-2 rounded-lg bg-muted/60 ps-3">
        <Text numberOfLines={1} className="min-w-0 flex-1 text-meta">{item.label || '未命名'}</Text>
        <Text className="text-meta text-muted-foreground">{new Date(item.created_ms).toLocaleDateString()}</Text>
        <Button variant="ghost" size="icon" className="size-11" accessibilityLabel={`撤销 ${item.label || '授权码'}`} disabled={busy !== ''} onPress={() => void call('revoke', 'revoke_decision_token', { token_id: item.token_id })}><Icon as={Trash2} className="size-4" /></Button>
      </View>)}
      <View className="flex-row gap-2">
        <Input accessibilityLabel="授权码名称" placeholder="外部聊天" maxLength={64} className="min-h-11 min-w-0 flex-1" value={label} editable={busy === ''} onChangeText={setLabel} />
        <Button variant="outline" className="min-h-11" disabled={busy !== ''} onPress={() => void call('token', 'create_decision_token', { label: label.trim() || '外部聊天' }).then((result) => {
          const value = (result as { token?: unknown } | null)?.token
          if (typeof value === 'string') { setToken(value); setLabel('') }
        })}><Icon as={busy === 'token' ? LoaderCircle : Plus} /><Text>生成授权码</Text></Button>
      </View>
    </Field>
    {error ? <ErrorText error={error.text} /> : null}
  </View>
}

/** Folder paths with a remove button each and a field to add one; a long path keeps its last folder readable. */
function PathList({ label, items, busy, trailing, onChange }: { label: string; items: string[]; busy: boolean; trailing?: (item: string) => ReactNode; onChange(next: string[]): void }) {
  const [path, setPath] = useState('')
  const add = () => { const next = path.trim(); setPath(''); if (next && !items.includes(next)) onChange([...items, next]) }
  const body = <>
    {items.map((item) => <View key={item} className="min-h-11 flex-row items-center gap-2 rounded-lg bg-muted/60 ps-3">
      <Icon as={FolderOpen} className="size-4 text-muted-foreground" />
      <Text numberOfLines={1} ellipsizeMode="head" className="min-w-0 flex-1 font-mono text-meta">{item}</Text>
      {trailing?.(item)}
      <Button variant="ghost" size="icon" className="size-11" accessibilityLabel={`移除 ${item}`} disabled={busy} onPress={() => onChange(items.filter((other) => other !== item))}><Icon as={X} /></Button>
    </View>)}
    <View className="flex-row gap-2">
      <Input accessibilityLabel="文件夹路径" placeholder="/path/to/project" autoCapitalize="none" autoCorrect={false} className="min-h-11 min-w-0 flex-1 font-mono" value={path} editable={!busy}
        returnKeyType="done" onChangeText={setPath} onSubmitEditing={add} />
      <Button variant="outline" className="min-h-11" disabled={busy || !path.trim()} onPress={add}><Icon as={Plus} /><Text>添加</Text></Button>
    </View>
  </>
  return label ? <Field label={label}>{body}</Field> : <View className="gap-2">{body}</View>
}

function SaveRow({ value, saved, placeholder, mono, working, onChange, onSave }: { value: string; saved: string; placeholder: string; mono?: boolean; working: boolean; onChange(value: string): void; onSave(): void }) {
  return <View className="flex-row gap-2">
    <Input accessibilityLabel={placeholder} placeholder={placeholder} autoCapitalize="none" autoCorrect={false} className={cn('h-11 min-w-0 flex-1', mono && 'font-mono')} value={value} editable={!working} onChangeText={onChange} />
    {value.trim() !== saved ? <Button variant="outline" className="min-h-11" disabled={working} onPress={onSave}><Text>保存</Text></Button> : null}
  </View>
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return <View className="gap-2">
    <Text className="text-meta font-medium text-muted-foreground">{label}</Text>
    {children}
  </View>
}

function Retry({ error, onRetry }: { error: string; onRetry(): void }) {
  return <View className="gap-2"><ErrorText error={error} /><Button variant="outline" className="min-h-11 self-start" onPress={onRetry}><Text>重试</Text></Button></View>
}

function Section({ icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <View className="gap-3">
    <View className="flex-row items-center gap-2">
      <Icon as={icon} />
      <Text role="heading" numberOfLines={1} className="shrink text-heading font-semibold">{title}</Text>
    </View>
    {children}
  </View>
}
