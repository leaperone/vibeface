import * as Clipboard from 'expo-clipboard'
import { CircleCheck, CircleHelp, CircleX, Copy, Inbox, Layers, LoaderCircle, MessageSquareShare, Plus, Trash2, X, Zap, type LucideIcon } from 'lucide-react-native'
import { useEffect, useState } from 'react'
import { Linking, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { waitingCopy } from '@vibeface/client/identity/copy.ts'
import { webhookTestResult } from '@vibeface/client/lib/errors.ts'
import { parseTriageSettings } from '@vibeface/client/workspace/model.ts'
import type { Outlet } from '@vibeface/client/workspace/types.ts'
import { ErrorText } from '@/components/action'
import { Details, EmptyState, Skeleton } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Segmented } from '@/components/ui/segmented'
import { Text } from '@/components/ui/text'
import { Field, PathList, Retry, useCall } from '@/features/devices/remote-settings'
import { cn } from '@/lib/utils'

const outlets: [Outlet, string, LucideIcon][] = [['record', waitingCopy.outlets.record, Inbox], ['both', waitingCopy.outlets.both, Layers]]

/**
 * Settings → 会话等你时 → 转发到外部聊天, as on the web: where the connected device sends sessions that need the person,
 * plus the external chat channel both ways. Saves on each change.
 */
export default function ExternalChat() {
  const { ready } = useDevice()
  if (!ready) return null
  if (!ready.principal.can_manage && ready.principal.source !== 'local_session') return <EmptyState icon={MessageSquareShare} title="只有管理设备能修改" className="flex-1 justify-center" />
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    <Triage />
  </KeyboardAwareScrollView>
}

function Triage() {
  const { value: settings, busy, error, call, load } = useCall(parseTriageSettings, 'triage_settings')
  const [url, setUrl] = useState('')
  const [secret, setSecret] = useState('')
  const [label, setLabel] = useState('')
  const [token, setToken] = useState('')
  const [test, setTest] = useState<{ ok: boolean; text: string } | null>(null)
  useEffect(() => { if (settings) setUrl(settings.webhook_url) }, [settings?.webhook_url])
  if (!settings) return error ? <Retry error={error.text} onRetry={load} /> : <Skeleton className="h-24 rounded-xl" />
  const config = { default_outlet: settings.default_outlet, projects: settings.projects, webhook_url: settings.webhook_url, scan_codex_sessions: settings.scan_codex_sessions, codex_sessions: settings.codex_sessions }
  const save = (key: string, patch: Partial<typeof config>, extra: Record<string, unknown> = {}) => {
    const next = { ...config, ...patch }
    return call(key, 'configure_triage', { ...next, webhook_url: next.webhook_url.trim() || null, ...extra })
  }
  const other = (outlet: Outlet): Outlet => outlet === 'both' ? 'record' : 'both'
  const { pending, delivered, dead } = settings.outbox
  // Until a chat is connected there is nothing to forward, so only connecting shows.
  const connected = Boolean(settings.webhook_url)
  const connect = <>
    <View className="flex-row items-center gap-2">
      <Text className="min-w-0 flex-1 text-meta text-muted-foreground">{waitingCopy.forwardHint}</Text>
      <Button variant="ghost" size="icon" accessibilityLabel="怎么接" onPress={() => { void Linking.openURL(waitingCopy.forwardHelp).catch(() => {}) }}><Icon as={CircleHelp} className="text-muted-foreground" /></Button>
    </View>
    <Field label={`地址 · ${waitingCopy.urlHint}`}><Input accessibilityLabel="外部聊天地址" placeholder="https://" keyboardType="url" autoCapitalize="none" autoCorrect={false} className="font-mono" value={url} onChangeText={setUrl} /></Field>
    <Field label={`密钥 · ${waitingCopy.secretHint}`}><View className="flex-row gap-2">
      <Input secureTextEntry accessibilityLabel="外部聊天密钥" placeholder={settings.webhook_secret_set ? '已设置' : '至少 16 个字符'} maxLength={512} className="min-w-0 flex-1" value={secret} onChangeText={setSecret} />
      {settings.webhook_secret_set ? <Button variant="ghost" size="icon" accessibilityLabel="清除密钥" disabled={busy !== ''} onPress={() => void save('webhook', {}, { webhook_secret: null })}><Icon as={Trash2} /></Button> : null}
    </View></Field>
    <View className="flex-row flex-wrap items-center gap-2">
      <Button variant="outline" disabled={busy !== '' || (url.trim() === settings.webhook_url && !secret) || (secret.length > 0 && secret.length < 16)}
        onPress={() => void save('webhook', { webhook_url: url }, secret ? { webhook_secret: secret } : {}).then(() => setSecret(''))}>
        {busy === 'webhook' ? <Icon as={LoaderCircle} /> : null}<Text>保存</Text>
      </Button>
      <Button variant="ghost" disabled={busy !== '' || !settings.webhook_url || !settings.webhook_secret_set} onPress={() => {
        setTest(null)
        void call('test', 'test_webhook').then((result) => {
          const data = result as { ok?: boolean; status?: number | null; error?: string | null } | null
          if (data) setTest(webhookTestResult(data))
        })
      }}><Icon as={busy === 'test' ? LoaderCircle : Zap} /><Text>测试</Text></Button>
      {test ? <View className="flex-row items-center gap-1"><Icon as={test.ok ? CircleCheck : CircleX} className={cn('size-4', test.ok ? 'text-success' : 'text-danger')} /><Text className={cn('text-meta', test.ok ? 'text-success' : 'text-danger')}>{test.text}</Text></View> : null}
    </View>
    {pending + delivered + dead ? <View className="flex-row items-center gap-3">
      {([[delivered, '已送达', CircleCheck, 'text-success'], [pending, '待重试', LoaderCircle, 'text-muted-foreground'], [dead, '失败', CircleX, 'text-danger']] as const).map(([count, text, icon, tone]) =>
        count ? <View key={text} accessible accessibilityLabel={`${text} ${count}`} className="flex-row items-center gap-1"><Icon as={icon} className={cn('size-4', tone)} /><Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>{count}</Text></View> : null)}
    </View> : null}
  </>
  return <View className="gap-6">
    <View accessibilityLabel="分诊状态" className="gap-1">
      <Text className="text-meta text-muted-foreground" style={{ fontVariant: ['tabular-nums'] }}>分诊今日已用 {settings.status.budget.used} / {settings.status.budget.limit} 次</Text>
      <Text className="text-meta text-muted-foreground">{settings.status.budget.paused_reason ? '今日额度已用完，分诊暂停，' : '额度重置时间：'}{new Date(settings.status.budget.retry_at_ms).toLocaleString('zh-CN')}{settings.status.budget.paused_reason ? '后恢复。' : '。'}</Text>
      <Text className="text-meta text-muted-foreground">待审阅 {settings.status.pending_events} · 审阅中 {settings.status.inflight_events} · 待确认 {settings.status.unknown_events}</Text>
      {settings.status.unknown_events ? <Text className="text-meta text-muted-foreground">结果待确认的事件不会自动重试。</Text> : null}
    </View>
    {connected ? <Field label="默认去向"><Segmented options={outlets.map(([value, label, icon]) => ({ value, label, icon }))} value={config.default_outlet} disabled={busy !== ''}
      onChange={(value) => void save('default', { default_outlet: value })} /></Field> : null}
    {/* The two folds sit together as one list of extras under the choice above. */}
    <View>
      {connected ? <Details label="按文件夹另设" open={config.projects.length > 0}>
        <PathList label="" items={config.projects.map((project) => project.prefix)} busy={busy !== ''}
          trailing={(prefix) => {
            const project = config.projects.find((item) => item.prefix === prefix)!
            const [, text] = outlets.find(([value]) => value === project.outlet)!
            return <Button variant="ghost" className="px-2" accessibilityHint="点一下切换" disabled={busy !== ''}
              onPress={() => void save('projects', { projects: config.projects.map((item) => item.prefix === prefix ? { ...item, outlet: other(item.outlet) } : item) })}><Text className="text-meta">{text}</Text></Button>
          }}
          onChange={(prefixes) => void save('projects', { projects: prefixes.map((prefix) => config.projects.find((item) => item.prefix === prefix) ?? { prefix, outlet: other(config.default_outlet) }) })} />
      </Details> : null}
      {/* Folded as on the web once connected; until then connecting shows on its own. */}
      {connected ? <Details label="外部聊天 · 已连接">
        {connect}
        <Details label="让外部聊天代你回答">
          <Field label="地址"><Text selectable numberOfLines={1} className="rounded-lg bg-muted/60 px-3 py-2 font-mono text-meta">{settings.decision_url}</Text></Field>
          {token ? <View className="flex-row items-center gap-2 rounded-lg border border-warning-border bg-warning-soft ps-3">
            <Text selectable className="min-w-0 flex-1 font-mono text-meta">{token}</Text>
            <Button variant="ghost" size="icon" accessibilityLabel="复制授权码" accessibilityHint="只显示这一次" onPress={() => void Clipboard.setStringAsync(token)}><Icon as={Copy} /></Button>
            <Button variant="ghost" size="icon" accessibilityLabel="收起" onPress={() => setToken('')}><Icon as={X} /></Button>
          </View> : null}
          {settings.decision_tokens.map((item) => <View key={item.token_id} className="min-h-11 flex-row items-center gap-2 rounded-lg bg-muted/60 ps-3">
            <Text numberOfLines={1} className="min-w-0 flex-1 text-meta">{item.label || '未命名'}</Text>
            <Text className="text-meta text-muted-foreground">{new Date(item.created_ms).toLocaleDateString()}</Text>
            <Button variant="ghost" size="icon" accessibilityLabel={`撤销 ${item.label || '授权码'}`} disabled={busy !== ''} onPress={() => void call('revoke', 'revoke_decision_token', { token_id: item.token_id })}><Icon as={Trash2} className="size-4" /></Button>
          </View>)}
          <View className="flex-row gap-2">
            <Input accessibilityLabel="授权码名称" placeholder="外部聊天" maxLength={64} className="min-w-0 flex-1" value={label} editable={busy === ''} onChangeText={setLabel} />
            <Button variant="outline" disabled={busy !== ''} onPress={() => void call('token', 'create_decision_token', { label: label.trim() || '外部聊天' }).then((result) => {
              const value = (result as { token?: unknown } | null)?.token
              if (typeof value === 'string') { setToken(value); setLabel('') }
            })}><Icon as={busy === 'token' ? LoaderCircle : Plus} /><Text>生成授权码</Text></Button>
          </View>
        </Details>
      </Details> : <View className="gap-4">{connect}</View>}
    </View>
    {error ? <ErrorText error={error.text} /> : null}
  </View>
}
