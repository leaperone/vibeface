import { FieldNote,FormField } from '@/components/arc/form-field'
import { EmptyState,InlineError,SettingsGroup,SettingsRow } from '@/components/arc/product-kit'
import { RoutedAction } from "@/components/arc/routed-action"
import { webPlatform } from '@/lib/platform'
import { useClipboard } from '@/lib/use-clipboard'
import { useDelivery } from '@vibeface/client/delivery/client.ts'
import { receiverChoices } from '@vibeface/client/delivery/model.ts'
import type { DeliveryStatus,SessionPaymentConfig,SessionRelayRoute } from '@vibeface/client/delivery/types.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { Switch } from "@vibeface/uiarc/switch/switch.tsx"
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Input } from '@vibeface/uiarc/input/input.tsx'
import { Select } from '@vibeface/uiarc/select/select.tsx'
import { useEffect,useRef,useState } from 'react'
import { ArrowRightLeft,FlaskConical,LockKeyhole,RadioTower } from 'lucide-react'

export function Lab() {
  const { ready } = useDevice()
  return ready ? <ConnectedLab key={`${ready.generation}:${ready.entry.device_id}`} connection={ready} /> : <EmptyState icon={FlaskConical} title="设备暂时离线" description="重新连接后可查看实验设置和传输记录。" />
}

function ConnectedLab({ connection }: { connection: NonNullable<ReturnType<typeof useDevice>['ready']> }) {
  const delivery = useDelivery(connection, webPlatform)
  const status = delivery.status
  const [sessionAction, setSessionAction] = useState(false)
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <p className="text-sm text-secondary">这里集中放置还在试用的功能，默认关闭。不开启实验也能照常使用聊天和设备同步。</p>
    {delivery.error && !sessionAction ? <InlineError error={delivery.error} /> : null}
    <SettingsGroup title="文件传输" footer="只控制当前连接的电脑。关闭会暂停传输和中转，保留传输记录与已保存的文件。付费订单的原截止时间不会延后。">
      <SettingsRow icon={FlaskConical} label="文件传输实验" description="加密免费转发与付费测试，付费部分只使用本地测试代币">
        {status ? <Switch aria-label="文件传输实验" checked={status.enabled} disabled={!delivery.canManage || delivery.busy !== null} onCheckedChange={next => { const checked = next === true;
          setSessionAction(false)
          void delivery.configure({ enabled: checked, charity_enabled: status.charity_enabled, paid_enabled: status.paid_enabled })
         }} /> : <span role="status">正在读取设置</span>}
      </SettingsRow>
      <SettingsRow icon={ArrowRightLeft} label="发送、接收和传输记录" description="关闭后仍可查看记录、保存已收到的文件">
        <RoutedAction size="sm" variant="secondary" to="/settings/lab/transfers">打开文件传输</RoutedAction>
      </SettingsRow>
    </SettingsGroup>
    {status ? <SessionRelaySettings status={status} delivery={delivery} showError={sessionAction} onAction={() => setSessionAction(true)} /> : null}
    {status && !delivery.canManage ? <p className="text-sm text-secondary">只有管理设备能更改实验设置。</p> : null}
  </div>
}

const emptyRoute: SessionRelayRoute = { conversation_id: '', peer: '', relay: '' }
const peerPattern = '[a-f0-9]{64}'
const validPeer = /^[a-f0-9]{64}$/

function SessionRelaySettings({ status, delivery, showError, onAction }: {
  status: DeliveryStatus; delivery: ReturnType<typeof useDelivery>; showError: boolean; onAction: () => void
}) {
  const { bots } = useWorkspace()
  const { directory } = useDevice()
  const [draft, setDraft] = useState<SessionRelayRoute | null>(null)
  const [paymentDraft, setPaymentDraft] = useState<SessionPaymentConfig | null>(status.session_payment ?? null)
  const [notice, setNotice] = useState('')
  const [relayTouched, setRelayTouched] = useState(false)
  const editButton = useRef<HTMLButtonElement>(null), firstField = useRef<HTMLButtonElement>(null)
  const returnFocus = useRef(false)
  const { copied, failed, copy } = useClipboard(status.node_peer)
  const route = status.session_route
  const available = status.enabled && status.charity_enabled
  const busy = delivery.busy !== null
  const canChange = delivery.canManage && !busy
  const editing = draft !== null && available
  useEffect(() => {
    if (editing) firstField.current?.focus()
  }, [editing])
  useEffect(() => {
    if (!editing && !busy && returnFocus.current) {
      returnFocus.current = false
      editButton.current?.focus()
    }
  }, [editing, busy, route])
  const edit = () => { setRelayTouched(false); setDraft(route ?? emptyRoute) }
  const closeEditor = () => { returnFocus.current = true; setDraft(null) }
  const update = (patch: Partial<SessionRelayRoute>) => setDraft(current => current ? { ...current, ...patch } : null)
  const peers = receiverChoices(directory?.members ?? [], status.node_peer)
  const bot = bots.find(item => item.conversation_id === route?.conversation_id)
  const other = peers.find(item => item.device_id === route?.peer)
  const submit = (next: SessionRelayRoute | null, payment?: SessionPaymentConfig | null) => {
    onAction()
    setNotice('')
    const paymentField = payment === undefined ? paymentDraft ?? undefined : payment === null && paymentDraft === null ? undefined : payment
    const settings = { relay_enabled: status.session_relay_enabled, route: next, ...(paymentField === undefined ? {} : { payment: paymentField }) }
    void delivery.configureSessionRelay(settings).then(saved => {
      if (saved) {
        setNotice(next ? '会话线路已保存在此电脑。' : '已关闭此电脑的会话线路。')
        closeEditor()
      }
    })
  }
  const relay = draft?.relay ?? ''
  const relayError = relay && (relay === draft?.peer || relay === status.node_peer)
    ? '转发电脑不能是本机或对端电脑。'
    : relayTouched && !validPeer.test(relay) ? '请输入 64 位小写连接 ID。' : undefined
  const paymentValid = !paymentDraft || /^[a-f0-9]{64}$/.test(paymentDraft.deployment) && /^[a-f0-9]{64}$/.test(paymentDraft.budget)
    && /^(0|[1-9][0-9]{0,77})$/.test(paymentDraft.amount) && paymentDraft.claim_until_ms > 0
  const valid = !!draft && paymentValid && bots.some(item => item.conversation_id === draft.conversation_id)
    && peers.some(item => item.device_id === draft.peer)
    && validPeer.test(relay) && relay !== draft.peer && relay !== status.node_peer

  return <div className="grid gap-3">
    <SettingsGroup title="加密会话转发" footer="公益模式，不会扣费。两端选同一个 Bot 会话和转发电脑，并把对方设为对端。首次开启不会补发旧记录。">
      <SettingsRow icon={LockKeyhole} stacked label="会话线路" description={route
        ? `${available ? '本机已配置' : '已暂停'} · ${bot?.name ?? '已保存会话'} · ${other?.name ?? '已保存对端'}`
        : '未开启。配置后，所选会话的新内容才会通过这条线路。'}>
        {route ? <span className="flex flex-wrap gap-2">
          <Button ref={editButton} type="button" size="sm" variant="secondary" aria-expanded={editing} aria-controls={editing ? 'session-relay-form' : undefined} disabled={!canChange || !available} onClick={edit}>编辑线路</Button>
          <Button type="button" size="sm" variant="secondary" disabled={!canChange} onClick={() => submit(null, null)}>关闭线路</Button>
        </span> : <Button ref={editButton} type="button" size="sm" variant="secondary" aria-expanded={editing} aria-controls={editing ? 'session-relay-form' : undefined} disabled={!canChange || !available} onClick={edit}>配置线路</Button>}
      </SettingsRow>
      <SettingsRow icon={RadioTower} label="在此电脑提供转发" description={status.session_relay_enabled
        ? (available ? '暂存其他设备的加密后的内容，会使用本机带宽和存储。' : '已暂停。重新开启文件传输和免费传输后会继续为其他设备转发。')
        : '关闭。开启后为其他设备暂存和转发加密后的内容。'}>
        <Switch
          aria-label="在此电脑提供转发"
          checked={status.session_relay_enabled}
          disabled={!canChange || (!available && !status.session_relay_enabled)}
          onCheckedChange={enabled => {
            onAction()
            setNotice('')
            const relay_enabled = enabled === true
            void delivery.configureSessionRelay({ relay_enabled, route }).then(saved => {
              if (saved) setNotice(relay_enabled ? '本机已开始提供公益转发。' : '本机已停止提供公益转发。')
            })
          }}
        />
      </SettingsRow>
      <SettingsRow icon={LockKeyhole} label="付费结算实验" description={paymentDraft ? '已启用：验收回执会进入批量结算队列。' : '关闭。公益转发不会产生付款。'}>
        <Switch aria-label="付费结算实验" checked={paymentDraft !== null} disabled={!canChange} onCheckedChange={enabled => {
          if (enabled) {
            setPaymentDraft(status.session_payment ?? { deployment: '', budget: '', amount: '0', claim_until_ms: Date.now() + 86_400_000 })
            if (route) setDraft(route)
          } else {
            setPaymentDraft(null)
            if (route) submit(route, null)
          }
        }} />
      </SettingsRow>
      {status.session_relay_enabled ? <SettingsRow icon={RadioTower} label="本机连接 ID" description="把 ID 分享给需要使用此转发的设备。">
        <Button type="button" size="sm" variant="secondary" onClick={() => { void copy(status.node_peer) }}>{copied ? '已复制' : '复制连接 ID'}</Button>
      </SettingsRow> : null}
    </SettingsGroup>
    {failed ? <p className="text-sm text-secondary" role="status">无法访问剪贴板，请手动复制：<code className="break-all">{status.node_peer}</code></p> : null}
    <span className="sr-only" role="status">{copied ? '转发连接 ID 已复制。' : ''}</span>
    <span className="sr-only" role="status">{delivery.busy === 'configureSessionRelay' ? '正在保存会话转发设置。' : notice}</span>
    {showError && delivery.error ? <InlineError error={delivery.error} action={{ label: '刷新设置', onClick: () => { void delivery.refresh() } }} /> : null}
    {!available ? <p className="text-sm text-secondary">先在<RoutedAction size="sm" variant="ghost" to="/settings/lab/transfers">文件传输</RoutedAction>中启用文件传输和免费传输。已配置的线路和转发会继续运行。</p> : null}
    {draft && available ? <form id="session-relay-form" aria-label="配置会话线路" className="grid gap-3 px-2" onSubmit={event => {
      event.preventDefault()
      if (valid && canChange) submit({ ...draft, relay })
    }}>
      <FormField>
        <Select ref={firstField} id="session-relay-conversation" label="Bot 会话" placeholder="选择要转发的会话" required disabled={!canChange}
          value={draft.conversation_id ? 'arc:' + draft.conversation_id : ''}
          options={bots.map(item => ({ value: 'arc:' + item.conversation_id, label: item.name }))}
          onValueChange={value => update({ conversation_id: value.slice(4) })}
        />
        {!bots.length ? <FieldNote>当前没有可选的 Bot 会话。</FieldNote> : null}
      </FormField>
      {paymentDraft ? <div className="grid gap-3 rounded-md border border-subtle p-3">
        <p className="text-sm font-medium">付费结算参数</p>
        <Input label="结算部署 ID" value={paymentDraft.deployment} maxLength={64} autoCapitalize="none" autoCorrect="off" spellCheck={false}
          onChange={event => setPaymentDraft({ ...paymentDraft, deployment: event.target.value.trim() })} />
        <Input label="预存预算 ID" value={paymentDraft.budget} maxLength={64} autoCapitalize="none" autoCorrect="off" spellCheck={false}
          onChange={event => setPaymentDraft({ ...paymentDraft, budget: event.target.value.trim() })} />
        <Input label="每条消息金额（最小单位）" inputMode="numeric" value={paymentDraft.amount}
          onChange={event => setPaymentDraft({ ...paymentDraft, amount: event.target.value.trim() })} />
        <Input label="claim 截止时间（毫秒时间戳）" inputMode="numeric" value={String(paymentDraft.claim_until_ms)}
          onChange={event => setPaymentDraft({ ...paymentDraft, claim_until_ms: Number(event.target.value) })} />
        {!paymentValid ? <FieldNote>请输入 64 位小写 ID、非负整数金额和有效截止时间。</FieldNote> : null}
      </div> : null}
      <FormField>
        <Select id="session-relay-peer" label="对端电脑" placeholder="选择我的设备中的电脑" required disabled={!canChange}
          value={draft.peer ? 'arc:' + draft.peer : ''}
          options={peers.map(item => ({ value: 'arc:' + item.device_id, label: item.name }))}
          onValueChange={value => update({ peer: value.slice(4) })}
        />
        {!peers.length ? <FieldNote>先把另一台电脑加入“我的设备”。</FieldNote> : null}
      </FormField>
      <FormField>
        <Input id="session-relay-provider" label="转发电脑的连接 ID" value={draft.relay} pattern={peerPattern} maxLength={64} required disabled={!canChange}
          autoCapitalize="none" autoCorrect="off" spellCheck={false}
          description="向已提供公益转发的电脑索取 ID，由 64 位小写字母 a–f 或数字组成。" error={relayError}
          onBlur={() => setRelayTouched(true)}
          onChange={event => update({ relay: event.target.value.trim() })}
        />
      </FormField>
      <div className="flex flex-wrap gap-2">
        <Button type="submit" disabled={!canChange || !valid}>{route ? '保存线路' : '启用线路'}</Button>
        <Button type="button" variant="secondary" disabled={busy} onClick={closeEditor}>取消</Button>
      </div>
    </form> : null}
  </div>
}
