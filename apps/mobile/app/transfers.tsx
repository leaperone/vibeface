import { ArrowDownToLine, ArrowRightLeft, File, LoaderCircle, RefreshCw } from 'lucide-react-native'
import { useState } from 'react'
import { Switch, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useDelivery } from '@vibeface/client/delivery/client.ts'
import { canRead, canRestart, changeMode, deadlineRows, evidenceLabels, paymentLabels, phaseLabels, walletNotice } from '@vibeface/client/delivery/model.ts'
import type { Acceptance, DeliveryDraft, DeliveryMode, DeliveryOrder, DeliverySettings, TransferFile } from '@vibeface/client/delivery/types.ts'
import { ErrorText } from '@/components/action'
import { EmptyState } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Segmented } from '@/components/ui/segmented'
import { Text } from '@/components/ui/text'
import { useRun } from '@/lib/use-run'
import { mobilePlatform } from '@/platform'

const modes: Array<{ value: DeliveryMode; label: string }> = [{ value: 'charity', label: '公益转发' }, { value: 'paid', label: '付费交付' }]
const acceptances: Array<{ value: Acceptance; label: string }> = [{ value: 'recoverable', label: '确认可恢复' }, { value: 'strict', label: '证明实际解密' }]

export default function TransfersScreen() {
  const { ready } = useDevice()
  return ready ? <Transfers key={`${ready.generation}:${ready.entry.device_id}`} connection={ready} /> : <EmptyState icon={ArrowRightLeft} title="设备暂时离线，连接后可查询原订单" />
}

function Transfers({ connection }: { connection: NonNullable<ReturnType<typeof useDevice>['ready']> }) {
  const delivery = useDelivery(connection, mobilePlatform)
  const [draft, setDraft] = useState<DeliveryDraft>({ receiver_peer: '', provider_peer: '', mode: 'charity', acceptance: 'recoverable', max_service_amount: '0', max_gas_amount: '0' })
  const [file, setFile] = useState<TransferFile | null>(null)
  const picker = useRun()
  const status = delivery.status, selected = delivery.selected
  const working = delivery.busy !== null || picker.busy !== null
  const change = (patch: Partial<DeliverySettings>) => {
    if (status) void delivery.configure({ enabled: status.enabled, charity_enabled: status.charity_enabled, paid_enabled: status.paid_enabled, ...patch })
  }
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-2xl self-center gap-6 p-4 pb-10">
    <Text className="text-meta text-muted-foreground">查找设备和公益转发免费，付费交付仅供本地测试。</Text>
    <Button variant="outline" className="min-h-11 self-start" disabled={working} onPress={() => void delivery.refresh()}><Icon as={delivery.busy === 'refresh' ? LoaderCircle : RefreshCw} /><Text>刷新订单</Text></Button>
    <ErrorText error={delivery.error} />
    {delivery.notice ? <Text role="status" className="text-meta text-success">{delivery.notice}</Text> : null}
    {!status ? <Text role="status" className="text-meta text-muted-foreground">正在读取传输状态。</Text> : <>
      <View className="gap-4 rounded-xl border border-border bg-card p-4">
        <Text role="heading" className="text-heading font-semibold">此设备的传输设置</Text>
        <Info label="发送设备" value={status.node_peer} />
        <Toggle label="启用文件传输" checked={status.enabled} disabled={!delivery.canManage || working} change={enabled => change({ enabled })} />
        <Toggle label="公益转发" checked={status.charity_enabled} disabled={!delivery.canManage || working} change={charity_enabled => change({ charity_enabled })} />
        <Toggle label="付费交付（本地测试）" checked={status.paid_enabled} disabled={!delivery.canManage || working} change={paid_enabled => change({ paid_enabled })} />
        <Text className="text-meta text-muted-foreground">{walletNotice(status.wallet)}{!delivery.canManage ? ' 只有已加入的管理员能更改设置或确认订单。' : ''}</Text>
        {status.wallet.state === 'unavailable' ? <Text role="alert" className="text-meta text-warning">{status.wallet.reason}</Text> : null}
        {status.wallet.address ? <Info label="设备钱包" value={status.wallet.address} /> : null}
        <Text className="text-meta text-muted-foreground">钱包创建、解锁和备份通过本机命令行完成。个人恢复文件不包含钱包和订单解密密钥。</Text>
      </View>
      {status.enabled && delivery.canManage ? <View className="gap-4 rounded-xl border border-border bg-card p-4">
        <Text role="heading" className="text-heading font-semibold">发送文件</Text>
        <InputField label="接收设备的连接 ID" value={draft.receiver_peer} disabled={working} onChange={receiver_peer => setDraft({ ...draft, receiver_peer })} peer />
        <InputField label="服务设备的连接 ID" value={draft.provider_peer} disabled={working} onChange={provider_peer => setDraft({ ...draft, provider_peer })} peer />
        <Text className="text-meta text-muted-foreground">付款不会让服务设备加入你的设备列表。</Text>
        <Segmented options={modes} value={draft.mode} disabled={working} onChange={mode => setDraft(changeMode(draft, mode))} />
        {draft.mode === 'paid' ? <>
          <Text className="text-meta text-muted-foreground">结算条件</Text>
          <Segmented options={acceptances} value={draft.acceptance} disabled={working} onChange={acceptance => setDraft({ ...draft, acceptance })} />
          <InputField label="服务费上限（测试代币最小单位）" value={draft.max_service_amount} disabled={working} onChange={max_service_amount => setDraft({ ...draft, max_service_amount })} numeric />
          <InputField label="链费上限（测试链原生币最小单位）" value={draft.max_gas_amount} disabled={working} onChange={max_gas_amount => setDraft({ ...draft, max_gas_amount })} numeric />
        </> : <Text className="text-meta text-muted-foreground">文件全程加密，公益收据仅确认接收，不产生链上证明或付款。</Text>}
        <Button variant="outline" className="min-h-11 self-start" disabled={working} onPress={() => void picker.run('pick', async () => { const picked = await mobilePlatform.files?.pick(); if (picked) setFile(picked) })}><Icon as={picker.busy === 'pick' ? LoaderCircle : File} /><Text>选择文件</Text></Button>
        <Text className="text-meta text-muted-foreground">{file ? `${file.name} · ${file.size} 字节` : '尚未选择文件'}。当前最多 {status.max_file_bytes} 字节。</Text>
        <ErrorText error={picker.error} />
        <Button className="min-h-11 self-start" disabled={working || !file || status.max_file_bytes === 0 || draft.mode === 'charity' && !status.charity_enabled || draft.mode === 'paid' && !status.paid_enabled} onPress={() => { if (file) void delivery.prepare(draft, file) }}>
          {delivery.busy === 'prepare' ? <Icon as={LoaderCircle} /> : null}<Text>准备订单并查看费用</Text>
        </Button>
        {selected && canRestart(selected) ? <Button variant="outline" className="min-h-11 self-start" disabled={working || !file || status.max_file_bytes === 0 || draft.mode === 'charity' && !status.charity_enabled || draft.mode === 'paid' && !status.paid_enabled} onPress={() => { if (file) void delivery.prepare(draft, file, selected.intent_id) }}><Text>作为新订单再次发送</Text></Button> : null}
        <Text className="text-meta text-muted-foreground">准备订单会把文件交给当前连接的可信设备加密，尚不付款。同一文件和条款复用原意图。原订单确认结束后，选择相同文件和条款，点击“作为新订单再次发送”才能创建另一笔订单。</Text>
      </View> : !status.enabled ? <Text className="text-meta text-muted-foreground">文件传输默认关闭。关闭时仍可免费查找设备，并查询已有订单。</Text> : null}
    </>}
    <View className="gap-3">
      <Text role="heading" className="text-heading font-semibold">订单</Text>
      {delivery.orders.length ? delivery.orders.map(order => <Button key={order.order_id} variant="outline" className="min-h-16 flex-col items-start gap-1 p-3" accessibilityState={{ selected: selected?.order_id === order.order_id }} onPress={() => delivery.select(order.order_id)}>
        <Text className="font-medium">{order.name ?? `密文订单 ${order.order_id.slice(0, 12)}`}</Text>
        <Text className="text-meta text-muted-foreground">{order.mode === 'charity' ? '公益' : '付费'} · {order.bytes} 字节 · {phaseLabels[order.phase]}</Text>
        <Text className="text-meta text-muted-foreground">{evidenceLabels[order.evidence]} · {paymentLabels[order.payment.state]}</Text>
      </Button>) : <Text className="text-meta text-muted-foreground">还没有传输订单。</Text>}
    </View>
    {selected ? <OrderDetails order={selected} busy={working} canManage={delivery.canManage} walletReady={status?.wallet.state === 'unlocked'} confirm={() => void delivery.confirm(selected)} accept={() => void delivery.accept(selected)} reconcile={() => void delivery.reconcile(selected)} save={() => void delivery.save(selected)} /> : null}
  </KeyboardAwareScrollView>
}

function OrderDetails({ order, busy, canManage, walletReady, confirm, accept, reconcile, save }: { order: DeliveryOrder; busy: boolean; canManage: boolean; walletReady: boolean; confirm(): void; accept(): void; reconcile(): void; save(): void }) {
  const paid = order.mode === 'paid'
  return <View className="gap-4 rounded-xl border border-border bg-card p-4">
    <Text role="heading" className="text-heading font-semibold">{order.phase === 'prepared' ? '确认这笔订单' : '订单详情'}</Text>
    <Text>{phaseLabels[order.phase]} · {paymentLabels[order.payment.state]}</Text>
    <Text className="text-meta text-muted-foreground">{evidenceLabels[order.evidence]}。交付确认不表示真人已阅读。</Text>
    <Info label="订单" value={order.order_id} /><Info label="付款意图" value={order.intent_id} />
    {order.name ? <Info label="文件" value={`${order.name} · ${order.bytes} 字节`} /> : null}
    <Info label="发送设备" value={order.source_peer} /><Info label="接收设备" value={order.receiver_peer} /><Info label="服务设备" value={order.provider_peer} />
    <Info label="验收条件" value={paid ? order.acceptance === 'strict' ? '接收设备 证明实际解密后结算' : '完整密文持有证明与正确密钥包可取后结算' : '公益收据'} />
    {paid ? <>
      <Info label="测试链 ID" value={order.payment.chain_id ?? ''} /><Info label="测试资产合约" value={order.payment.asset ?? ''} /><Info label="付款钱包" value={order.payment.payer ?? ''} /><Info label="固定收款钱包" value={order.payment.provider_wallet ?? ''} />
      <View className="gap-3 border-y border-border py-3">
        <Info label="传输费" value={order.payment.delivery_fee} /><Info label="暂存预留费" value={order.payment.reservation_fee} /><Info label="时效优先费" value={order.payment.priority_fee} /><Info label="服务费合计（最小单位）" value={order.payment.total} /><Info label="链费上限（原生币最小单位）" value={order.payment.gas_cap} />
        <Info label="已赚取 / 已提款 / 已退款" value={`${order.payment.earned} / ${order.payment.withdrawn} / ${order.payment.refunded}`} />
      </View>
      {deadlineRows(order).map(([label, value]) => <Info key={label} label={label} value={value} />)}
      <Text className="text-meta text-muted-foreground">时效优先费只有在其截止前完成整单才赚取。过了时效截止，仍可在结算硬截止前完成交付。暂存费支付容量预留，不代表持续存储证明。</Text>
      {order.payment.tx_hash ? <Info label="原交易" value={order.payment.tx_hash} /> : null}
      {order.acceptance === 'strict' ? <Text className="text-meta text-warning">严格验收需要接收设备 提交解密证明。对方离线或未提交时，服务商可能收不到费用。</Text> : null}
    </> : <Text className="text-meta text-muted-foreground">本订单免费，不调用钱包或链上付款。</Text>}
    {order.windows.map(window => <Text key={window.index} className="text-meta text-muted-foreground">窗口 {window.index + 1} · 密文已落盘 {window.committed_bytes} / {window.bytes + 68} 字节{window.decryption_proven ? ' · 已证明解密' : ''}</Text>)}
    {order.sha256 ? <Info label="原文件 SHA-256" value={order.sha256} /> : null}
    <ErrorText error={order.error ?? ''} />
    <View className="flex-row flex-wrap gap-2">
      {order.role === 'sender' && order.phase === 'prepared' ? <Button className="min-h-11" disabled={busy || !canManage || paid && !walletReady} onPress={confirm}><Text>{paid ? '确认费用并提交原订单' : '确认公益传输'}</Text></Button> : null}
      {order.role === 'receiver' && !canRead(order) && order.phase !== 'refunded' && order.phase !== 'failed' ? <Button className="min-h-11" disabled={busy || !canManage} onPress={accept}><Text>接受此订单</Text></Button> : null}
      <Button variant="outline" className="min-h-11" disabled={busy || !canManage} onPress={reconcile}><Text>继续与对账</Text></Button>
      {canRead(order) ? <Button variant="outline" className="min-h-11" disabled={busy} onPress={save}><Icon as={ArrowDownToLine} /><Text>取回文件</Text></Button> : null}
    </View>
    {paid && order.phase === 'prepared' && !walletReady ? <Text className="text-meta text-warning">请先在发送设备 本机创建或解锁钱包，再刷新此订单。</Text> : null}
    {order.payment.state === 'unknown' || order.phase === 'reconciling' ? <Text className="text-meta text-warning">原交易结果尚未确认。继续与对账会查询原意图，不会创建另一笔付款。</Text> : null}
  </View>
}

function Info({ label, value }: { label: string; value: string }) { return <View className="gap-1"><Text className="text-meta text-muted-foreground">{label}</Text><Text selectable className="text-meta">{value}</Text></View> }
function Toggle({ label, checked, disabled, change }: { label: string; checked: boolean; disabled: boolean; change(value: boolean): void }) {
  return <View className="min-h-11 flex-row items-center justify-between gap-3"><Text className="min-w-0 flex-1 text-meta">{label}</Text><Switch accessibilityLabel={label} value={checked} disabled={disabled} onValueChange={change} /></View>
}
function InputField({ label, value, disabled, onChange, peer, numeric }: { label: string; value: string; disabled: boolean; onChange(value: string): void; peer?: boolean; numeric?: boolean }) {
  return <View className="gap-2"><Text className="text-meta font-medium">{label}</Text><Input accessibilityLabel={label} value={value} editable={!disabled} onChangeText={onChange} autoCapitalize="none" autoCorrect={false} maxLength={peer ? 64 : 78} keyboardType={numeric ? 'number-pad' : 'default'} className="min-h-11 font-mono" /></View>
}
