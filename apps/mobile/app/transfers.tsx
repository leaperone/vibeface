import * as Clipboard from 'expo-clipboard'
import { ArrowDownLeft, ArrowDownToLine, ArrowRightLeft, ArrowUpRight, Check, CircleCheck, Copy, Crown, File, FlaskConical, Laptop, LoaderCircle, RefreshCw, ShieldCheck, type LucideIcon } from 'lucide-react-native'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useDelivery } from '@vibeface/client/delivery/client.ts'
import { canRestart, changeMode, deadlineRows, draftFor, evidenceLabels, modeOptions, orderActions, orderHeading, orderStatus, orderSummary, orderTitle, publicationLabel, receiverChoices, receivingBatches, settlementAmountsLabel, sizeLabel, transferCopy, walletNotice } from '@vibeface/client/delivery/model.ts'
import { transferSelection, type DeliveryDraft, type DeliveryOrder, type DeliverySettings, type TransferFile } from '@vibeface/client/delivery/types.ts'
import { useExperiments } from '@vibeface/client/lib/experiments.ts'
import { ErrorText } from '@/components/action'
import { Details, EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Segmented } from '@/components/ui/segmented'
import { Switch } from '@/components/ui/switch'
import { Text } from '@/components/ui/text'
import { useRun } from '@/lib/use-run'
import { cn } from '@/lib/utils'
import { mobilePlatform } from '@/platform'

const roleIcons = { sender: ArrowUpRight, receiver: ArrowDownLeft, provider: ArrowRightLeft } as const

export default function TransfersScreen() {
  const { ready } = useDevice()
  return ready ? <Transfers key={`${ready.generation}:${ready.entry.device_id}`} connection={ready} /> : <EmptyState icon={ArrowRightLeft} title={`设备暂时离线。${transferCopy.offline}`} className="flex-1 justify-center" />
}

function Transfers({ connection }: { connection: NonNullable<ReturnType<typeof useDevice>['ready']> }) {
  const delivery = useDelivery(connection, mobilePlatform)
  const { directory } = useDevice()
  // Paid delivery and its test chain stay out of sight unless 实验功能 is on in settings.
  const [experiments] = useExperiments(mobilePlatform.storage)
  const [draft, setDraft] = useState<DeliveryDraft>({ receiver_peer: '', provider_peer: '', mode: 'charity', acceptance: 'recoverable', max_service_amount: '0', max_gas_amount: '0', budget_id: null })
  const [file, setFile] = useState<TransferFile | null>(null)
  const selection = useRef(transferSelection())
  useEffect(() => {
    const owner = transferSelection()
    selection.current = owner
    return () => owner.close()
  }, [])
  // Feedback shows next to the control that caused it, not at the top of a long page.
  const [origin, setOrigin] = useState<'page' | 'form' | 'order'>('page')
  const picker = useRun()
  const status = delivery.status, selected = delivery.selected
  const working = delivery.busy !== null || picker.busy !== null
  const change = (patch: Partial<DeliverySettings>) => {
    setOrigin('page')
    if (status) void delivery.configure({ enabled: status.enabled, charity_enabled: status.charity_enabled, paid_enabled: status.paid_enabled, ...patch })
  }
  const feedback = (where: typeof origin) => origin === where ? <Feedback error={delivery.error} notice={delivery.notice} /> : null
  // Paid already on at this Node stays visible, so it can be seen and turned off.
  const paid = experiments || !!status?.paid_enabled
  const sending = draftFor(draft, paid)
  const availableBudgets = delivery.budgets.filter(budget => budget.state === 'open' && budget.payer.toLowerCase() === status?.wallet.address?.toLowerCase())
  const preparedBatch = delivery.orders.filter(order => order.role === 'sender' && order.mode === 'paid' && order.phase === 'prepared' && order.payment.budget_id === draft.budget_id)
  const canConfirmBatch = preparedBatch.length === 2 && preparedBatch.every(order => order.windows.length === 1)
  const mine = receiverChoices(directory?.members ?? [], status?.node_peer ?? '')
  const picked = mine.some(item => item.device_id === draft.receiver_peer)
  const receiverId = <InputField label={transferCopy.receiver} value={picked ? '' : draft.receiver_peer} disabled={working} onChange={receiver_peer => setDraft({ ...draft, receiver_peer })} peer />
  const unusable = working || !file || !draft.receiver_peer || status?.max_file_bytes === 0 || sending.mode === 'charity' && !status?.charity_enabled || sending.mode === 'paid' && (!status?.paid_enabled || !sending.budget_id)
  const prepare = (restart?: string) => {
    if (!file || unusable) return
    setOrigin('form')
    const owner = selection.current
    void owner.use(selected => delivery.prepare(sending, selected, restart)).then(success => { if (success && owner.replace(null)) setFile(null) })
  }
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    {feedback('page')}
    {!status ? <Text role="status" className="text-meta text-muted-foreground">{transferCopy.loading}</Text> : <>
      <View className="gap-3 rounded-xl border border-border bg-card p-4 shadow-xs dark:border-line-strong">
        <Text role="heading" className="text-heading font-semibold">传输设置</Text>
        <Toggle label="启用文件传输" checked={status.enabled} disabled={!delivery.canManage || working} change={enabled => change({ enabled })} />
        {status.enabled ? <>
          <Toggle label={transferCopy.charity} checked={status.charity_enabled} disabled={!delivery.canManage || working} change={charity_enabled => change({ charity_enabled })} />
          {paid ? <Toggle label={transferCopy.paid} icon={FlaskConical} checked={status.paid_enabled} disabled={!delivery.canManage || working} change={paid_enabled => change({ paid_enabled })} /> : null}
          <View className="gap-1"><Text className="text-meta text-muted-foreground">{transferCopy.connectionId}</Text><CopyConnectionId value={status.node_peer} /></View>
        </> : <Text className="text-meta text-muted-foreground">{transferCopy.off}</Text>}
        {!delivery.canManage ? <IconLine icon={Crown} text={transferCopy.adminOnly} /> : null}
        {paid ? <>
          {status.paid_enabled || status.wallet.state !== 'absent' ? <Text className="text-meta text-muted-foreground">{walletNotice(status.wallet)}</Text> : null}
          {status.wallet.state === 'unavailable' ? <Text role="alert" className="text-meta text-warning">{status.wallet.reason}</Text> : null}
          <Details>
            {status.wallet.address ? <Info label="设备钱包" value={status.wallet.address} /> : null}
            {status.receiver_key ? <><Info label="当前接收密钥 ID" value={status.receiver_key.key_id} /><Info label="当前接收密钥序号" value={status.receiver_key.serial.toString()} /></> : null}
            <Text className="text-meta text-muted-foreground">{transferCopy.walletDetails}</Text>
          </Details>
        </> : null}
      </View>
      {status.enabled && delivery.canManage ? <View className="gap-4 rounded-xl border border-border bg-card p-4 shadow-xs dark:border-line-strong">
        <Text role="heading" className="text-heading font-semibold">发送文件</Text>
        {mine.length ? <View className="gap-2">
          <Text className="text-meta font-medium">{transferCopy.sendTo}</Text>
          <View className="overflow-hidden rounded-lg border border-border">{mine.map((item, index) => <Pressable key={item.device_id} accessibilityRole="radio" accessibilityState={{ checked: draft.receiver_peer === item.device_id, disabled: working }} disabled={working}
            className={cn('min-h-11 flex-row items-center gap-3 px-3 active:bg-muted', index > 0 && 'border-t border-border')} onPress={() => setDraft({ ...draft, receiver_peer: item.device_id })}>
            <Icon as={Laptop} className="size-4 text-muted-foreground" />
            <Text numberOfLines={1} className="min-w-0 flex-1">{item.name}</Text>
            {draft.receiver_peer === item.device_id ? <Icon as={Check} className="size-4 text-primary" /> : null}
          </Pressable>)}</View>
          <Details label={transferCopy.otherDevice} open={!picked && Boolean(draft.receiver_peer)}>{receiverId}</Details>
        </View> : receiverId}
        <InputField label={transferCopy.provider} hint={transferCopy.providerHint} value={draft.provider_peer} disabled={working} onChange={provider_peer => setDraft({ ...draft, provider_peer })} peer />
        {paid ? <Segmented options={modeOptions} value={draft.mode} disabled={working} onChange={mode => setDraft(changeMode(draft, mode))} /> : null}
        {sending.mode === 'paid' ? <>
          <Text className="text-meta text-muted-foreground">{transferCopy.protected}</Text>
          <Text className="text-meta font-medium">预付预算</Text>
          {availableBudgets.map(budget => <Pressable key={budget.budget_id} accessibilityRole="radio" accessibilityState={{ checked: draft.budget_id === budget.budget_id, disabled: working }} disabled={working}
            className="min-h-11 flex-row items-center gap-3 rounded-lg border border-border px-3" onPress={() => setDraft({ ...draft, budget_id: budget.budget_id, provider_peer: budget.provider_peer, max_gas_amount: budget.gas_cap })}>
            <Text className="min-w-0 flex-1">{budget.budget_id.slice(0, 12)} · 可分配 {budget.free}</Text>
            {draft.budget_id === budget.budget_id ? <Icon as={Check} className="size-4 text-primary" /> : null}
          </Pressable>)}
          {!availableBudgets.length ? <Text className="text-meta text-muted-foreground">先在发送设备本机建立预付预算，再刷新。</Text> : null}
          <InputField label={transferCopy.serviceCap} value={draft.max_service_amount} disabled={working} onChange={max_service_amount => setDraft({ ...draft, max_service_amount })} numeric />
          <Info label={transferCopy.gasCap} value={draft.max_gas_amount} />
        </> : <IconLine icon={ShieldCheck} tone="text-success" text={transferCopy.free} />}
        <View className="flex-row flex-wrap items-center gap-x-3 gap-y-2">
          <Button variant="outline" disabled={working} onPress={() => void picker.run('pick', async () => { const owner = selection.current; const picked = await mobilePlatform.files?.pick(); if (picked && owner.replace(picked)) setFile(picked) })}><Icon as={picker.busy === 'pick' ? LoaderCircle : File} /><Text>选择文件</Text></Button>
          <Text className="min-w-0 shrink text-meta text-muted-foreground">{file ? <><Text className="text-meta text-foreground">{file.name}</Text> · {sizeLabel(file.size)}</> : transferCopy.maxSize(status.max_file_bytes)}</Text>
        </View>
        <ErrorText error={picker.error} />
        {file && file.size > status.max_file_bytes ? <ErrorText error={transferCopy.tooLarge(status.max_file_bytes)} /> : null}
        {feedback('form')}
        <View className="flex-row flex-wrap gap-2">
          <Button disabled={unusable} onPress={() => prepare()}>{delivery.busy === 'prepare' ? <Icon as={LoaderCircle} /> : null}<Text>{transferCopy.prepare(sending.mode)}</Text></Button>
          {selected && canRestart(selected) && (paid || selected.mode === 'charity') ? <Button variant="outline" disabled={unusable} onPress={() => prepare(selected.intent_id)}><Text>作为新订单再次发送</Text></Button> : null}
        </View>
        <Details>
          <Text className="text-meta text-muted-foreground">{transferCopy.prepareDetails}</Text>
          {sending.mode === 'paid' ? <Text className="text-meta text-muted-foreground">{transferCopy.units}</Text> : null}
        </Details>
      </View> : null}
    </>}
    {paid && delivery.budgets.length ? <View className="gap-3 rounded-xl border border-border bg-card p-4">
      <Text role="heading" className="text-heading font-semibold">预付预算</Text>
      {delivery.budgets.map(budget => <View key={budget.budget_id} className="gap-2">
        <Info label="预算" value={budget.budget_id} />
        <Info label="可分配 / 已占用" value={`${budget.free} / ${budget.locked}`} />
        <Info label="已收到待结算的文件片段" value={budget.ready_windows.length.toString()} />
        <Info label="累计赚取 / 提款 / 退款" value={`${budget.earned} / ${budget.withdrawn} / ${budget.refunded}`} />
        {budget.payee.toLowerCase() === status?.wallet.address?.toLowerCase() ? <Button variant="outline" disabled={working || budget.ready_windows.length === 0 || !delivery.canManage || !status?.enabled || !status.paid_enabled || status.wallet.state !== 'unlocked'} onPress={() => { setOrigin('page'); void delivery.flushBudget(budget.budget_id) }}><Text>结算已就绪批次（最多两窗）</Text></Button> : null}
      </View>)}
      {canConfirmBatch ? <View className="gap-2">
        {preparedBatch.map(order => <Text key={order.order_id} className="text-meta">{order.name} · 服务费 {order.payment.total}</Text>)}
        <Button disabled={working || !delivery.canManage || !status?.enabled || !status.paid_enabled || status.wallet.state !== 'unlocked'} onPress={() => { setOrigin('page'); void delivery.confirmBatch(preparedBatch) }}><Text>确认这两笔并同批分配预算</Text></Button>
      </View> : null}
    </View> : null}
    <View className="gap-3">
      <View className="flex-row items-center justify-between gap-3">
        <Text role="heading" className="text-heading font-semibold">传输记录</Text>
        <Button variant="ghost" disabled={working} onPress={() => { setOrigin('page'); void delivery.refresh() }}><Icon as={delivery.busy === 'refresh' ? LoaderCircle : RefreshCw} /><Text>刷新</Text></Button>
      </View>
      {receivingBatches(delivery.orders).map(([budgetId, batch]) => <View key={budgetId} className="gap-2 rounded-xl border border-border bg-card p-4">
        {batch.map(order => <Text key={order.order_id} className="text-meta">{orderTitle(order)}</Text>)}
        <Button disabled={working || !delivery.canManage || !status?.enabled || !status.paid_enabled} onPress={() => { setOrigin('page'); void delivery.acceptBatch(batch) }}><Text>同时接收这两份文件</Text></Button>
      </View>)}
      {delivery.orders.length ? <View className="overflow-hidden rounded-xl border border-border bg-card shadow-xs dark:border-line-strong">{delivery.orders.map((order, index) => <Pressable key={order.order_id} accessibilityRole="button" accessibilityState={{ selected: selected?.order_id === order.order_id }}
        className={cn('min-h-16 flex-row items-center gap-3 px-4 py-3 active:bg-muted', index > 0 && 'border-t border-border', selected?.order_id === order.order_id && 'bg-muted/60')} onPress={() => delivery.select(order.order_id)}>
        <Icon as={roleIcons[order.role]} className="size-4 text-muted-foreground" />
        <View className="min-w-0 flex-1 gap-0.5">
          <Text numberOfLines={1} className="font-medium">{orderTitle(order)}</Text>
          <Text className="text-meta text-muted-foreground">{orderSummary(order)}</Text>
        </View>
      </Pressable>)}</View> : <EmptyState icon={ArrowRightLeft} title={transferCopy.noRecords} />}
    </View>
    {selected ? <OrderDetails key={selected.order_id} order={selected} busy={working} canManage={delivery.canManage} walletReady={status?.wallet.state === 'unlocked'} feedback={feedback('order')}
      confirm={() => { setOrigin('order'); void delivery.confirm(selected) }} accept={() => { setOrigin('order'); void delivery.accept(selected) }}
      reconcile={() => { setOrigin('order'); void delivery.reconcile(selected) }} save={() => { setOrigin('order'); void delivery.save(selected) }} /> : null}
  </KeyboardAwareScrollView>
}

function OrderDetails({ order, busy, canManage, walletReady, feedback, confirm, accept, reconcile, save }: { order: DeliveryOrder; busy: boolean; canManage: boolean; walletReady: boolean; feedback: ReactNode; confirm(): void; accept(): void; reconcile(): void; save(): void }) {
  const paid = order.mode === 'paid'
  const can = orderActions(order)
  return <View className="gap-4 rounded-xl border border-border bg-card p-4 shadow-xs dark:border-line-strong">
    <Text role="heading" className="text-heading font-semibold">{orderHeading(order)}</Text>
    <Text>{orderStatus(order)}</Text>
    {order.name ? <Info label="文件" value={`${order.name} · ${sizeLabel(order.bytes)}`} /> : null}
    {paid ? <>
      <Info label="传输方式" value={transferCopy.paid} />
      <Info label="预付预算" value={order.payment.budget_id ?? ''} />
      <Info label={transferCopy.serviceTotal} value={order.payment.total} />
      <Info label={transferCopy.gasCap} value={order.payment.gas_cap} />
    </> : <IconLine icon={ShieldCheck} tone="text-success" text={transferCopy.charity} />}
    <ErrorText error={order.error ?? ''} />
    {feedback}
    <View className="flex-row flex-wrap gap-2">
      {can.confirm ? <Button disabled={busy || !canManage || paid && !walletReady} onPress={confirm}><Text>{transferCopy.confirm(order.mode)}</Text></Button> : null}
      {can.accept ? <Button disabled={busy || !canManage} onPress={accept}><Text>接收文件</Text></Button> : null}
      {can.save ? <Button variant={order.role === 'receiver' ? 'default' : 'outline'} disabled={busy} onPress={save}><Icon as={ArrowDownToLine} /><Text>保存文件</Text></Button> : null}
      <Button variant="outline" disabled={busy || !canManage} onPress={reconcile}><Text>检查进度</Text></Button>
    </View>
    {paid && order.phase === 'prepared' && !walletReady ? <Text className="text-meta text-warning">{transferCopy.walletFirst}</Text> : null}
    {order.payment.state === 'unknown' || order.phase === 'reconciling' ? <Text className="text-meta text-warning">{transferCopy.unknownPayment}</Text> : null}
    <Details>
      <Text className="text-meta text-muted-foreground">{evidenceLabels[order.evidence]}。交付确认不表示真人已阅读。</Text>
      <Info label="订单" value={order.order_id} /><Info label="付款意图" value={order.intent_id} />
      <Info label="发送设备" value={order.source_peer} /><Info label="接收设备" value={order.receiver_peer} /><Info label="中转设备" value={order.provider_peer} />
      <Info label="验收条件" value={paid ? '完整密文持有证明与正确密钥包可取后结算' : '公益收据'} />
      {paid ? <>
        <Info label="测试链 ID" value={order.payment.chain_id ?? ''} /><Info label="测试资产合约" value={order.payment.asset ?? ''} /><Info label="付款钱包" value={order.payment.payer ?? ''} /><Info label="固定收款钱包" value={order.payment.provider_wallet ?? ''} />
        <Info label="传输费" value={order.payment.delivery_fee} /><Info label="暂存预留费" value={order.payment.reservation_fee} /><Info label="时效优先费" value={order.payment.priority_fee} />
        <Info label={settlementAmountsLabel(order, 'order')} value={`${order.payment.earned} / ${order.payment.refunded}`} />
        {order.payment.settlement ? <>
          <Info label={settlementAmountsLabel(order, 'budget')} value={`${order.payment.settlement.earned} / ${order.payment.settlement.withdrawn} / ${order.payment.settlement.refunded}`} />
          <Info label="批次状态" value={publicationLabel(order)} />
          {order.payment.settlement.publication ? <>
            <Info label="本批目标赚取金额" value={order.payment.settlement.publication.total_amount} />
            <Info label="本批完整金额截止" value={new Date(order.payment.settlement.publication.economic_cutoff * 1000).toLocaleString()} />
            {order.payment.settlement.publication.availability_risk ? <Text role="status" className="text-meta text-warning">{order.payment.settlement.publication.availability_risk}</Text> : null}
          </> : null}
        </> : null}
        {deadlineRows(order).map(([label, value]) => <Info key={label} label={label} value={value} />)}
        <Text className="text-meta text-muted-foreground">时效优先费只有在其截止前完成整单才赚取。过了时效截止，仍可在结算硬截止前完成交付。暂存费支付容量预留，不代表持续存储证明。</Text>
        {order.payment.tx_hash ? <Info label="原交易" value={order.payment.tx_hash} /> : null}
      </> : <Text className="text-meta text-muted-foreground">本订单免费，不调用钱包或链上付款。</Text>}
      {order.windows.map(window => <Text key={window.index} className="text-meta text-muted-foreground">窗口 {window.index + 1} · 密文已落盘 {window.committed_bytes} / {window.bytes + 68} 字节{window.decryption_proven ? ' · 已证明解密' : ''}</Text>)}
      {order.sha256 ? <Info label="原文件 SHA-256" value={order.sha256} /> : null}
    </Details>
  </View>
}

function Feedback({ error, notice }: { error: string; notice: string }) {
  if (error) return <ErrorText error={error} />
  if (notice) return <View role="status"><IconLine icon={CircleCheck} tone="text-success" text={notice} /></View>
  return null
}
function IconLine({ icon, text, tone }: { icon: typeof ShieldCheck; text: string; tone?: string }) {
  return <View className="flex-row items-center gap-1.5"><Icon as={icon} className={cn('size-4', tone ?? 'text-muted-foreground')} /><Text className={cn('shrink text-meta', tone ?? 'text-muted-foreground')}>{text}</Text></View>
}
function Info({ label, value }: { label: string; value: string }) { return <View className="gap-1"><Text className="text-meta text-muted-foreground">{label}</Text><Text selectable className="text-meta">{value}</Text></View> }
function Toggle({ label, icon, checked, disabled, change }: { label: string; icon?: LucideIcon; checked: boolean; disabled: boolean; change(value: boolean): void }) {
  return <View className="min-h-11 flex-row items-center justify-between gap-3">
    <View className="min-w-0 flex-1 flex-row items-center gap-1.5"><Text className="shrink text-meta">{label}</Text>{icon ? <Icon as={icon} className="size-4 text-muted-foreground" /> : null}</View>
    <Switch accessibilityLabel={label} value={checked} disabled={disabled} onValueChange={change} />
  </View>
}
function InputField({ label, hint, value, disabled, onChange, peer, numeric }: { label: string; hint?: string; value: string; disabled: boolean; onChange(value: string): void; peer?: boolean; numeric?: boolean }) {
  return <View className="gap-2">
    <Text className="text-meta font-medium">{label}</Text>
    <Input accessibilityLabel={label} value={value} editable={!disabled} onChangeText={onChange} autoCapitalize="none" autoCorrect={false} maxLength={peer ? 64 : 78} keyboardType={numeric ? 'number-pad' : 'default'} className="font-mono" />
    {hint ? <Text className="text-meta text-muted-foreground">{hint}</Text> : null}
  </View>
}

/** The 64-character ID stays unseen; this copies it to send to whoever sends a file. */
function CopyConnectionId({ value }: { value: string }) {
  const [copied, setCopied] = useState(false)
  return <Button variant="outline" className="self-start" onPress={() => { void Clipboard.setStringAsync(value).then(() => setCopied(true), () => {}) }}>
    <Icon as={copied ? Check : Copy} className={copied ? 'text-success' : undefined} /><Text>{copied ? '已复制' : '复制连接 ID'}</Text>
  </Button>
}
