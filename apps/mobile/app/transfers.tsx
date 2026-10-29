import { ArrowDownLeft, ArrowDownToLine, ArrowRightLeft, ArrowUpRight, CircleCheck, Crown, File, LoaderCircle, RefreshCw, ShieldCheck } from 'lucide-react-native'
import { useState, type ReactNode } from 'react'
import { Pressable, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useDelivery } from '@vibeface/client/delivery/client.ts'
import { acceptanceOptions, canRestart, changeMode, deadlineRows, evidenceLabels, modeOptions, orderActions, orderHeading, orderStatus, orderSummary, orderTitle, settlementAmountsLabel, sizeLabel, transferCopy, walletNotice } from '@vibeface/client/delivery/model.ts'
import type { DeliveryDraft, DeliveryOrder, DeliverySettings, TransferFile } from '@vibeface/client/delivery/types.ts'
import { ErrorText } from '@/components/action'
import { CopyableId, Details, EmptyState } from '@/components/kit'
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
  const [draft, setDraft] = useState<DeliveryDraft>({ receiver_peer: '', provider_peer: '', mode: 'charity', acceptance: 'recoverable', max_service_amount: '0', max_gas_amount: '0' })
  const [file, setFile] = useState<TransferFile | null>(null)
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
  const unusable = working || !file || status?.max_file_bytes === 0 || draft.mode === 'charity' && !status?.charity_enabled || draft.mode === 'paid' && !status?.paid_enabled
  const prepare = (restart?: string) => { if (file) { setOrigin('form'); void delivery.prepare(draft, file, restart) } }
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    {feedback('page')}
    {!status ? <Text role="status" className="text-meta text-muted-foreground">{transferCopy.loading}</Text> : <>
      <View className="gap-3 rounded-xl border border-border bg-card p-4 shadow-xs dark:border-line-strong">
        <Text role="heading" className="text-heading font-semibold">传输设置</Text>
        <Toggle label="启用文件传输" checked={status.enabled} disabled={!delivery.canManage || working} change={enabled => change({ enabled })} />
        {status.enabled ? <>
          <Toggle label="公益转发" checked={status.charity_enabled} disabled={!delivery.canManage || working} change={charity_enabled => change({ charity_enabled })} />
          <Toggle label="付费交付（本地测试）" checked={status.paid_enabled} disabled={!delivery.canManage || working} change={paid_enabled => change({ paid_enabled })} />
          <View className="gap-1"><Text className="text-meta text-muted-foreground">{transferCopy.connectionId}</Text><CopyableId value={status.node_peer} label="连接 ID" /></View>
        </> : <Text className="text-meta text-muted-foreground">{transferCopy.off}</Text>}
        {!delivery.canManage ? <IconLine icon={Crown} text={transferCopy.adminOnly} /> : null}
        {status.paid_enabled || status.wallet.state !== 'absent' ? <Text className="text-meta text-muted-foreground">{walletNotice(status.wallet)}</Text> : null}
        {status.wallet.state === 'unavailable' ? <Text role="alert" className="text-meta text-warning">{status.wallet.reason}</Text> : null}
        <Details>
          {status.wallet.address ? <Info label="设备钱包" value={status.wallet.address} /> : null}
          {status.receiver_key ? <><Info label="当前接收密钥 ID" value={status.receiver_key.key_id} /><Info label="当前接收密钥序号" value={status.receiver_key.serial.toString()} /></> : null}
          <Text className="text-meta text-muted-foreground">{transferCopy.walletDetails}</Text>
        </Details>
      </View>
      {status.enabled && delivery.canManage ? <View className="gap-4 rounded-xl border border-border bg-card p-4 shadow-xs dark:border-line-strong">
        <Text role="heading" className="text-heading font-semibold">发送文件</Text>
        <InputField label={transferCopy.receiver} value={draft.receiver_peer} disabled={working} onChange={receiver_peer => setDraft({ ...draft, receiver_peer })} peer />
        <InputField label={transferCopy.provider} hint={transferCopy.providerHint} value={draft.provider_peer} disabled={working} onChange={provider_peer => setDraft({ ...draft, provider_peer })} peer />
        <Segmented options={modeOptions} value={draft.mode} disabled={working} onChange={mode => setDraft(changeMode(draft, mode))} />
        {draft.mode === 'paid' ? <>
          <Text className="text-meta text-muted-foreground">什么时候付款</Text>
          <Segmented options={acceptanceOptions} value={draft.acceptance} disabled={working} onChange={acceptance => setDraft({ ...draft, acceptance })} />
          <InputField label="服务费上限（测试代币最小单位）" value={draft.max_service_amount} disabled={working} onChange={max_service_amount => setDraft({ ...draft, max_service_amount })} numeric />
          <InputField label="链费上限（测试链原生币最小单位）" value={draft.max_gas_amount} disabled={working} onChange={max_gas_amount => setDraft({ ...draft, max_gas_amount })} numeric />
        </> : <IconLine icon={ShieldCheck} tone="text-success" text={transferCopy.free} />}
        <View className="flex-row flex-wrap items-center gap-x-3 gap-y-2">
          <Button variant="outline" disabled={working} onPress={() => void picker.run('pick', async () => { const picked = await mobilePlatform.files?.pick(); if (picked) setFile(picked) })}><Icon as={picker.busy === 'pick' ? LoaderCircle : File} /><Text>选择文件</Text></Button>
          <Text className="min-w-0 shrink text-meta text-muted-foreground">{file ? <><Text className="text-meta text-foreground">{file.name}</Text> · {sizeLabel(file.size)}</> : transferCopy.maxSize(status.max_file_bytes)}</Text>
        </View>
        <ErrorText error={picker.error} />
        {file && file.size > status.max_file_bytes ? <ErrorText error={transferCopy.tooLarge(status.max_file_bytes)} /> : null}
        {feedback('form')}
        <View className="flex-row flex-wrap gap-2">
          <Button disabled={unusable} onPress={() => prepare()}>{delivery.busy === 'prepare' ? <Icon as={LoaderCircle} /> : null}<Text>{transferCopy.prepare(draft.mode)}</Text></Button>
          {selected && canRestart(selected) ? <Button variant="outline" disabled={unusable} onPress={() => prepare(selected.intent_id)}><Text>作为新订单再次发送</Text></Button> : null}
        </View>
        <Details><Text className="text-meta text-muted-foreground">{transferCopy.prepareDetails}</Text></Details>
      </View> : null}
    </>}
    <View className="gap-3">
      <View className="flex-row items-center justify-between gap-3">
        <Text role="heading" className="text-heading font-semibold">传输记录</Text>
        <Button variant="ghost" disabled={working} onPress={() => { setOrigin('page'); void delivery.refresh() }}><Icon as={delivery.busy === 'refresh' ? LoaderCircle : RefreshCw} /><Text>刷新</Text></Button>
      </View>
      {delivery.orders.length ? <View className="overflow-hidden rounded-xl border border-border bg-card shadow-xs dark:border-line-strong">{delivery.orders.map((order, index) => <Pressable key={order.order_id} accessibilityRole="button" accessibilityState={{ selected: selected?.order_id === order.order_id }}
        className={cn('min-h-16 flex-row items-center gap-3 px-4 py-3 active:bg-muted/60', index > 0 && 'border-t border-border', selected?.order_id === order.order_id && 'bg-muted/60')} onPress={() => delivery.select(order.order_id)}>
        <Icon as={roleIcons[order.role]} className="size-4 text-muted-foreground" />
        <View className="min-w-0 flex-1 gap-0.5">
          <Text numberOfLines={1} className="font-medium">{orderTitle(order)}</Text>
          <Text className="text-meta text-muted-foreground">{orderSummary(order)}</Text>
        </View>
      </Pressable>)}</View> : <Text className="text-meta text-muted-foreground">{transferCopy.noRecords}</Text>}
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
      <Info label="传输方式" value="付费交付" />
      <Info label="服务费合计（最小单位）" value={order.payment.total} />
      <Info label="链费上限（原生币最小单位）" value={order.payment.gas_cap} />
      {order.acceptance === 'strict' ? <Text className="text-meta text-warning">{transferCopy.strict}</Text> : null}
    </> : <IconLine icon={ShieldCheck} tone="text-success" text="公益转发 · 免费" />}
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
      <Info label="验收条件" value={paid ? order.acceptance === 'strict' ? '接收设备证明实际解密后结算' : '完整密文持有证明与正确密钥包可取后结算' : '公益收据'} />
      {paid ? <>
        <Info label="测试链 ID" value={order.payment.chain_id ?? ''} /><Info label="测试资产合约" value={order.payment.asset ?? ''} /><Info label="付款钱包" value={order.payment.payer ?? ''} /><Info label="固定收款钱包" value={order.payment.provider_wallet ?? ''} />
        <Info label="传输费" value={order.payment.delivery_fee} /><Info label="暂存预留费" value={order.payment.reservation_fee} /><Info label="时效优先费" value={order.payment.priority_fee} />
        <Info label={settlementAmountsLabel(order)} value={`${order.payment.earned} / ${order.payment.withdrawn} / ${order.payment.refunded}`} />
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
function Toggle({ label, checked, disabled, change }: { label: string; checked: boolean; disabled: boolean; change(value: boolean): void }) {
  return <View className="min-h-11 flex-row items-center justify-between gap-3"><Text className="min-w-0 flex-1 text-meta">{label}</Text><Switch accessibilityLabel={label} value={checked} disabled={disabled} onValueChange={change} /></View>
}
function InputField({ label, hint, value, disabled, onChange, peer, numeric }: { label: string; hint?: string; value: string; disabled: boolean; onChange(value: string): void; peer?: boolean; numeric?: boolean }) {
  return <View className="gap-2">
    <Text className="text-meta font-medium">{label}</Text>
    <Input accessibilityLabel={label} value={value} editable={!disabled} onChangeText={onChange} autoCapitalize="none" autoCorrect={false} maxLength={peer ? 64 : 78} keyboardType={numeric ? 'number-pad' : 'default'} className="font-mono" />
    {hint ? <Text className="text-meta text-muted-foreground">{hint}</Text> : null}
  </View>
}
