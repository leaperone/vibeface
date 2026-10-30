import { Bot, ChevronDown, ChevronRight, CircleAlert, CircleCheck, CircleDashed, ListChecks, LoaderCircle, Monitor, Pencil, Play, PlugZap, Plus, Sparkles, Trash2, X, type LucideIcon } from 'lucide-react-native'
import { useEffect, useState, type ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { deviceCopy } from '@vibeface/client/identity/copy.ts'
import { botOnChange, folderSuggestions, messageOf, parseAppServerLink, parseNodeSettings, providerGroups, providerNames, type AppServer, type AppServerLink, type AppServerMode, type ManagerSettings, type NodeSettingsResponse, type ProviderId, type ProviderIssue, type ProviderStatus } from '@vibeface/client/lib/node.ts'
import { folderLabels } from '@vibeface/client/workspace/bot-settings.ts'
import { ErrorText } from '@/components/action'
import { Details, EmptyState, Skeleton, providerIcons, toneText, type Tone } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Text } from '@/components/ui/text'
import { useLive } from '@/data/live'
import { Field, PathList, Retry, useCall } from '@/features/devices/remote-settings'
import { cn } from '@/lib/utils'

/**
 * The AI of the device this phone connects through, as on the web's device page. It goes over the encrypted connection,
 * so an admin phone can set it up remotely. Variable values are write-only.
 */
export default function AiScreen() {
  const { ready } = useDevice()
  if (!ready) return null
  if (!ready.principal.can_manage && ready.principal.source !== 'local_session') return <EmptyState icon={Sparkles} title="只有管理设备能修改" className="flex-1 justify-center" />
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    <Section icon={Sparkles} title={ready.info.name || 'AI'}><AiSettings /></Section>
  </KeyboardAwareScrollView>
}

const states: Record<ProviderStatus['state'], { label: string; tone: Tone; icon: LucideIcon }> = {
  ready: { label: '可用', tone: 'success', icon: CircleCheck },
  needs_setup: { label: '需设置', tone: 'warning', icon: CircleAlert },
  starting: { label: '启动中', tone: 'neutral', icon: LoaderCircle },
  off: { label: '未使用', tone: 'neutral', icon: CircleDashed },
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
  useEffect(() => { if (folders !== undefined) live?.refresh().then(() => setError((shown) => shown?.at === 'refresh' ? null : shown), (cause) => { if (ready?.isCurrent()) setError({ at: 'refresh', text: messageOf(cause, '读取最新内容') }) }) }, [botState, folders])
  if (!response) return error ? <Retry error={error.text} onRetry={load} /> : <Skeleton className="h-28 rounded-xl" />
  const { roots, manager } = response.settings
  const test = async (): Promise<AppServerLink | null> => {
    if (!ready?.isCurrent()) return null
    try { const link = parseAppServerLink(await ready.request('test_codex_app_server')); void load(); return link }
    catch (cause) { setError({ at: 'codex', text: messageOf(cause) }); return null }
  }
  const { shown, folded } = providerGroups(response.providers)
  const row = (item: ProviderStatus, index: number) => <ProviderRow key={item.provider} item={item} first={index === 0} response={response} busy={busy} test={test}
    error={error?.at === item.provider ? error.text : ''}
    change={(key, change) => call(`${item.provider}:${key}`, 'configure_provider', { provider: item.provider, ...change }, item.provider).then(Boolean)}
    saveAssistant={(next) => call(`${item.provider}:assistant_details`, 'configure_node', { roots, manager: next }, item.provider).then(Boolean)} />
  const list = (items: ProviderStatus[]) => <View className="overflow-hidden rounded-xl border border-border bg-card shadow-xs dark:border-line-strong">{items.map(row)}</View>
  return <View className="gap-6">
    <View className="gap-1 px-1">
      {(['bot', 'execution'] as const).map((role) => <View key={role} className="flex-row items-start gap-1.5">
        <Icon as={roles[role].icon} className="mt-0.5 size-4 text-muted-foreground" />
        <Text className="min-w-0 flex-1 text-meta text-muted-foreground"><Text className="text-meta font-medium">{roles[role].label}</Text> · {roles[role].note}</Text>
      </View>)}
    </View>
    {shown.length ? list(shown) : null}
    {folded.length ? <Details label={`未安装 · ${folded.map((item) => providerNames[item.provider]).join('、')}`} open={!shown.length}>{list(folded)}</Details> : null}
    <PathList label="可用文件夹" items={roots} shown={response.display_paths} suggested={folderSuggestions(response)} busy={busy !== ''} onChange={(next) => void call('roots', 'configure_node', { roots: next, manager }, 'roots')} />
    {error && ['roots', 'refresh', ''].includes(error.at) ? <ErrorText error={error.text} /> : null}
  </View>
}

/** One line per provider, as on the web: its uses that are on, and its state unless it is unused. Tapping it opens the switches and settings. */
function ProviderRow({ item, first, response, busy, error, change, saveAssistant, test }: {
  item: ProviderStatus; first: boolean; response: NodeSettingsResponse; busy: string; error: string
  change(key: string, value: Change): Promise<boolean>; saveAssistant(manager: ManagerSettings): Promise<boolean>; test(): Promise<AppServerLink | null>
}) {
  const name = providerNames[item.provider]
  const assistant = item.roles.includes('agent'), execution = item.roles.includes('execution')
  const manager = assistant ? response.settings.manager : null
  const state = item.state === 'off' ? null : states[item.state]
  const issue = item.state === 'needs_setup' ? item.issues[0] : undefined
  const [open, setOpen] = useState(false)
  const [bin, setBin] = useState(item.bin)
  useEffect(() => setBin(item.bin), [item.bin])
  const working = busy.startsWith(`${item.provider}:`)
  const using = [assistant ? roles.bot : null, execution ? roles.execution : null].filter((role) => role !== null)
  return <View className={cn(!first && 'border-t border-border')}>
    <Pressable accessibilityRole="button" accessibilityState={{ expanded: open }} accessibilityLabel={[name, ...using.map((role) => role.label), state?.label].filter(Boolean).join('，')}
      className="min-h-14 flex-row items-center gap-3 px-4 py-2 active:bg-muted/60" onPress={() => setOpen(!open)}>
      <Icon as={providerIcons[item.provider]} className="text-muted-foreground" />
      <Text numberOfLines={1} className="min-w-0 flex-1 font-medium">{name}</Text>
      {using.map((role) => <Icon key={role.label} as={role.icon} className="size-4 text-brand" />)}
      {state ? <View className="flex-row items-center gap-1">
        <Icon as={state.icon} className={cn('size-4', toneText[state.tone])} />
        <Text className={cn('text-meta', toneText[state.tone])}>{state.label}</Text>
      </View> : null}
      <Icon as={open ? ChevronDown : ChevronRight} className="size-4 text-muted-foreground" />
    </Pressable>
    {issue ? <View className="-mt-1 flex-row flex-wrap items-center gap-x-2 px-4 pb-2">
      <Icon as={CircleAlert} className="size-4 text-warning" />
      <Text className="shrink text-meta text-warning">{issueText(item.provider, issue)}{item.issues.length > 1 ? `，还有 ${item.issues.length - 1} 项` : ''}</Text>
      {issue.code === 'app_server_not_found' && item.app_server
        ? <Button variant="link" className="px-0" disabled={working} onPress={() => void change('app_server', { app_server: { mode: 'spawn', socket: item.app_server?.socket ?? null } })}><Text>{deviceCopy.appServer.useSpawn}</Text></Button>
        : open ? null : <Button variant="link" className="px-0" onPress={() => setOpen(true)}><Text>去设置</Text></Button>}
    </View> : null}
    {open ? <>
      {item.supports.includes('agent') ? <Toggle role="bot" name={name} pressed={assistant} busy={busy === `${item.provider}:bot`} disabled={busy !== ''} onPress={() => void change('bot', assistant ? { bot: false } : botOnChange(response, item))} /> : null}
      {item.supports.includes('execution') ? <Toggle role="execution" name={name} pressed={execution} busy={busy === `${item.provider}:execution`} disabled={busy !== ''} onPress={() => void change('execution', { execution: !execution })} /> : null}
      <View className="gap-4 border-t border-border p-4">
        {/* Keyed by the roles, so switching one drops a test result that no longer matches what runs. */}
        {item.app_server ? <AppServerSettings key={item.roles.join()} server={item.app_server} working={working} save={change} test={test} /> : null}
        <Field label="程序">
          <SaveRow value={bin} saved={item.bin} placeholder={item.bin.split(/[\\/]/).pop() ?? ''} mono working={working} onChange={setBin} onSave={() => void change('bin', { bin: bin.trim() })} />
        </Field>
        {manager ? <AssistantDetails key={JSON.stringify(manager)} manager={manager} working={working} save={saveAssistant} /> : null}
        <Variables item={item} working={working} change={change} />
        {issue?.code === 'launch_failed' ? <Field label="启动失败原因"><Text selectable className="font-mono text-meta">{issue.detail}</Text></Field> : null}
      </View>
    </> : null}
    {error ? <View className="px-4 pb-3"><ErrorText error={error} /></View> : null}
  </View>
}

const roles = {
  bot: { ...deviceCopy.roles.bot, icon: Bot },
  execution: { ...deviceCopy.roles.execution, icon: ListChecks },
}

function Toggle({ role, name, pressed, busy, disabled, onPress }: { role: keyof typeof roles; name: string; pressed: boolean; busy: boolean; disabled: boolean; onPress(): void }) {
  const { label, hint, icon } = roles[role]
  // Only one provider runs the bot, so that row draws a radio dot: tapping it moves the bot here, tapping it again turns it off, hence a toggle for assistive tech.
  // Executing tasks is a system switch any provider can have on. While a save runs the controls keep their look and ignore taps; the spinner says which one is changing.
  if (role === 'bot') return <Pressable accessibilityRole="togglebutton" accessibilityState={{ checked: pressed, disabled }} accessibilityLabel={`${name} ${label}`} accessibilityHint={hint(name)}
    onPress={disabled ? undefined : onPress} className="min-h-12 flex-row items-center gap-3 border-t border-border px-4 active:bg-muted">
    <Icon as={busy ? LoaderCircle : icon} className="text-muted-foreground" />
    <Text className="min-w-0 flex-1">{label}</Text>
    <View className={cn('size-5 items-center justify-center rounded-full border-2', pressed ? 'border-brand' : 'border-muted-foreground')}>
      {pressed ? <View className="size-2.5 rounded-full bg-brand" /> : null}
    </View>
  </Pressable>
  return <View className="min-h-12 flex-row items-center gap-3 border-t border-border px-4">
    <Icon as={busy ? LoaderCircle : icon} className="text-muted-foreground" />
    <Text className="min-w-0 flex-1">{label}</Text>
    <Switch value={pressed} accessibilityLabel={`${name} ${label}`} accessibilityHint={hint(name)} onValueChange={disabled ? undefined : onPress} />
  </View>
}

const modes: [AppServerMode, string, LucideIcon][] = [['system', deviceCopy.appServer.system, Monitor], ['spawn', deviceCopy.appServer.spawn, Play]]
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
        variant={server.mode === mode ? 'outline' : 'ghost'} onPress={() => { if (mode !== server.mode) void save('app_server', { app_server: { mode, socket: server.socket } }) }}>
        <Icon as={modeIcon} /><Text>{text}</Text>
      </Button>)}
    </View>
    <View className="flex-row flex-wrap items-center gap-2">
      <Icon as={icon} className={cn('size-4', tone)} />
      <Text className={cn('shrink text-meta', tone)}>{tested ? '测试：' : ''}{label}</Text>
      <Button variant="ghost" disabled={working || testing || server.link.state === 'off'} onPress={async () => { setTesting(true); try { setTested(await test()) } finally { setTesting(false) } }}>
        <Icon as={testing ? LoaderCircle : PlugZap} /><Text>测试连接</Text>
      </Button>
    </View>
    {shown.state === 'handshake_failed' || shown.state === 'launch_failed' ? <Text selectable className="font-mono text-meta text-muted-foreground">{shown.reason}</Text> : null}
    {server.mode === 'system' ? <SaveRow value={socket} saved={server.socket ?? ''} placeholder="App Server socket 的绝对路径" mono working={working} onChange={setSocket}
      onSave={() => void save('socket', { app_server: { mode: 'system', socket: socket.trim() || null } })} /> : null}
  </Field>
}

const assistantFields = ['model', 'workspace'] as const

/** The bot's model and folder, inside the settings of the provider it runs on. */
function AssistantDetails({ manager, working, save }: { manager: ManagerSettings; working: boolean; save(manager: ManagerSettings): Promise<boolean> }) {
  const [draft, setDraft] = useState(manager)
  const cursor = manager.provider === 'cursor'
  const dirty = assistantFields.some((key) => draft[key] !== manager[key])
  const field = (key: (typeof assistantFields)[number], label: string, placeholder?: string) => <Field key={key} label={label}>
    <Input accessibilityLabel={label} className={key !== 'model' ? 'font-mono' : undefined} autoCapitalize="none" autoCorrect={false} placeholder={placeholder}
      value={draft[key]} editable={!working} onChangeText={(text) => setDraft({ ...draft, [key]: text })} />
  </Field>
  return <View className="gap-3">
    <Text className="text-meta font-medium text-muted-foreground">Bot</Text>
    {field('model', '模型', cursor ? 'auto' : '默认')}
    {field('workspace', folderLabels.botRun)}
    {dirty ? <Button variant="outline" className="self-start" disabled={working || (cursor && !draft.model.trim())}
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
          <Button variant="ghost" size="icon" accessibilityLabel={`${row.set ? '替换' : '填写'} ${row.name}`} disabled={working} onPress={() => { setEditing(row.name); setValue('') }}><Icon as={Pencil} className="size-4" /></Button>
          {row.set ? <Button variant="ghost" size="icon" accessibilityLabel={`删除 ${row.name}`} disabled={working} onPress={() => void set(row.name, null)}><Icon as={Trash2} className="size-4" /></Button> : null}
        </View>
        {editing === row.name ? <View className="flex-row gap-2 pe-2 pb-1">
          <Input secureTextEntry autoFocus accessibilityLabel={`${row.name} 的值`} className="min-w-0 flex-1" maxLength={4096} value={value} onChangeText={setValue} />
          <Button variant="outline" disabled={working || !value} onPress={() => void set(row.name, value).then((ok) => { if (ok) { setEditing(null); setValue('') } })}><Text>保存</Text></Button>
          <Button variant="ghost" size="icon" accessibilityLabel="取消" onPress={() => { setEditing(null); setValue('') }}><Icon as={X} /></Button>
        </View> : null}
      </View>
    })}
    <Input accessibilityLabel="变量名" placeholder="变量名" className="font-mono" autoCapitalize="characters" autoCorrect={false} maxLength={256} value={newName} editable={!working} onChangeText={setNewName} />
    <View className="flex-row gap-2">
      <Input secureTextEntry accessibilityLabel="值" placeholder="值" className="min-w-0 flex-1" maxLength={4096} value={newValue} editable={!working} onChangeText={setNewValue} />
      <Button variant="outline" disabled={working || !addName || !newValue} onPress={() => void set(addName, newValue).then((ok) => { if (ok) { setNewName(''); setNewValue('') } })}>
        <Icon as={Plus} /><Text>添加</Text>
      </Button>
    </View>
    <Text className="text-meta text-muted-foreground">只存在这台电脑上，保存后看不到原值</Text>
    {item.app_server?.mode === 'system' ? <Text className="text-meta text-muted-foreground">用已在运行的 Codex 时，这些变量不生效</Text> : null}
  </Field>
}

function SaveRow({ value, saved, placeholder, mono, working, onChange, onSave }: { value: string; saved: string; placeholder: string; mono?: boolean; working: boolean; onChange(value: string): void; onSave(): void }) {
  return <View className="flex-row gap-2">
    <Input accessibilityLabel={placeholder} placeholder={placeholder} autoCapitalize="none" autoCorrect={false} className={cn('min-w-0 flex-1', mono && 'font-mono')} value={value} editable={!working} onChangeText={onChange} />
    {value.trim() !== saved ? <Button variant="outline" disabled={working} onPress={onSave}><Text>保存</Text></Button> : null}
  </View>
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
